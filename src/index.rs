//! Minimal git index (version 2, no extensions) writer.
//!
//! Ported from josharian/git-cow-worktree `index.go` (MIT, see
//! THIRD_PARTY_NOTICES). The index exists for exactly one reader: the
//! `git checkout -f HEAD` that follows. Git decides whether to rewrite a
//! working-tree file by comparing it against the cached stat data of its
//! index entry, so without this index the checkout would rewrite every file
//! we just cloned and undo the sharing. Entries we omit are not lost:
//! checkout builds the real index from the target tree and writes anything
//! missing here.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use anyhow::{bail, Context, Result};
use sha1::Digest;

use crate::git::ObjectFormat;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stat {
    pub ctime_s: u32,
    pub ctime_ns: u32,
    pub mtime_s: u32,
    pub mtime_ns: u32,
    pub dev: u32,
    pub ino: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u32,
}

#[cfg(unix)]
impl Stat {
    /// The fields git stores, truncated to 32 bits exactly as git does.
    pub fn from_metadata(m: &std::fs::Metadata) -> Stat {
        use std::os::unix::fs::MetadataExt;
        Stat {
            ctime_s: m.ctime() as u32,
            ctime_ns: m.ctime_nsec() as u32,
            mtime_s: m.mtime() as u32,
            mtime_ns: m.mtime_nsec() as u32,
            dev: m.dev() as u32,
            ino: m.ino() as u32,
            uid: m.uid(),
            gid: m.gid(),
            size: m.size() as u32,
        }
    }
}

#[cfg(windows)]
impl Stat {
    /// What Git for Windows' `mingw_lstat` reports (see winclone.rs).
    pub fn from_metadata(m: &std::fs::Metadata) -> Stat {
        use std::os::windows::fs::MetadataExt;
        crate::winclone::git_stat(m.creation_time(), m.last_write_time(), m.file_size())
    }
}

#[derive(Clone, Debug)]
pub struct IndexEntry {
    pub path: Vec<u8>,
    pub mode: u32,
    pub oid: Vec<u8>,
    pub stat: Stat,
}

const NAME_MASK: usize = 0x0fff;

/// Serialize `entries` (any order) as a v2 index with a trailing checksum.
pub fn encode(entries: &mut [IndexEntry], fmt: ObjectFormat) -> Result<Vec<u8>> {
    // Git requires entries sorted by raw path bytes (stage 0 only here).
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    for w in entries.windows(2) {
        if w[0].path == w[1].path {
            bail!(
                "duplicate index entry {:?}",
                String::from_utf8_lossy(&w[0].path)
            );
        }
    }
    let mut buf = Vec::with_capacity(12 + entries.len() * 96);
    buf.extend_from_slice(b"DIRC");
    buf.extend_from_slice(&2u32.to_be_bytes());
    buf.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for e in entries.iter() {
        if e.oid.len() != fmt.raw_len() {
            bail!(
                "object id length {} does not match the repository format",
                e.oid.len()
            );
        }
        let start = buf.len();
        let s = &e.stat;
        for v in [
            s.ctime_s, s.ctime_ns, s.mtime_s, s.mtime_ns, s.dev, s.ino, e.mode, s.uid, s.gid,
            s.size,
        ] {
            buf.extend_from_slice(&v.to_be_bytes());
        }
        buf.extend_from_slice(&e.oid);
        let flags = e.path.len().min(NAME_MASK) as u16;
        buf.extend_from_slice(&flags.to_be_bytes());
        buf.extend_from_slice(&e.path);
        // At least one NUL, then pad the entry to a multiple of 8 bytes.
        buf.push(0);
        while (buf.len() - start) % 8 != 0 {
            buf.push(0);
        }
    }
    match fmt {
        ObjectFormat::Sha1 => {
            let sum = sha1::Sha1::digest(&buf);
            buf.extend_from_slice(&sum);
        }
        ObjectFormat::Sha256 => {
            let sum = sha2::Sha256::digest(&buf);
            buf.extend_from_slice(&sum);
        }
    }
    Ok(buf)
}

/// Write the index through `index.lock` + rename, as git does, so a
/// concurrent git process fails loudly instead of racing us and a partial
/// write can never become the index.
pub fn write(path: &Path, entries: &mut [IndexEntry], fmt: ObjectFormat) -> Result<()> {
    let mut data = encode(entries, fmt)?;
    if std::env::var_os("SHARETREE_TEST_CORRUPT_INDEX").is_some() {
        // Test hook: an entry count git will refuse, to exercise recovery.
        data[8..12].copy_from_slice(&0x7fff_ffffu32.to_be_bytes());
    }
    let mut lock = path.as_os_str().to_owned();
    lock.push(".lock");
    let lock = std::path::PathBuf::from(lock);
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock)
        .with_context(|| format!("create {}", lock.display()))?;
    // No fsync, like git's own index writes: the next command reads it at
    // once, and on Btrfs an fsync here waits for the transaction holding
    // every clone we just made (1.4 s on a kernel tree).
    let res = f.write_all(&data);
    drop(f);
    if let Err(e) = res {
        let _ = std::fs::remove_file(&lock);
        return Err(e).context("write index.lock");
    }
    if let Err(e) = std::fs::rename(&lock, path) {
        let _ = std::fs::remove_file(&lock);
        return Err(e).context("rename index.lock");
    }
    Ok(())
}

pub fn hex_to_raw(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_are_sorted_padded_and_checksummed() {
        let mut v = vec![
            IndexEntry {
                path: b"b".to_vec(),
                mode: 0o100644,
                oid: vec![1; 20],
                stat: Stat::default(),
            },
            IndexEntry {
                path: b"a/c".to_vec(),
                mode: 0o100755,
                oid: vec![2; 20],
                stat: Stat::default(),
            },
        ];
        let data = encode(&mut v, ObjectFormat::Sha1).unwrap();
        assert_eq!(&data[..4], b"DIRC");
        assert_eq!(u32::from_be_bytes(data[8..12].try_into().unwrap()), 2);
        // 62-byte fixed part + 3-byte name + NUL padding to 72.
        assert_eq!(&data[12 + 62..12 + 65], b"a/c");
        assert_eq!((data.len() - 12 - 20) % 8, 0);
        let body = &data[..data.len() - 20];
        assert_eq!(
            &data[data.len() - 20..],
            sha1::Sha1::digest(body).as_slice()
        );
    }

    #[test]
    fn rejects_wrong_oid_length() {
        let mut v = vec![IndexEntry {
            path: b"x".to_vec(),
            mode: 0o100644,
            oid: vec![0; 20],
            stat: Stat::default(),
        }];
        assert!(encode(&mut v, ObjectFormat::Sha256).is_err());
    }

    #[test]
    fn hex_round_trip() {
        assert_eq!(hex_to_raw("00ff10"), Some(vec![0, 255, 16]));
        assert_eq!(hex_to_raw("0"), None);
    }
}
