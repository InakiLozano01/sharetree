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

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use anyhow::{bail, Context, Result};
use sha1::Digest;

use crate::git::ObjectFormat;
#[cfg(any(unix, windows))]
use crate::{git, hash, reflink, util};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Stat {
    ctime_s: u32,
    ctime_ns: u32,
    mtime_s: u32,
    mtime_ns: u32,
    dev: u32,
    ino: u32,
    uid: u32,
    gid: u32,
    size: u32,
}

#[cfg(unix)]
impl Stat {
    /// The fields git stores, truncated to 32 bits exactly as git does.
    fn from_metadata(m: &std::fs::Metadata) -> Stat {
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
    /// What Git for Windows' `mingw_lstat` reports.
    fn from_metadata(m: &std::fs::Metadata) -> Stat {
        use std::os::windows::fs::MetadataExt;
        Stat::from_windows_times(m.creation_time(), m.last_write_time(), m.file_size())
    }
}

#[cfg(any(windows, test))]
const EPOCH_DIFF_100NS: i64 = 116_444_736_000_000_000;

#[cfg(any(windows, test))]
fn filetime_to_unix(ft: u64) -> (i64, u32) {
    // Git's compatibility layer uses creation and last-write times, both
    // in 100 ns intervals since the FILETIME epoch of 1601-01-01.
    let t = ft as i64 - EPOCH_DIFF_100NS;
    (
        t.div_euclid(10_000_000),
        (t.rem_euclid(10_000_000) * 100) as u32,
    )
}

#[cfg(any(windows, test))]
impl Stat {
    fn from_windows_times(creation: u64, last_write: u64, size: u64) -> Self {
        let (cs, cns) = filetime_to_unix(creation);
        let (ms, mns) = filetime_to_unix(last_write);
        Self {
            ctime_s: cs as u32,
            ctime_ns: cns,
            mtime_s: ms as u32,
            mtime_ns: mns,
            size: size as u32,
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug)]
struct IndexEntry {
    path: Vec<u8>,
    mode: u32,
    oid: Vec<u8>,
    stat: Stat,
}

#[cfg(any(unix, windows))]
pub struct VerifiedClone {
    entry: IndexEntry,
    size: u64,
}

/// Hash the bytes of a newly created clone against the target blob. A clone
/// that fails verification is removed so Git can write the path itself.
#[cfg(any(unix, windows))]
pub fn verify_clone(
    worktree: &Path,
    path: &[u8],
    target: &git::Entry,
    fmt: ObjectFormat,
) -> Option<VerifiedClone> {
    let full = worktree.join(util::bytes_to_path(path));
    let verify = || {
        let want = hex_to_raw(&target.oid)?;
        if !target.is_regular() || want.len() != fmt.raw_len() {
            return None;
        }
        let mut file = reflink::open_no_follow(&full).ok()?;
        let before = file.metadata().ok()?;
        let stat = clone_stat(&before, target.size, target.mode)?;
        if hash::blob_oid(&mut file, target.size, fmt).ok()?? != want {
            return None;
        }
        let after = file.metadata().ok()?;
        if clone_stat(&after, target.size, target.mode) != Some(stat) {
            return None;
        }
        Some(VerifiedClone {
            entry: IndexEntry {
                path: path.to_vec(),
                mode: target.mode,
                oid: want,
                stat,
            },
            size: target.size,
        })
    };
    let verified = verify();
    if verified.is_none() {
        let _ = fs::remove_file(full);
    }
    verified
}

#[cfg(any(unix, windows))]
fn clone_stat(metadata: &fs::Metadata, size: u64, mode: u32) -> Option<Stat> {
    (reflink::is_plain_file(metadata)
        && metadata.len() == size
        && util::exec_bit_matches(metadata, mode == git::MODE_EXEC))
    .then(|| Stat::from_metadata(metadata))
}

#[cfg(any(unix, windows))]
#[derive(Default)]
pub struct SeededIndex {
    pub entries: usize,
    pub bytes: u64,
}

/// Recheck each verified clone and write the fresh worktree's index with
/// only the files Git can keep. Git creates its own index if none qualify.
#[cfg(any(unix, windows))]
pub fn seed_worktree(
    worktree: &Path,
    verified: Vec<VerifiedClone>,
    fmt: ObjectFormat,
) -> Result<SeededIndex> {
    let mut seeded = SeededIndex::default();
    let mut entries = Vec::with_capacity(verified.len());
    for VerifiedClone { entry, size } in verified {
        let full = worktree.join(util::bytes_to_path(&entry.path));
        let still = fs::symlink_metadata(&full)
            .ok()
            .and_then(|md| clone_stat(&md, size, entry.mode));
        if still == Some(entry.stat) {
            seeded.bytes += size;
            entries.push(entry);
        } else {
            let _ = fs::remove_file(full);
        }
    }
    seeded.entries = entries.len();
    if !entries.is_empty() {
        write(&git::git_path(worktree, "index")?, &mut entries, fmt)?;
    }
    Ok(seeded)
}

const NAME_MASK: usize = 0x0fff;

/// Serialize `entries` (any order) as a v2 index with a trailing checksum.
fn encode(entries: &mut [IndexEntry], fmt: ObjectFormat) -> Result<Vec<u8>> {
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
fn write(path: &Path, entries: &mut [IndexEntry], fmt: ObjectFormat) -> Result<()> {
    let data = encode(entries, fmt)?;
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
    let res = (|| -> Result<()> {
        // A concurrent Git command could have created the index before we
        // took its lock. This partial index is only for a fresh worktree.
        match fs::symlink_metadata(path) {
            Ok(_) => bail!("{} already exists", path.display()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("inspect {}", path.display())),
        }
        f.write_all(&data).context("write index.lock")
    })();
    drop(f);
    if let Err(e) = res {
        let _ = std::fs::remove_file(&lock);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&lock, path) {
        let _ = std::fs::remove_file(&lock);
        return Err(e).context("rename index.lock");
    }
    Ok(())
}

fn hex_to_raw(hex: &str) -> Option<Vec<u8>> {
    let (pairs, remainder) = hex.as_bytes().as_chunks::<2>();
    if !remainder.is_empty() {
        return None;
    }
    pairs
        .iter()
        .map(|&[a, b]| Some(((a as char).to_digit(16)? * 16 + (b as char).to_digit(16)?) as u8))
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
        assert_eq!(hex_to_raw("00é1"), None);
        assert_eq!(hex_to_raw("0€"), None);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn seeded_index_contains_only_verified_unchanged_clones() {
        for fmt in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            let object_format = match fmt {
                ObjectFormat::Sha1 => "--object-format=sha1",
                ObjectFormat::Sha256 => "--object-format=sha256",
            };
            git::output(root, &["init", object_format]).unwrap();
            let good = b"hello\n";
            let oid =
                git::output_with_input(root, &["hash-object", "-w", "--stdin"], good).unwrap();
            let oid = std::str::from_utf8(git::trim_nl(&oid)).unwrap().to_string();
            let target = git::Entry {
                oid: oid.clone(),
                mode: git::MODE_REGULAR,
                size: good.len() as u64,
            };
            fs::write(root.join("same"), good).unwrap();
            fs::write(root.join("changed"), good).unwrap();
            fs::write(root.join("wrong"), b"world\n").unwrap();

            assert!(verify_clone(root, b"wrong", &target, fmt).is_none());
            assert!(!root.join("wrong").exists());
            let unchanged = verify_clone(root, b"same", &target, fmt).unwrap();
            let changed = verify_clone(root, b"changed", &target, fmt).unwrap();
            fs::write(root.join("changed"), b"different size\n").unwrap();

            let seeded = seed_worktree(root, vec![unchanged, changed], fmt).unwrap();
            assert_eq!(seeded.entries, 1);
            assert_eq!(seeded.bytes, good.len() as u64);
            assert!(!root.join("changed").exists());
            assert_eq!(
                git::ls_files_stage(root).unwrap(),
                vec![(b"same".to_vec(), git::MODE_REGULAR, oid)]
            );
        }
    }

    #[test]
    fn existing_git_index_is_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index");
        fs::write(&path, b"existing index").unwrap();
        assert!(write(&path, &mut [], ObjectFormat::Sha1).is_err());
        assert_eq!(fs::read(path).unwrap(), b"existing index");
        assert!(!dir.path().join("index.lock").exists());
    }

    #[test]
    fn filetimes_map_like_git_for_windows() {
        let epoch = EPOCH_DIFF_100NS as u64;
        assert_eq!(filetime_to_unix(epoch), (0, 0));
        let ft = epoch + 1_767_225_600 * 10_000_000 + 1_234_567;
        assert_eq!(filetime_to_unix(ft), (1_767_225_600, 123_456_700));
        assert_eq!(filetime_to_unix(epoch - 1), (-1, 999_999_900));

        let stat = Stat::from_windows_times(ft, ft + 10_000_000, (5 << 32) + 7);
        assert_eq!((stat.ctime_s, stat.ctime_ns), (1_767_225_600, 123_456_700));
        assert_eq!(stat.mtime_s, 1_767_225_601);
        assert_eq!((stat.dev, stat.ino, stat.uid, stat.gid), (0, 0, 0, 0));
        assert_eq!(stat.size, 7, "truncated to 32 bits like git");
    }
}
