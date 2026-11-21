use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;

#[cfg(unix)]
#[path = "reflink_unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "reflink_windows.rs"]
mod platform;

#[cfg(unix)]
pub use platform::clone_preserving;
pub use platform::{is_plain_file, open_no_follow};

#[derive(Debug)]
pub enum CloneError {
    Unsupported(io::Error),
    SourceChanged,
    Io(io::Error),
}

impl std::fmt::Display for CloneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CloneError::Unsupported(e) => write!(f, "{} unsupported: {e}", platform::CLONE_NAME),
            CloneError::SourceChanged => write!(f, "source changed during clone"),
            CloneError::Io(e) => write!(f, "{e}"),
        }
    }
}

struct PendingFile<'a> {
    path: &'a Path,
    file: Option<File>,
    keep: bool,
}

impl<'a> PendingFile<'a> {
    fn new(path: &'a Path, file: File) -> Self {
        Self {
            path,
            file: Some(file),
            keep: false,
        }
    }

    fn close(&mut self) {
        drop(self.file.take());
    }

    fn keep(mut self) {
        self.keep = true;
    }
}

impl Drop for PendingFile<'_> {
    fn drop(&mut self) {
        self.close();
        if !self.keep {
            let _ = fs::remove_file(self.path);
        }
    }
}

pub fn clone_tracked(
    src: &Path,
    dst: &Path,
    create_mode: u32,
    want_size: u64,
) -> Result<(), CloneError> {
    let src = platform::open_source(src).map_err(CloneError::Io)?;
    let before = src.metadata().map_err(CloneError::Io)?;
    if !is_plain_file(&before) || before.len() != want_size {
        return Err(CloneError::SourceChanged);
    }
    let clone = platform::clone_into_new(&src, &before, dst, create_mode)?;
    let after = src.metadata().map_err(CloneError::Io)?;
    if !platform::same_source(&before, &after) {
        return Err(CloneError::SourceChanged);
    }
    clone.keep();
    Ok(())
}

pub fn probe(dir: &Path) -> Result<(), String> {
    let tag = format!(
        ".sharetree-probe-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    );
    probe_named(dir, &tag)
}

fn probe_named(dir: &Path, tag: &str) -> Result<(), String> {
    let a = dir.join(format!("{tag}.a"));
    let b = dir.join(format!("{tag}.b"));
    let data: Vec<u8> = (0..platform::PROBE_LEN)
        .map(|i| (i * 7 + 3) as u8)
        .collect();
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut source = PendingFile::new(
        &a,
        opts.open(&a)
            .map_err(|e| format!("cannot create a probe file in {}: {e}", dir.display()))?,
    );
    source
        .file
        .as_mut()
        .expect("open probe source")
        .write_all(&data)
        .map_err(|e| e.to_string())?;
    let mut clone = platform::probe_clone(&mut source, &b)?;
    let mut back = Vec::with_capacity(data.len());
    clone
        .file
        .as_mut()
        .expect("open probe clone")
        .read_to_end(&mut back)
        .map_err(|e| e.to_string())?;
    if back != data {
        return Err("a test clone read back different bytes".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_preserves_existing_files_with_its_names() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("collision.a");
        let b = dir.path().join("collision.b");
        fs::write(&a, b"existing source").unwrap();
        fs::write(&b, b"existing destination").unwrap();
        assert!(probe_named(dir.path(), "collision").is_err());
        assert_eq!(fs::read(&a).unwrap(), b"existing source");
        assert_eq!(fs::read(&b).unwrap(), b"existing destination");

        fs::remove_file(&a).unwrap();
        assert!(probe_named(dir.path(), "collision").is_err());
        assert!(!a.exists());
        assert_eq!(fs::read(&b).unwrap(), b"existing destination");
    }
}
