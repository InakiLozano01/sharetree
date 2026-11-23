#![cfg_attr(not(windows), allow(dead_code))]

pub const MAX_REGION: u64 = (4 << 30) - 1;

pub fn clone_regions(len: u64, cluster: u64) -> Vec<(u64, u64)> {
    assert!(cluster.is_power_of_two(), "cluster size {cluster}");
    let total = len.div_ceil(cluster) * cluster;
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

    const FILE_SUPPORTS_BLOCK_REFCOUNTING: u32 = 0x0800_0000;

    fn wide(p: &Path) -> Vec<u16> {
        p.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    fn handle(f: &File) -> HANDLE {
        f.as_raw_handle() as HANDLE
    }

    pub fn volume_can_clone(f: &File) -> io::Result<bool> {
        let mut flags = 0u32;
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
        if unsafe { GetVolumePathNameW(wide(path).as_ptr(), root.as_mut_ptr(), root.len() as u32) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(root)
    }

    pub fn volume_serial(path: &Path) -> io::Result<u64> {
        let root = volume_root(path)?;
        let mut serial = 0u32;
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

    pub fn cluster_size(path: &Path) -> io::Result<u64> {
        let root = volume_root(path)?;
        let (mut spc, mut bps, mut free, mut total) = (0u32, 0u32, 0u32, 0u32);
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

    pub fn clone_file(src: &File, src_attrs: u32, dst: &Path, len: u64) -> io::Result<File> {
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
