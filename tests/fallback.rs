#![cfg(unix)]

mod common;

use std::fs;
use std::os::unix::fs::MetadataExt;

use common::*;

#[test]
fn tmpfs_falls_back_to_git_worktree_add() {
    let Some(sb) = Sandbox::tmpfs() else { return };
    let main = sb.repo("main", &[]);
    sb.git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            &sb.p("stock").to_string_lossy(),
        ],
    );
    let err = sb.sharetree_ok(&main, &["add", "--detach", &sb.p("cow").to_string_lossy()]);
    assert!(
        err.contains("tmpfs") && err.contains("cannot reflink"),
        "{err}"
    );
    assert!(err.contains("running plain `git worktree add`"), "{err}");
    assert_equivalent(&sb, &sb.p("stock"), &sb.p("cow"));
    eprint!("{err}");
}

#[test]
fn warm_on_tmpfs_copies_nothing() {
    let Some(sb) = Sandbox::tmpfs() else { return };
    let main = sb.repo("main", &[]);
    write(&main.join(".gitignore"), b"node_modules/\n");
    write(&main.join("node_modules/pkg/index.js"), &bytes(1, 10_000));
    sb.commit_all(&main, "ignore");
    let err = sb.sharetree_ok(
        &main,
        &["add", "--warm", "--detach", &sb.p("cow").to_string_lossy()],
    );
    assert!(err.contains("plain `git worktree add`"), "{err}");
    assert!(
        !sb.p("cow/node_modules").exists(),
        "caches must never be byte-copied"
    );
    eprint!("{err}");
}

#[test]
fn destination_on_another_filesystem_falls_back() {
    let Some(home) = Sandbox::reflink() else {
        return;
    };
    let Some(tmp) = Sandbox::tmpfs() else { return };
    let native_device = fs::metadata(home.root()).unwrap().dev();
    let tmpfs_device = fs::metadata(tmp.root()).unwrap().dev();
    assert_ne!(
        native_device, tmpfs_device,
        "fixtures must be on distinct devices"
    );
    let main = home.repo("main", &[]);
    home.git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            &tmp.p("stock").to_string_lossy(),
        ],
    );
    let err = home.sharetree_ok(&main, &["add", "--detach", &tmp.p("cow").to_string_lossy()]);
    assert!(err.contains("running plain `git worktree add`"), "{err}");
    assert_equivalent(&home, &tmp.p("stock"), &tmp.p("cow"));
    eprint!("{err}");
}

#[test]
fn no_cow_flag_and_env_delegate() {
    let sb = Sandbox::home();
    let main = sb.repo("main", &[]);
    sb.git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            &sb.p("stock").to_string_lossy(),
        ],
    );
    let err = sb.sharetree_ok(
        &main,
        &["add", "--no-cow", "--detach", &sb.p("a").to_string_lossy()],
    );
    assert!(err.contains("--no-cow"), "{err}");
    assert_equivalent(&sb, &sb.p("stock"), &sb.p("a"));
    let out = sb
        .sharetree_cmd(&main)
        .env("SHARETREE_FORCE_FALLBACK", "1")
        .args(["add", "--detach", &sb.p("b").to_string_lossy()])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_equivalent(&sb, &sb.p("stock"), &sb.p("b"));
}
