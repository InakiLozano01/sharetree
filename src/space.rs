//! Space accounting (design §3, §8.5 `du`).
//!
//! `du` sums each file's allocated blocks and cannot see that extents are
//! shared, so N reflinked worktrees look like N full copies. What matters is
//! *exclusive* bytes: the space that deleting the worktree would free.
//!
//! * Btrfs: `btrfs filesystem du -s` reports Total / Exclusive per path.
//! * Other Linux filesystems with FIEMAP (XFS reflink=1, ...): the sum of
//!   extents not flagged SHARED, an estimate (logical lengths; an extent
//!   shared with a snapshot or with another file in the same tree counts as
//!   shared).
//! * Elsewhere: not measurable here; use a `df` delta on an idle volume.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

use crate::git;
use crate::util::human;

#[derive(Clone, Debug)]
pub struct Usage {
    /// What `du` would say: allocated blocks, shared or not.
    pub apparent: u64,
    /// Bytes only this worktree holds, if measurable.
    pub exclusive: Option<u64>,
    pub method: &'static str,
}

/// Top-level entries of a worktree, minus its `.git`.
fn top_entries(root: &Path) -> Result<Vec<PathBuf>> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(root)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name() != ".git")
        .map(|e| e.path())
        .collect();
    v.sort();
    Ok(v)
}

/// Sum of allocated bytes under `p` (like `du -s`), not following symlinks.
fn allocated(p: &Path) -> u64 {
    let Ok(md) = p.symlink_metadata() else {
        return 0;
    };
    #[cfg(unix)]
    let own = {
        use std::os::unix::fs::MetadataExt;
        md.blocks() * 512
    };
    #[cfg(not(unix))]
    let own = md.len();
    if md.is_dir() {
        own + std::fs::read_dir(p)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| allocated(&e.path()))
                    .sum::<u64>()
            })
            .unwrap_or(0)
    } else {
        own
    }
}

#[cfg(target_os = "linux")]
fn btrfs_exclusive(entries: &[PathBuf]) -> Option<u64> {
    if entries.is_empty() {
        return Some(0);
    }
    let mut args: Vec<OsString> = vec![
        "filesystem".into(),
        "du".into(),
        "-s".into(),
        "--raw".into(),
    ];
    args.extend(entries.iter().map(|p| p.as_os_str().to_owned()));
    let out = std::process::Command::new("btrfs")
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_btrfs_du(&String::from_utf8_lossy(&out.stdout))
}

/// Sum the Exclusive column of `btrfs filesystem du -s --raw`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn parse_btrfs_du(text: &str) -> Option<u64> {
    let mut sum = 0u64;
    let mut rows = 0;
    for line in text.lines().skip(1) {
        let mut f = line.split_whitespace();
        let (Some(_total), Some(excl)) = (f.next(), f.next()) else {
            continue;
        };
        sum += excl.parse::<u64>().ok()?;
        rows += 1;
    }
    (rows > 0).then_some(sum)
}

#[cfg(target_os = "linux")]
fn fiemap_unshared(p: &Path) -> Option<u64> {
    let md = p.symlink_metadata().ok()?;
    if md.is_dir() {
        let mut sum = 0;
        for e in std::fs::read_dir(p).ok()?.filter_map(|e| e.ok()) {
            sum += fiemap_unshared(&e.path())?;
        }
        Some(sum)
    } else if md.is_file() {
        let f = std::fs::File::open(p).ok()?;
        let ext = crate::linux::extents(&f, true).ok()?;
        Some(ext.iter().filter(|e| !e.shared()).map(|e| e.length).sum())
    } else {
        Some(0)
    }
}

pub fn measure(root: &Path) -> Result<Usage> {
    let entries = top_entries(root)?;
    let apparent = entries.iter().map(|e| allocated(e)).sum();
    #[cfg(target_os = "linux")]
    {
        if crate::linux::fs_magic(root) == Some(crate::linux::BTRFS_SUPER_MAGIC) {
            if let Some(x) = btrfs_exclusive(&entries) {
                return Ok(Usage {
                    apparent,
                    exclusive: Some(x),
                    method: "btrfs filesystem du",
                });
            }
        }
        let mut sum = Some(0u64);
        for e in &entries {
            sum = sum.and_then(|s| fiemap_unshared(e).map(|x| s + x));
        }
        if let Some(x) = sum {
            return Ok(Usage {
                apparent,
                exclusive: Some(x),
                method: "FIEMAP unshared extents (estimate)",
            });
        }
    }
    Ok(Usage {
        apparent,
        exclusive: None,
        method: "not measurable here (use a df delta)",
    })
}

pub const WHY_DU_LIES: &str = "\
`du` (APPARENT) adds up every file's blocks and cannot see that reflinked
extents are shared, so it reports each worktree as a full copy. EXCLUSIVE is
what deleting the worktree would free. On Btrfs it comes from
`btrfs filesystem du -s`; otherwise it is a FIEMAP estimate. For a ground
truth, compare `df` (statvfs free space) before and after, after `sync`, on an
otherwise idle filesystem. Extents shared with snapshots count as shared.";

pub fn run_du(args: &[OsString]) -> Result<i32> {
    let mut raw = false;
    let mut all = false;
    let mut paths: Vec<PathBuf> = Vec::new();
    for a in args {
        match a.to_str() {
            Some("--raw") => raw = true,
            Some("--all") => all = true,
            Some(s) if s.starts_with('-') => bail!("du: unknown option {s}"),
            _ => paths.push(PathBuf::from(a)),
        }
    }
    let cwd = std::env::current_dir()?;
    if all {
        paths = git::list_worktrees(&cwd)?
            .into_iter()
            .filter(|w| !w.bare && w.path.exists())
            .map(|w| w.path)
            .collect();
    } else if paths.is_empty() {
        paths.push(git::toplevel(&cwd)?);
    }
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("sync").status();
    if !raw {
        println!(
            "{:>12} {:>12}  {:<22} PATH",
            "APPARENT", "EXCLUSIVE", "METHOD"
        );
    }
    for p in &paths {
        let u = measure(p)?;
        if raw {
            println!(
                "{}\t{}\t{}",
                u.apparent,
                u.exclusive.map_or("-".to_string(), |x| x.to_string()),
                p.display()
            );
        } else {
            let method = if u.method.starts_with("btrfs") {
                "btrfs fi du"
            } else if u.method.starts_with("FIEMAP") {
                "fiemap estimate"
            } else {
                "n/a"
            };
            println!(
                "{:>12} {:>12}  {:<22} {}",
                human(u.apparent),
                u.exclusive.map_or("?".to_string(), human),
                method,
                p.display()
            );
        }
    }
    if !raw {
        println!("\n{WHY_DU_LIES}");
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_btrfs_du_output() {
        let t = "     Total   Exclusive  Set shared  Filename\n  50331648       12288    50319360  /a/x\n      4096        4096           0  /a/y\n";
        assert_eq!(parse_btrfs_du(t), Some(16384));
        assert_eq!(
            parse_btrfs_du("     Total   Exclusive  Set shared  Filename\n"),
            None
        );
    }
}
