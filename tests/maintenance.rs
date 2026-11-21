//! `rm`, `ls`, `du`.

#![cfg(unix)]

mod common;

use std::time::{Duration, Instant};

use common::*;

#[test]
fn rm_checks_like_git_then_parks_and_sweeps() {
    let sb = Sandbox::home();
    let main = sb.repo("main", &[]);
    let wt = sb.p("wt");
    sb.sharetree_ok(&main, &["add", "-b", "topic", &wt.to_string_lossy()]);

    // Dirty: refused without --force.
    write(&wt.join("scratch.txt"), b"wip");
    let out = sb.sharetree(&main, &["rm", &wt.to_string_lossy()]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--force"));
    assert!(wt.exists());

    // cwd inside: refused even with --force.
    let out = sb.sharetree(&wt.join("src"), &["rm", "-f", &wt.to_string_lossy()]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("current directory"));

    // Main worktree: refused.
    assert!(!sb
        .sharetree(&wt, &["rm", "-f", &main.to_string_lossy()])
        .status
        .success());

    sb.sharetree_ok(&main, &["rm", "-f", &wt.to_string_lossy()]);
    assert!(!wt.exists());
    assert!(!sb
        .git(&main, &["worktree", "list", "--porcelain"])
        .contains(&*wt.to_string_lossy()));
    // The branch survives, as with `git worktree remove`.
    sb.git(&main, &["rev-parse", "--verify", "topic"]);
    // The background sweep empties the trash.
    let trash = main.join(".git/sharetree-trash");
    let t = Instant::now();
    while trash.exists() && t.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!trash.exists(), "trash not swept");
    // The name is free again.
    sb.sharetree_ok(&main, &["add", "--detach", &wt.to_string_lossy()]);
}

#[test]
fn rm_sync_and_locked() {
    let sb = Sandbox::home();
    let main = sb.repo("main", &[]);
    let wt = sb.p("wt");
    sb.sharetree_ok(&main, &["add", "--detach", "--lock", &wt.to_string_lossy()]);
    let out = sb.sharetree(&main, &["rm", "-f", &wt.to_string_lossy()]);
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("locked"));
    sb.sharetree_ok(&main, &["rm", "-f", "-f", "--sync", &wt.to_string_lossy()]);
    assert!(!wt.exists());
    assert!(
        !main.join(".git/sharetree-trash").exists()
            || std::fs::read_dir(main.join(".git/sharetree-trash"))
                .unwrap()
                .next()
                .is_none()
    );
    assert!(sb.git(&main, &["worktree", "list"]).lines().count() == 1);
}

#[test]
fn sweep_refuses_paths_outside_trash() {
    let sb = Sandbox::home();
    let victim = sb.p("keep/me");
    write(&victim.join("f"), b"x");
    let out = sb.sharetree(sb.root(), &["__sweep", &victim.to_string_lossy()]);
    assert!(!out.status.success());
    assert!(victim.join("f").exists());
}

#[test]
fn ls_keeps_missing_worktrees_and_du_all_skips_them() {
    let sb = Sandbox::home();
    let main = sb.repo("main", &[]);
    let missing = sb.p("missing");
    sb.git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            &missing.to_string_lossy(),
        ],
    );
    std::fs::remove_dir_all(&missing).unwrap();

    let out = sb.sharetree(&main, &["list", "--no-size"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.starts_with("HEAD"), "{text}");
    assert!(text.contains(&*missing.to_string_lossy()), "{text}");

    let out = sb.sharetree(&main, &["ls"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    let missing_row = text
        .lines()
        .find(|line| line.contains(&*missing.to_string_lossy()))
        .unwrap();
    assert!(missing_row.split_whitespace().take(2).eq(["-", "-"]));

    let out = sb.sharetree(&main, &["du", "--all", "--raw"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    let mut rows = text.lines();
    let row = rows.next().unwrap();
    assert!(
        rows.next().is_none(),
        "expected only the main worktree: {text}"
    );
    let mut columns = row.splitn(3, '\t');
    assert!(columns.next().unwrap().parse::<u64>().is_ok(), "{row}");
    assert_eq!(columns.nth(1), Some(main.to_string_lossy().as_ref()));
}

#[test]
fn ls_and_du_report_exclusive_bytes() {
    let Some(sb) = Sandbox::reflink() else { return };
    let main = sb.repo("main", &[]);
    sb.sharetree_ok(&main, &["add", "-b", "cow", &sb.p("cow").to_string_lossy()]);
    sb.git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "stock",
            &sb.p("stock").to_string_lossy(),
        ],
    );
    let out = sb.sharetree(&main, &["ls"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    eprintln!("{text}");
    assert_eq!(
        text.lines()
            .filter(|l| l.contains(&*sb.root().to_string_lossy()))
            .count(),
        3
    );
    assert!(text.contains("[main]"));
    let row = |name: &str| {
        text.lines()
            .find(|l| l.trim_end().ends_with(&format!("/{name}")))
            .unwrap()
            .to_string()
    };
    if fs_type(sb.root()) == "btrfs" && btrfs_exclusive(&[sb.p("cow")]).is_some() {
        // ~330 KiB of tracked data: the stock tree owns it, the cow one does not.
        assert!(
            row("cow").trim_start().starts_with("0 B") || row("cow").contains(" KiB"),
            "{}",
            row("cow")
        );
        assert!(
            !row("stock").trim_start().starts_with("0 B"),
            "{}",
            row("stock")
        );
    }
    let out = sb.sharetree(&main, &["du", "--all"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("EXCLUSIVE") && text.contains("cannot see that reflinked"),
        "{text}"
    );
    assert!(sb.sharetree(&main, &["ls", "--no-size"]).status.success());
}
