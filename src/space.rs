use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

use crate::git::{self, Worktree};
use crate::util::human;

struct Usage {
    apparent: u64,
    exclusive: Option<Exclusive>,
}

struct Exclusive {
    bytes: u64,
    method: &'static str,
}

fn top_entries(root: &Path) -> Result<Vec<PathBuf>> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(root)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name() != ".git")
        .map(|e| e.path())
        .collect();
    v.sort();
    Ok(v)
}

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

#[cfg(any(target_os = "linux", test))]
fn parse_btrfs_du(text: &str) -> Option<u64> {
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

fn measure(root: &Path) -> Result<Usage> {
    let entries = top_entries(root)?;
    let apparent = entries.iter().map(|e| allocated(e)).sum();
    #[cfg(target_os = "linux")]
    let exclusive = exclusive(root, &entries);
    #[cfg(not(target_os = "linux"))]
    let exclusive = None;
    Ok(Usage {
        apparent,
        exclusive,
    })
}

#[cfg(target_os = "linux")]
fn exclusive(root: &Path, entries: &[PathBuf]) -> Option<Exclusive> {
    if crate::linux::fs_magic(root) == Some(crate::linux::BTRFS_SUPER_MAGIC) {
        if let Some(bytes) = btrfs_exclusive(entries) {
            return Some(Exclusive {
                bytes,
                method: "btrfs fi du",
            });
        }
    }
    let bytes = entries
        .iter()
        .map(|entry| fiemap_unshared(entry))
        .sum::<Option<u64>>()?;
    Some(Exclusive {
        bytes,
        method: "fiemap estimate",
    })
}

fn has_checkout(worktree: &Worktree) -> bool {
    !worktree.bare && worktree.path.exists()
}

fn sync_before_measuring() {
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("sync").status();
}

pub(crate) fn run_ls(args: &[OsString]) -> Result<i32> {
    let mut sizes = true;
    for a in args {
        match a.to_str() {
            Some("--no-size") => sizes = false,
            _ => bail!("ls: unexpected argument {}", a.to_string_lossy()),
        }
    }
    let cwd = std::env::current_dir()?;
    let worktrees = git::list_worktrees(&cwd)?;
    if sizes {
        sync_before_measuring();
        println!(
            "{:>12} {:>12}  {:<24} PATH",
            "EXCLUSIVE", "APPARENT", "HEAD"
        );
    } else {
        println!("{:<24} PATH", "HEAD");
    }
    let mut measured_any = false;
    for w in &worktrees {
        let head = if w.bare {
            "(bare)".to_string()
        } else if let Some(b) = &w.branch {
            b.trim_start_matches("refs/heads/").to_string()
        } else if let Some(h) = &w.head {
            format!("(detached {})", &h[..h.len().min(10)])
        } else {
            "(no HEAD)".to_string()
        };
        let mut flags = String::new();
        if w.is_main {
            flags.push_str(" [main]");
        }
        if w.locked {
            flags.push_str(" [locked]");
        }
        if w.prunable {
            flags.push_str(" [prunable]");
        }
        if !sizes {
            println!("{head:<24} {}{flags}", w.path.display());
            continue;
        }
        let (excl, app) = if has_checkout(w) {
            let usage = measure(&w.path)?;
            measured_any |= usage.exclusive.is_some();
            (
                usage.exclusive.map_or("?".into(), |x| human(x.bytes)),
                human(usage.apparent),
            )
        } else {
            ("-".to_string(), "-".to_string())
        };
        println!(
            "{excl:>12} {app:>12}  {head:<24} {}{flags}",
            w.path.display()
        );
    }
    if sizes {
        println!(
            "\nEXCLUSIVE: bytes only that worktree holds ({}); APPARENT: what `du` reports.\nThe main worktree's .git is not counted. See `sharetree du` for details.",
            if measured_any { "btrfs fi du or FIEMAP" } else { "not measurable on this filesystem" }
        );
    }
    Ok(0)
}

const WHY_DU_LIES: &str = "\
`du` (APPARENT) adds up every file's blocks and cannot see that reflinked
extents are shared, so it reports each worktree as a full copy. EXCLUSIVE is
what deleting the worktree would free. On Btrfs it comes from
`btrfs filesystem du -s`; otherwise it is a FIEMAP estimate. For a ground
truth, compare `df` (statvfs free space) before and after, after `sync`, on an
otherwise idle filesystem. Extents shared with snapshots count as shared.";

pub(crate) fn run_du(args: &[OsString]) -> Result<i32> {
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
            .filter(has_checkout)
            .map(|w| w.path)
            .collect();
    } else if paths.is_empty() {
        paths.push(git::toplevel(&cwd)?);
    }
    sync_before_measuring();
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
                u.exclusive.map_or("-".to_string(), |x| x.bytes.to_string()),
                p.display()
            );
        } else {
            let (exclusive, method) = u
                .exclusive
                .map_or(("?".to_string(), "n/a"), |x| (human(x.bytes), x.method));
            println!(
                "{:>12} {:>12}  {:<22} {}",
                human(u.apparent),
                exclusive,
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
