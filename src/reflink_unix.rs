use std::fs::{self, File, Metadata, OpenOptions};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use super::{CloneError, PendingFile};

pub(super) const CLONE_NAME: &str = "reflink";
pub(super) const PROBE_LEN: usize = 4096;

fn is_unsupported_errno(code: i32) -> bool {
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

pub(super) fn open_source(src: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(src)
}

pub fn open_no_follow(p: &Path) -> io::Result<File> {
    open_source(p)
}

pub fn is_plain_file(m: &Metadata) -> bool {
    m.is_file()
}

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

pub(super) fn same_source(before: &Metadata, after: &Metadata) -> bool {
    stamp(before) == stamp(after)
}

#[cfg(target_os = "linux")]
pub(super) fn clone_into_new<'a>(
    src: &File,
    _metadata: &Metadata,
    dst: &'a Path,
    create_mode: u32,
) -> Result<PendingFile<'a>, CloneError> {
    use std::os::fd::AsRawFd;
    const FICLONE: u64 = 0x4004_9409;
    let d = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(create_mode)
        .custom_flags(libc::O_CLOEXEC)
        .open(dst)
        .map_err(CloneError::Io)?;
    let pending = PendingFile::new(dst, d);
    let d = pending.file.as_ref().expect("open clone");
    let rc = unsafe { libc::ioctl(d.as_raw_fd(), FICLONE as _, src.as_raw_fd()) };
    if rc != 0 {
        return Err(classify(io::Error::last_os_error()));
    }
    Ok(pending)
}

#[cfg(target_os = "macos")]
pub(super) fn clone_into_new<'a>(
    src: &File,
    _metadata: &Metadata,
    dst: &'a Path,
    create_mode: u32,
) -> Result<PendingFile<'a>, CloneError> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
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
    let normalize = || -> io::Result<File> {
        if unsafe { libc::lchflags(c.as_ptr(), 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        strip_xattrs(&c)?;
        let mode = create_mode & !crate::util::umask() & 0o7777;
        fs::set_permissions(dst, fs::Permissions::from_mode(mode))?;
        open_no_follow(dst)
    };
    match normalize() {
        Ok(file) => Ok(PendingFile::new(dst, file)),
        Err(e) => {
            let _ = fs::remove_file(dst);
            Err(CloneError::Io(e))
        }
    }
}

#[cfg(target_os = "macos")]
fn strip_xattrs(path: &std::ffi::CStr) -> io::Result<()> {
    const XATTR_NOFOLLOW: libc::c_int = 0x0001;
    let size = unsafe { libc::listxattr(path.as_ptr(), std::ptr::null_mut(), 0, XATTR_NOFOLLOW) };
    if size < 0 {
        return Err(io::Error::last_os_error());
    }
    if size == 0 {
        return Ok(());
    }
    let mut buf = vec![0u8; size as usize];
    let n = unsafe {
        libc::listxattr(
            path.as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            XATTR_NOFOLLOW,
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    for name in buf[..n as usize]
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
    {
        let cname = std::ffi::CString::new(name).map_err(io::Error::other)?;
        if unsafe { libc::removexattr(path.as_ptr(), cname.as_ptr(), XATTR_NOFOLLOW) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn clone_into_new<'a>(
    _src: &File,
    _metadata: &Metadata,
    _dst: &'a Path,
    _create_mode: u32,
) -> Result<PendingFile<'a>, CloneError> {
    Err(CloneError::Unsupported(io::Error::from_raw_os_error(
        libc::ENOTSUP,
    )))
}

pub fn clone_preserving(src: &Path, dst: &Path) -> Result<u64, CloneError> {
    let s = open_source(src).map_err(CloneError::Io)?;
    let m = s.metadata().map_err(CloneError::Io)?;
    if !m.is_file() {
        return Err(CloneError::SourceChanged);
    }
    let pending = clone_into_new(&s, &m, dst, 0o600)?;
    let d = pending.file.as_ref().expect("open clone");
    (|| -> io::Result<()> {
        d.set_permissions(fs::Permissions::from_mode(m.permissions().mode() & 0o7777))?;
        let times = fs::FileTimes::new()
            .set_accessed(m.accessed()?)
            .set_modified(m.modified()?);
        d.set_times(times)
    })()
    .map_err(CloneError::Io)?;
    pending.keep();
    Ok(m.len())
}

pub(super) fn probe_clone<'a>(
    source: &mut PendingFile<'_>,
    dst: &'a Path,
) -> Result<PendingFile<'a>, String> {
    let src = open_source(source.path).map_err(|e| e.to_string())?;
    let metadata = src.metadata().map_err(|e| e.to_string())?;
    let mut clone = clone_into_new(&src, &metadata, dst, 0o600);
    if matches!(&clone, Err(CloneError::Unsupported(e)) if e.raw_os_error() == Some(libc::EAGAIN)) {
        source
            .file
            .as_ref()
            .expect("open probe source")
            .sync_all()
            .map_err(|e| e.to_string())?;
        clone = clone_into_new(&src, &metadata, dst, 0o600);
    }
    clone.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn apfs_clone_removes_more_than_four_kib_of_xattr_names() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let dir = tempfile::tempdir().unwrap();
        if super::super::probe(dir.path()).is_err() {
            return;
        }
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        fs::write(&src, vec![5; PROBE_LEN]).unwrap();
        let source = CString::new(src.as_os_str().as_bytes()).unwrap();
        let value = b"v";
        for n in 0..64 {
            let name = CString::new(format!("sharetree.test.{n}.{}", "x".repeat(80))).unwrap();
            let result = unsafe {
                libc::setxattr(
                    source.as_ptr(),
                    name.as_ptr(),
                    value.as_ptr().cast(),
                    value.len(),
                    0,
                    0,
                )
            };
            assert_eq!(result, 0, "{}", io::Error::last_os_error());
        }
        let src = open_source(&src).unwrap();
        let _clone = clone_into_new(&src, &src.metadata().unwrap(), &dst, 0o600).unwrap();
        let dest = CString::new(dst.as_os_str().as_bytes()).unwrap();
        let attrs = unsafe { libc::listxattr(dest.as_ptr(), std::ptr::null_mut(), 0, 0) };
        assert_eq!(attrs, 0);
    }

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
