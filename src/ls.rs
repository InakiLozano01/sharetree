//! `sharetree ls`: worktrees with the bytes only they hold.

use std::ffi::OsString;

use anyhow::{bail, Result};

use crate::git;
use crate::space;
use crate::util::human;

pub fn run(args: &[OsString]) -> Result<i32> {
    let mut sizes = true;
    for a in args {
        match a.to_str() {
            Some("--no-size") => sizes = false,
            _ => bail!("ls: unexpected argument {}", a.to_string_lossy()),
        }
    }
    let cwd = std::env::current_dir()?;
    let wts = git::list_worktrees(&cwd)?;
    #[cfg(target_os = "linux")]
    if sizes {
        let _ = std::process::Command::new("sync").status();
    }
    if sizes {
        println!(
            "{:>12} {:>12}  {:<24} PATH",
            "EXCLUSIVE", "APPARENT", "HEAD"
        );
    } else {
        println!("{:<24} PATH", "HEAD");
    }
    let mut measured_any = false;
    for w in &wts {
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
        let (excl, app) = if w.bare || !w.path.exists() {
            ("-".to_string(), "-".to_string())
        } else {
            let u = space::measure(&w.path)?;
            measured_any |= u.exclusive.is_some();
            (u.exclusive.map_or("?".into(), human), human(u.apparent))
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
