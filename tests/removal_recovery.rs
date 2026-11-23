#![cfg(unix)]

mod common;

use std::{fs, os::unix::fs::symlink};

use common::*;

#[test]
fn missing_volume_requires_double_force_and_removes_the_dangling_alias() {
    let sb = Sandbox::home();
    let repo = sb.repo("main", &[]);
    let tree = sb.p("in-volume");
    let alias = sb.p("alias");
    sb.git(
        &repo,
        &[
            "worktree",
            "add",
            "--detach",
            "--lock",
            "--reason",
            "sharetree volume 'default': kept registered while the volume is unmounted",
            &tree.to_string_lossy(),
        ],
    );
    symlink("in-volume", &alias).unwrap();
    fs::rename(&tree, sb.p("volume-offline")).unwrap();

    let attempt = sb.sharetree(&repo, &["rm", &alias.to_string_lossy()]);
    assert!(!attempt.status.success());
    assert!(String::from_utf8_lossy(&attempt.stderr).contains("not mounted"));
    assert!(alias.symlink_metadata().is_ok());

    sb.sharetree_ok(&repo, &["rm", "-ff", &alias.to_string_lossy()]);
    assert!(alias.symlink_metadata().is_err());
    assert!(sb.p("volume-offline/.git").is_file());
    assert_eq!(sb.git(&repo, &["worktree", "list"]).lines().count(), 1);
}

#[test]
fn resumes_a_worktree_removal_interrupted_after_parking() {
    let sb = Sandbox::home();
    let repo = sb.repo("main", &[]);
    let tree = sb.p("checkout");
    let trash = repo.join(".git/sharetree-trash/checkout-interrupted");
    sb.git(
        &repo,
        &["worktree", "add", "--detach", &tree.to_string_lossy()],
    );
    fs::create_dir_all(trash.parent().unwrap()).unwrap();
    fs::rename(&tree, &trash).unwrap();

    sb.sharetree_ok(&repo, &["rm", "--sync", &tree.to_string_lossy()]);
    assert!(!trash.parent().unwrap().exists());
    assert_eq!(sb.git(&repo, &["worktree", "list"]).lines().count(), 1);
}
