//! Windows block cloning (ReFS / Dev Drive) with
//! `FSCTL_DUPLICATE_EXTENTS_TO_FILE`. **Off by default and untested**: it
//! runs only with `sharetree add --windows-clone` or `windows_clone = true`
//! in the config, and no Windows machine has run it yet.
//!
//! Rules from Microsoft's block-cloning documentation and the research note
//! (§2.3), each mapped to code below:
//!
//! * the volume must report `FILE_SUPPORTS_BLOCK_REFCOUNTING` (a Dev Drive
//!   before Windows 11 24H2 is ReFS but cannot clone): `volume_can_clone`;
//! * the source is flushed with `FlushFileBuffers` first: a clone taken while
//!   source data is still in the write cache produced permanently
//!   zero-filled files (git-lfs #6312), and a worktree tool clones exactly
//!   such freshly written files;
//! * the destination's integrity-stream setting must equal the source's, and
//!   a sparse source needs a sparse destination: `match_attributes`;
//! * the destination is extended with `SetEndOfFile` before cloning;
//! * regions start and end on cluster boundaries and each is < 4 GiB:
//!   `clone_regions` (portable, unit-tested). The last region is rounded up
//!   past the source's end of file to a cluster boundary, as Microsoft's
//!   sample does; the destination keeps its exact length.
//!
//! Every clone is still hashed against the target blob before it enters the
//! index (`index.rs`), so a wrong clone costs a rewrite by git, never a wrong
//! file in the worktree.

// The portable helpers are used only by the Windows engine.
#![cfg_attr(not(windows), allow(dead_code))]

/// Microsoft: "each region must be less than 4 GiB".
pub const MAX_REGION: u64 = (4 << 30) - 1;

/// Split a clone of `len` bytes into `(offset, byte_count)` regions that
/// start on cluster boundaries, have cluster-multiple lengths, and are each
/// below 4 GiB. The total covers `len` rounded up to whole clusters.
pub fn clone_regions(len: u64, cluster: u64) -> Vec<(u64, u64)> {
    assert!(cluster.is_power_of_two(), "cluster size {cluster}");
    let total = len.div_ceil(cluster) * cluster;
    // Largest cluster multiple below 4 GiB.
    let step = MAX_REGION / cluster * cluster;
    let mut v = Vec::new();
    let mut off = 0;
    while off < total {
        let n = (total - off).min(step);
        v.push((off, n));
        off += n;
    }
    v
}

pub const NOTE: &str = "Windows block cloning (FSCTL_DUPLICATE_EXTENTS_TO_FILE) is \
EXPERIMENTAL and has never been run on Windows by its authors. Each source is \
flushed first (git-lfs #6312) and every clone is hashed against its blob before \
git is told it is up to date; anything that fails is written by git.";

#[cfg(windows)]
pub use sys::*;

#[cfg(windows)]
mod sys {
    use std::ffi::c_void;
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;

    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE};
    use windows_sys::Win32::Storage::FileSystem::{
        FileEndOfFileInfo, FlushFileBuffers, GetDiskFreeSpaceW, GetVolumeInformationByHandleW,
        GetVolumeInformationW, GetVolumePathNameW, SetFileInformationByHandle,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_SPARSE_FILE, FILE_END_OF_FILE_INFO,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
    };
    use windows_sys::Win32::System::Ioctl::{
        DUPLICATE_EXTENTS_DATA, FSCTL_DUPLICATE_EXTENTS_TO_FILE, FSCTL_GET_INTEGRITY_INFORMATION,
        FSCTL_GET_INTEGRITY_INFORMATION_BUFFER, FSCTL_SET_INTEGRITY_INFORMATION,
        FSCTL_SET_INTEGRITY_INFORMATION_BUFFER, FSCTL_SET_SPARSE,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    use super::clone_regions;

    /// winnt.h (windows-sys has it under System_SystemServices).
    const FILE_SUPPORTS_BLOCK_REFCOUNTING: u32 = 0x0800_0000;

    fn wide(p: &Path) -> Vec<u16> {
        p.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    fn handle(f: &File) -> HANDLE {
        f.as_raw_handle() as HANDLE
    }

    /// Does the volume holding `f` support block cloning?
    pub fn volume_can_clone(f: &File) -> io::Result<bool> {
        let mut flags = 0u32;
        // SAFETY: valid handle; null buffers with zero sizes are allowed.
        let ok = unsafe {
            GetVolumeInformationByHandleW(
                handle(f),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut flags,
                std::ptr::null_mut(),
                0,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(flags & FILE_SUPPORTS_BLOCK_REFCOUNTING != 0)
    }

    fn volume_root(path: &Path) -> io::Result<Vec<u16>> {
        let mut root = vec![0u16; 1024];
        // SAFETY: NUL-terminated input, output buffer of the stated length.
        if unsafe { GetVolumePathNameW(wide(path).as_ptr(), root.as_mut_ptr(), root.len() as u32) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(root)
    }

    /// Serial number of the volume holding `path` (a reflink cannot cross
    /// volumes).
    pub fn volume_serial(path: &Path) -> io::Result<u64> {
        let root = volume_root(path)?;
        let mut serial = 0u32;
        // SAFETY: NUL-terminated root, out-pointer to a local, null buffers
        // with zero sizes.
        let ok = unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                std::ptr::null_mut(),
                0,
                &mut serial,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(serial as u64)
    }

    /// Cluster size of the volume holding `path`.
    pub fn cluster_size(path: &Path) -> io::Result<u64> {
        let root = volume_root(path)?;
        let (mut spc, mut bps, mut free, mut total) = (0u32, 0u32, 0u32, 0u32);
        // SAFETY: out-pointers to locals.
        if unsafe { GetDiskFreeSpaceW(root.as_ptr(), &mut spc, &mut bps, &mut free, &mut total) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        let c = spc as u64 * bps as u64;
        if !c.is_power_of_two() {
            return Err(io::Error::other(format!("odd cluster size {c}")));
        }
        Ok(c)
    }

    unsafe fn ioctl(
        h: HANDLE,
        code: u32,
        input: *const c_void,
        input_size: u32,
        output: *mut c_void,
        output_size: u32,
    ) -> io::Result<u32> {
        let mut returned = 0u32;
        // SAFETY: buffers are valid for their lengths; synchronous handle.
        let ok = unsafe {
            DeviceIoControl(
                h,
                code,
                input,
                input_size,
                output,
                output_size,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(returned)
    }

    /// Give `dst` the source's integrity-stream and sparse settings.
    fn match_attributes(src: &File, dst: &File, src_attrs: u32) -> io::Result<()> {
        let mut info = FSCTL_GET_INTEGRITY_INFORMATION_BUFFER::default();
        let info_size = std::mem::size_of_val(&info) as u32;
        let returned = unsafe {
            ioctl(
                handle(src),
                FSCTL_GET_INTEGRITY_INFORMATION,
                std::ptr::null(),
                0,
                std::ptr::from_mut(&mut info).cast(),
                info_size,
            )?
        };
        if returned < info_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "incomplete ReFS integrity information",
            ));
        }
        let set = FSCTL_SET_INTEGRITY_INFORMATION_BUFFER {
            ChecksumAlgorithm: info.ChecksumAlgorithm,
            Reserved: 0,
            Flags: info.Flags,
        };
        unsafe {
            ioctl(
                handle(dst),
                FSCTL_SET_INTEGRITY_INFORMATION,
                std::ptr::from_ref(&set).cast(),
                std::mem::size_of_val(&set) as u32,
                std::ptr::null_mut(),
                0,
            )?;
        }
        if src_attrs & FILE_ATTRIBUTE_SPARSE_FILE != 0 {
            unsafe {
                ioctl(
                    handle(dst),
                    FSCTL_SET_SPARSE,
                    std::ptr::null(),
                    0,
                    std::ptr::null_mut(),
                    0,
                )?;
            }
        }
        Ok(())
    }

    fn set_len(f: &File, len: u64) -> io::Result<()> {
        let info = FILE_END_OF_FILE_INFO {
            EndOfFile: len as i64,
        };
        // SAFETY: documented struct for FileEndOfFileInfo.
        let ok = unsafe {
            SetFileInformationByHandle(
                handle(f),
                FileEndOfFileInfo,
                &info as *const _ as *const c_void,
                std::mem::size_of::<FILE_END_OF_FILE_INFO>() as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Open a donor for cloning: not following reparse points, and denying
    /// other writers while it is open (share mode read only). Write access
    /// is requested only because `FlushFileBuffers` requires it; nothing is
    /// written.
    pub fn open_source(src: &Path) -> io::Result<File> {
        OpenOptions::new()
            .access_mode(GENERIC_READ | GENERIC_WRITE)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(src)
    }

    pub fn is_plain_file(m: &std::fs::Metadata) -> bool {
        m.is_file() && m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
    }

    /// Flush `src`, create `dst` (new), match its attributes, size it, and
    /// clone `len` bytes region by region. On error `dst` is removed.
    pub fn clone_file(src: &File, src_attrs: u32, dst: &Path, len: u64) -> io::Result<File> {
        // SAFETY: valid handle opened with write access.
        if unsafe { FlushFileBuffers(handle(src)) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let d = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(dst)?;
        let res = (|| -> io::Result<()> {
            match_attributes(src, &d, src_attrs)?;
            set_len(&d, len)?;
            if len == 0 {
                return Ok(());
            }
            let cluster = cluster_size(dst)?;
            for (off, n) in clone_regions(len, cluster) {
                let data = DUPLICATE_EXTENTS_DATA {
                    FileHandle: handle(src),
                    SourceFileOffset: off as i64,
                    TargetFileOffset: off as i64,
                    ByteCount: n as i64,
                };
                unsafe {
                    ioctl(
                        handle(&d),
                        FSCTL_DUPLICATE_EXTENTS_TO_FILE,
                        std::ptr::from_ref(&data).cast(),
                        std::mem::size_of_val(&data) as u32,
                        std::ptr::null_mut(),
                        0,
                    )?;
                }
            }
            Ok(())
        })();
        match res {
            Ok(()) => Ok(d),
            Err(e) => {
                drop(d);
                let _ = std::fs::remove_file(dst);
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const K4: u64 = 4096;
    const G: u64 = 1 << 30;

    #[test]
    fn regions_are_aligned_and_below_4_gib() {
        assert!(clone_regions(0, K4).is_empty());
        assert_eq!(
            clone_regions(1, K4),
            vec![(0, K4)],
            "rounded up to a cluster"
        );
        assert_eq!(clone_regions(K4, K4), vec![(0, K4)]);
        assert_eq!(clone_regions(K4 + 1, K4), vec![(0, 2 * K4)]);
        for (len, cluster) in [
            (10 * G + 123, K4),
            (4 * G, K4),
            (9 * G, 65536),
            (4 * G - 1, K4),
        ] {
            let r = clone_regions(len, cluster);
            let mut expect = 0;
            for &(off, n) in &r {
                assert_eq!(off, expect, "contiguous");
                assert_eq!(off % cluster, 0);
                assert_eq!(n % cluster, 0);
                assert!(n < 4 * G, "{n}");
                expect += n;
            }
            assert_eq!(expect, len.div_ceil(cluster) * cluster);
            assert!(expect - len < cluster);
        }
        assert_eq!(
            clone_regions(4 * G, K4).len(),
            2,
            "exactly 4 GiB needs two regions"
        );
    }
}
