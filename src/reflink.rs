//! Per-file copy-on-write clone primitives.
//!
//! Linux: `ioctl(FICLONE)` on a freshly created destination.
//! macOS: `fclonefileat(CLONE_NOFOLLOW)`, then normalize the metadata APFS
//! copied (BSD flags, xattrs, mode), as git-twig does.
//! Never a byte copy, never a hardlink: callers fall back to git itself.
//!
//! The errno classification follows lukeed/lane `cow.rs` (MIT), plus EAGAIN
//! (OpenZFS may refuse to clone dirty data).

use std::fs::{self, File, Metadata, OpenOptions};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

#[derive(Debug)]
pub enum CloneError {
    /// The filesystem (or this file pair) cannot share extents.
    Unsupported(io::Error),
    /// The donor changed while we cloned it, or is not what the plan expected.
    SourceChanged,
    /// Any other failure (missing donor, permissions, ENOSPC...).
    Io(io::Error),
}

impl std::fmt::Display for CloneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CloneError::Unsupported(e) => write!(f, "reflink unsupported: {e}"),
            CloneError::SourceChanged => write!(f, "source changed during clone"),
            CloneError::Io(e) => write!(f, "{e}"),
        }
    }
}

pub fn is_unsupported_errno(code: i32) -> bool {
    [
        libc::ENOTSUP,
        libc::EOPNOTSUPP,
        libc::ENOTTY,
        libc::EXDEV,
        libc::EINVAL,
        libc::EPERM,
        libc::ENOSYS,
        libc::EAGAIN,
    ]
    .contains(&code)
}

fn classify(e: io::Error) -> CloneError {
    match e.raw_os_error() {
        Some(code) if is_unsupported_errno(code) => CloneError::Unsupported(e),
        _ => CloneError::Io(e),
    }
}

/// Open a donor file without following a final symlink and without blocking
/// on a FIFO that replaced it.
fn open_source(src: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(src)
}

/// Open a file for reading without following a final symlink (used to hash
/// a fresh clone).
pub fn open_no_follow(p: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(p)
}

pub fn is_plain_file(m: &Metadata) -> bool {
    m.is_file()
}

/// What must not change between "decide to clone" and "clone done".
#[derive(PartialEq, Eq)]
struct Stamp(u64, i64, i64, i64, i64, u64, u64);

fn stamp(m: &Metadata) -> Stamp {
    Stamp(
        m.len(),
        m.mtime(),
        m.mtime_nsec(),
        m.ctime(),
        m.ctime_nsec(),
        m.ino(),
        m.dev(),
    )
}

/// Clone the open `src` into a new file `dst` created with `create_mode`
/// (subject to umask and default ACLs, exactly like git's own open()).
#[cfg(target_os = "linux")]
fn clone_into_new(src: &File, dst: &Path, create_mode: u32) -> Result<File, CloneError> {
    use std::os::fd::AsRawFd;
    // _IOW(0x94, 9, int), include/uapi/linux/fs.h
    const FICLONE: u64 = 0x4004_9409;
    let d = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(create_mode)
        .custom_flags(libc::O_CLOEXEC)
        .open(dst)
        .map_err(CloneError::Io)?;
    // SAFETY: both descriptors are open for the duration of the call.
    let rc = unsafe { libc::ioctl(d.as_raw_fd(), FICLONE as _, src.as_raw_fd()) };
    if rc != 0 {
        let e = io::Error::last_os_error();
        drop(d);
        let _ = fs::remove_file(dst);
        return Err(classify(e));
    }
    Ok(d)
}

#[cfg(target_os = "macos")]
fn clone_into_new(src: &File, dst: &Path, create_mode: u32) -> Result<File, CloneError> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    // sys/clonefile.h; not exported by the libc crate.
    const CLONE_NOFOLLOW: u32 = 0x0001;
    const CLONE_NOOWNERCOPY: u32 = 0x0002;
    extern "C" {
        fn fclonefileat(
            srcfd: libc::c_int,
            dst_dirfd: libc::c_int,
            dst: *const libc::c_char,
            flags: u32,
        ) -> libc::c_int;
    }
    let c = CString::new(dst.as_os_str().as_bytes())
        .map_err(|e| CloneError::Io(io::Error::new(io::ErrorKind::InvalidInput, e)))?;
    // SAFETY: valid fd and NUL-terminated path.
    let rc = unsafe {
        fclonefileat(
            src.as_raw_fd(),
            libc::AT_FDCWD,
            c.as_ptr(),
            CLONE_NOFOLLOW | CLONE_NOOWNERCOPY,
        )
    };
    if rc != 0 {
        return Err(classify(io::Error::last_os_error()));
    }
    // A git checkout has neither BSD flags nor xattrs (quarantine etc.).
    unsafe {
        libc::lchflags(c.as_ptr(), 0);
    }
    strip_xattrs(&c);
    let mode = create_mode & !crate::util::umask() & 0o7777;
    if let Err(e) = fs::set_permissions(dst, fs::Permissions::from_mode(mode)) {
        let _ = fs::remove_file(dst);
        return Err(CloneError::Io(e));
    }
    OpenOptions::new()
        .read(true)
        .open(dst)
        .map_err(CloneError::Io)
}

#[cfg(target_os = "macos")]
fn strip_xattrs(path: &std::ffi::CStr) {
    const XATTR_NOFOLLOW: libc::c_int = 0x0001;
    let mut buf = vec![0u8; 4096];
    // SAFETY: buffer sized as passed.
    let n = unsafe {
        libc::listxattr(
            path.as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            XATTR_NOFOLLOW,
        )
    };
    if n <= 0 {
        return;
    }
    for name in buf[..n as usize]
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
    {
        if let Ok(cname) = std::ffi::CString::new(name) {
            unsafe {
                libc::removexattr(path.as_ptr(), cname.as_ptr(), XATTR_NOFOLLOW);
            }
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn clone_into_new(_src: &File, _dst: &Path, _create_mode: u32) -> Result<File, CloneError> {
    Err(CloneError::Unsupported(io::Error::from_raw_os_error(
        libc::ENOTSUP,
    )))
}

/// Clone a tracked file for the new worktree. The donor must be a regular
/// file of `want_size` bytes, and must not change while it is cloned (the
/// donor descriptor is re-`fstat`ed afterwards, an idea from huntharo/git's
/// reflink-checkout design). Content is verified separately by hashing.
pub fn clone_tracked(
    src: &Path,
    dst: &Path,
    create_mode: u32,
    want_size: u64,
) -> Result<(), CloneError> {
    let s = open_source(src).map_err(CloneError::Io)?;
    let before = s.metadata().map_err(CloneError::Io)?;
    if !before.is_file() || before.len() != want_size {
        return Err(CloneError::SourceChanged);
    }
    let d = clone_into_new(&s, dst, create_mode)?;
    drop(d);
    let after = s.metadata().map_err(CloneError::Io)?;
    if stamp(&before) != stamp(&after) {
        let _ = fs::remove_file(dst);
        return Err(CloneError::SourceChanged);
    }
    Ok(())
}

/// Clone an ignored cache file, keeping its permission bits and timestamps
/// (build tools compare mtimes). Returns the file size.
pub fn clone_preserving(src: &Path, dst: &Path) -> Result<u64, CloneError> {
    let s = open_source(src).map_err(CloneError::Io)?;
    let m = s.metadata().map_err(CloneError::Io)?;
    if !m.is_file() {
        return Err(CloneError::SourceChanged);
    }
    let d = clone_into_new(&s, dst, 0o600)?;
    let res = (|| -> io::Result<()> {
        d.set_permissions(fs::Permissions::from_mode(m.permissions().mode() & 0o7777))?;
        let times = fs::FileTimes::new()
            .set_accessed(m.accessed()?)
            .set_modified(m.modified()?);
        d.set_times(times)
    })();
    if let Err(e) = res {
        drop(d);
        let _ = fs::remove_file(dst);
        return Err(CloneError::Io(e));
    }
    Ok(m.len())
}

/// Prove that `dir` can reflink by cloning a 4 KiB file inside it. A
/// filesystem-name allowlist would be wrong for overlayfs, XFS reflink=0,
/// ZFS without block cloning, etc. (git-twig README; lane `probe`).
pub fn probe(dir: &Path) -> Result<(), String> {
    let tag = format!(
        ".sharetree-probe-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos())
    );
    let a = dir.join(format!("{tag}.a"));
    let b = dir.join(format!("{tag}.b"));
    let res = (|| -> Result<(), String> {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&a)
            .map_err(|e| format!("cannot create a probe file in {}: {e}", dir.display()))?;
        io::Write::write_all(&mut f, &[0x5a; 4096]).map_err(|e| e.to_string())?;
        let s = open_source(&a).map_err(|e| e.to_string())?;
        match clone_into_new(&s, &b, 0o600) {
            Ok(_) => Ok(()),
            // OpenZFS may refuse to clone dirty blocks; retry once synced.
            // (Not up front: an fsync on Btrfs can wait on a whole
            // transaction commit, seconds after a large delete.)
            Err(CloneError::Unsupported(e)) if e.raw_os_error() == Some(libc::EAGAIN) => {
                f.sync_all().map_err(|e| e.to_string())?;
                clone_into_new(&s, &b, 0o600)
                    .map(drop)
                    .map_err(|e| e.to_string())
            }
            Err(e) => Err(e.to_string()),
        }
    })();
    let _ = fs::remove_file(&a);
    let _ = fs::remove_file(&b);
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_errnos_fall_back_but_enospc_does_not() {
        for code in [
            libc::EOPNOTSUPP,
            libc::EXDEV,
            libc::EINVAL,
            libc::ENOTTY,
            libc::EAGAIN,
        ] {
            assert!(matches!(
                classify(io::Error::from_raw_os_error(code)),
                CloneError::Unsupported(_)
            ));
        }
        assert!(matches!(
            classify(io::Error::from_raw_os_error(libc::ENOSPC)),
            CloneError::Io(_)
        ));
    }
}
