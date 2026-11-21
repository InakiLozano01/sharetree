use std::fs::{File, Metadata, OpenOptions};
use std::io;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

use crate::winclone;

use super::{CloneError, PendingFile};

pub(super) const CLONE_NAME: &str = "block cloning";
pub(super) const PROBE_LEN: usize = 65536;

pub use winclone::is_plain_file;
pub(super) use winclone::open_source;

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

fn stamp(m: &Metadata) -> (u64, u64, u64, u32) {
    (
        m.file_size(),
        m.last_write_time(),
        m.creation_time(),
        m.file_attributes(),
    )
}

pub(super) fn same_source(before: &Metadata, after: &Metadata) -> bool {
    stamp(before) == stamp(after)
}

pub(super) fn clone_into_new<'a>(
    src: &File,
    metadata: &Metadata,
    dst: &'a Path,
    _create_mode: u32,
) -> Result<PendingFile<'a>, CloneError> {
    let clone = winclone::clone_file(src, metadata.file_attributes(), dst, metadata.len())
        .map_err(classify)?;
    Ok(PendingFile::new(dst, clone))
}

pub(super) fn probe_clone<'a>(
    source: &mut PendingFile<'_>,
    dst: &'a Path,
) -> Result<PendingFile<'a>, String> {
    source.close();
    let src = open_source(source.path).map_err(|e| e.to_string())?;
    if !winclone::volume_can_clone(&src).map_err(|e| e.to_string())? {
        return Err(format!(
            "the volume holding {} does not support block cloning (ReFS / Dev Drive on \
             Windows 11 24H2 or later)",
            source.path.parent().unwrap_or(source.path).display()
        ));
    }
    let metadata = src.metadata().map_err(|e| e.to_string())?;
    clone_into_new(&src, &metadata, dst, 0).map_err(|e| e.to_string())
}
