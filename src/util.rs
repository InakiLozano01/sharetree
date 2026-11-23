use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;
use std::time::Instant;

static UMASK: OnceLock<u32> = OnceLock::new();

pub fn init_umask() {
    #[cfg(unix)]
    UMASK.get_or_init(|| unsafe {
        let m = libc::umask(0o022);
        libc::umask(m);
        m as u32
    });
    #[cfg(not(unix))]
    UMASK.get_or_init(|| 0);
}

#[allow(dead_code)]
pub fn umask() -> u32 {
    *UMASK.get().unwrap_or(&0o022)
}

#[cfg(unix)]
pub fn bytes_to_path(b: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(b))
}

#[cfg(not(unix))]
pub fn bytes_to_path(b: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(b).replace('/', "\\"))
}

#[cfg(unix)]
pub fn path_to_bytes(p: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    p.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
pub fn path_to_bytes(p: &Path) -> Vec<u8> {
    p.to_string_lossy().replace('\\', "/").into_bytes()
}

pub fn absolutize(p: &Path, base: &Path) -> PathBuf {
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(unix)]
pub fn device_id(p: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).ok().map(|m| m.dev())
}

#[cfg(windows)]
pub fn device_id(p: &Path) -> Option<u64> {
    crate::winclone::volume_serial(p).ok()
}

#[cfg(not(any(unix, windows)))]
pub fn device_id(_p: &Path) -> Option<u64> {
    None
}

#[cfg(unix)]
pub fn exec_bit_matches(m: &std::fs::Metadata, exec: bool) -> bool {
    use std::os::unix::fs::PermissionsExt;
    (m.permissions().mode() & 0o100 != 0) == exec
}

#[cfg(not(unix))]
pub fn exec_bit_matches(_m: &std::fs::Metadata, _exec: bool) -> bool {
    true
}

pub fn nearest_existing(p: &Path) -> PathBuf {
    let mut cur = p.to_path_buf();
    loop {
        if cur.symlink_metadata().is_ok() {
            return cur;
        }
        if !cur.pop() {
            return PathBuf::from("/");
        }
    }
}

pub fn canon(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

pub fn is_within(child: &Path, parent: &Path) -> bool {
    canon(child).starts_with(canon(parent))
}

pub fn human(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

pub struct Trace {
    on: bool,
    start: Instant,
}

impl Trace {
    pub fn new(verbose: bool) -> Self {
        let env = std::env::var_os("SHARETREE_TRACE").is_some_and(|v| !v.is_empty() && v != "0");
        Trace {
            on: verbose || env,
            start: Instant::now(),
        }
    }

    pub fn phase(&self, name: &str, t0: Instant) {
        if self.on {
            eprintln!(
                "sharetree: {name:<14} {:>8.1} ms",
                t0.elapsed().as_secs_f64() * 1000.0
            );
        }
    }

    pub fn note(&self, msg: impl AsRef<str>) {
        if self.on {
            eprintln!("sharetree: {}", msg.as_ref());
        }
    }

    pub fn total(&self) {
        if self.on {
            eprintln!(
                "sharetree: {:<14} {:>8.1} ms",
                "total",
                self.start.elapsed().as_secs_f64() * 1000.0
            );
        }
    }
}

pub fn workers(n_items: usize) -> usize {
    let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
    cpus.min(8).min(n_items.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolutize_resolves_dots() {
        assert_eq!(
            absolutize(Path::new("../b/./c"), Path::new("/x/a")),
            PathBuf::from("/x/b/c")
        );
        assert_eq!(
            absolutize(Path::new("/q/r"), Path::new("/x")),
            PathBuf::from("/q/r")
        );
    }

    #[test]
    fn human_units() {
        assert_eq!(human(10), "10 B");
        assert_eq!(human(1536), "1.5 KiB");
        assert_eq!(human(3 * 1024 * 1024), "3.0 MiB");
    }
}
