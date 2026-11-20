//! Windows clone primitives for the add engine, with the same interface as
//! `reflink.rs` on Unix. Block cloning itself lives in `winclone.rs`; this
//! file adds the donor checks and the probe. Experimental and untested: the
//! engine reaches it only with `--windows-clone` / `windows_clone = true`.

use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

use crate::winclone;

#[derive(Debug)]
pub enum CloneError {
    /// The volume (or this file pair) cannot block-clone.
    Unsupported(io::Error),
    /// The donor changed while it was cloned, or is not what the plan expected.
    SourceChanged,
    /// Any other failure.
    Io(io::Error),
}

impl std::fmt::Display for CloneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CloneError::Unsupported(e) => write!(f, "block cloning unsupported: {e}"),
            CloneError::SourceChanged => write!(f, "source changed during clone"),
            CloneError::Io(e) => write!(f, "{e}"),
        }
    }
}

// ERROR_NOT_SUPPORTED, ERROR_INVALID_FUNCTION, ERROR_NOT_SAME_DEVICE,
// ERROR_INVALID_PARAMETER: the volume or file pair cannot clone.
fn classify(e: io::Error) -> CloneError {
    match e.raw_os_error() {
        Some(50 | 1 | 17 | 87) => CloneError::Unsupported(e),
        _ => CloneError::Io(e),
    }
}

pub fn open_no_follow(p: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(p)
}

pub fn is_plain_file(m: &Metadata) -> bool {
    winclone::is_plain_file(m)
}

fn stamp(m: &Metadata) -> (u64, u64, u64, u32) {
    (
        m.file_size(),
        m.last_write_time(),
        m.creation_time(),
        m.file_attributes(),
    )
}

/// Clone a tracked file for the new worktree: flush the donor, clone it
/// block by block, and make sure the donor did not change meanwhile (it is
/// held open denying other writers). `_create_mode` is unused: Windows has
/// no exec bit and git ignores file modes there. Content is verified
/// separately by hashing the clone.
pub fn clone_tracked(
    src: &Path,
    dst: &Path,
    _create_mode: u32,
    want_size: u64,
) -> Result<(), CloneError> {
    let s = winclone::open_source(src).map_err(CloneError::Io)?;
    let before = s.metadata().map_err(CloneError::Io)?;
    if !winclone::is_plain_file(&before) || before.len() != want_size {
        return Err(CloneError::SourceChanged);
    }
    let d = winclone::clone_file(&s, before.file_attributes(), dst, want_size).map_err(classify)?;
    drop(d);
    let after = s.metadata().map_err(CloneError::Io)?;
    if stamp(&before) != stamp(&after) {
        let _ = fs::remove_file(dst);
        return Err(CloneError::SourceChanged);
    }
    Ok(())
}

/// Prove that `dir` can block-clone: the volume flag, then a real clone of
/// a 64 KiB file whose bytes are read back and compared.
pub fn probe(dir: &Path) -> Result<(), String> {
    let tag = format!(".sharetree-probe-{}", std::process::id());
    let a = dir.join(format!("{tag}.a"));
    let b = dir.join(format!("{tag}.b"));
    let res = (|| -> Result<(), String> {
        let data: Vec<u8> = (0..65536u32).map(|i| (i * 7 + 3) as u8).collect();
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&a)
            .map_err(|e| format!("cannot create a probe file in {}: {e}", dir.display()))?;
        f.write_all(&data).map_err(|e| e.to_string())?;
        drop(f);
        let s = winclone::open_source(&a).map_err(|e| e.to_string())?;
        if !winclone::volume_can_clone(&s).map_err(|e| e.to_string())? {
            return Err(format!(
                "the volume holding {} does not support block cloning (ReFS / Dev Drive on \
                 Windows 11 24H2 or later)",
                dir.display()
            ));
        }
        let attrs = s.metadata().map_err(|e| e.to_string())?.file_attributes();
        drop(winclone::clone_file(&s, attrs, &b, data.len() as u64).map_err(|e| e.to_string())?);
        let mut back = Vec::new();
        File::open(&b)
            .and_then(|mut f| f.read_to_end(&mut back))
            .map_err(|e| e.to_string())?;
        if back != data {
            return Err("a test clone read back different bytes".into());
        }
        Ok(())
    })();
    let _ = fs::remove_file(&a);
    let _ = fs::remove_file(&b);
    res
}
