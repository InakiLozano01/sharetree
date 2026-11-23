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
    let alias_hop = sb.p("alias-hop");
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
    symlink("alias-hop", &alias).unwrap();
    symlink("in-volume", &alias_hop).unwrap();
    fs::rename(&tree, sb.p("volume-offline")).unwrap();

    let attempt = sb.sharetree(&repo, &["rm", &alias.to_string_lossy()]);
    assert!(!attempt.status.success());
    assert!(String::from_utf8_lossy(&attempt.stderr).contains("not mounted"));
    assert!(alias.symlink_metadata().is_ok());

    sb.sharetree_ok(&repo, &["rm", "-ff", &alias.to_string_lossy()]);
    assert!(alias.symlink_metadata().is_err());
    assert_eq!(
        fs::read_link(alias_hop).unwrap(),
        std::path::Path::new("in-volume")
    );
    assert!(sb.p("volume-offline/.git").is_file());
    assert_eq!(sb.git(&repo, &["worktree", "list"]).lines().count(), 1);
}

#[test]
fn resumes_a_worktree_removal_interrupted_after_parking() {
    let sb = Sandbox::home();
    let repo = sb.repo("main", &[]);
    let tree = sb.p("checkout");
    let trash = repo.join(".git/sharetree-trash/checkout-interrupted");
    let added = sb.git_raw(
        &repo,
        &[
            "worktree",
            "add",
            "--relative-paths",
            "--detach",
            &tree.to_string_lossy(),
        ],
    );
    if !added.status.success() {
        assert!(String::from_utf8_lossy(&added.stderr).contains("unknown option"));
        sb.git(
            &repo,
            &["worktree", "add", "--detach", &tree.to_string_lossy()],
        );
    }
    fs::write(
        tree.join(".git"),
        "gitdir: ./../main/.git/worktrees/checkout\r\n",
    )
    .unwrap();
    fs::create_dir_all(trash.parent().unwrap()).unwrap();
    fs::rename(&tree, &trash).unwrap();

    sb.sharetree_ok(&repo, &["rm", "--sync", &tree.to_string_lossy()]);
    assert!(!trash.parent().unwrap().exists());
    assert_eq!(sb.git(&repo, &["worktree", "list"]).lines().count(), 1);
}

#[test]
fn rm_preserves_a_checkout_whose_registration_is_inconsistent() {
    let sb = Sandbox::home();
    let repo = sb.repo("main", &[]);
    let checkout = sb.p("checkout");
    sb.git(
        &repo,
        &["worktree", "add", "--detach", &checkout.to_string_lossy()],
    );
    let admin = repo.join(".git/worktrees/checkout");
    let original = fs::read(admin.join("commondir")).unwrap();
    fs::write(admin.join("commondir"), "../missing\n").unwrap();
    let registration = sb.git(&repo, &["worktree", "list", "--porcelain"]);

    let attempt = sb.sharetree(&repo, &["rm", "-ff", "--sync", &checkout.to_string_lossy()]);
    assert!(!attempt.status.success());
    assert!(checkout.join("big.bin").is_file());
    assert!(admin.join("HEAD").is_file());
    assert_eq!(
        sb.git(&repo, &["worktree", "list", "--porcelain"]),
        registration
    );
    assert!(!repo.join(".git/sharetree-trash").exists());

    fs::write(admin.join("commondir"), original).unwrap();
    sb.sharetree_ok(&repo, &["rm", "--sync", &checkout.to_string_lossy()]);
    assert!(!checkout.exists());
}

#[test]
fn rm_preserves_both_worktrees_when_the_checkout_points_to_the_wrong_admin() {
    let sb = Sandbox::home();
    let repo = sb.repo("main", &[]);
    let checkout = sb.p("checkout");
    let other = sb.p("other");
    for path in [&checkout, &other] {
        sb.git(
            &repo,
            &["worktree", "add", "--detach", &path.to_string_lossy()],
        );
    }
    fs::copy(other.join(".git"), checkout.join(".git")).unwrap();
    let registrations = sb.git(&repo, &["worktree", "list", "--porcelain"]);

    let attempt = sb.sharetree(&repo, &["rm", "-ff", "--sync", &checkout.to_string_lossy()]);
    assert!(!attempt.status.success());
    assert!(String::from_utf8_lossy(&attempt.stderr).contains("does not point back"));
    assert!(checkout.join("big.bin").is_file());
    assert!(other.join("big.bin").is_file());
    assert_eq!(
        sb.git(&repo, &["worktree", "list", "--porcelain"]),
        registrations
    );
    assert!(!repo.join(".git/sharetree-trash").exists());
}

#[test]
fn rm_works_when_an_unrelated_registration_has_a_file_as_its_parent() {
    let sb = Sandbox::home();
    let repo = sb.repo("main", &[]);
    let stale = sb.p("stale/old-checkout");
    let checkout = sb.p("checkout");
    for path in [&stale, &checkout] {
        sb.git(
            &repo,
            &["worktree", "add", "--detach", &path.to_string_lossy()],
        );
    }
    fs::rename(sb.p("stale"), sb.p("saved")).unwrap();
    fs::write(sb.p("stale"), b"keep").unwrap();

    sb.sharetree_ok(&repo, &["rm", "--sync", &checkout.to_string_lossy()]);
    assert!(!checkout.exists());
    assert!(sb.p("saved/old-checkout/big.bin").is_file());
    assert!(repo.join(".git/worktrees/old-checkout").is_dir());
    assert_eq!(fs::read(sb.p("stale")).unwrap(), b"keep");
}

#[test]
fn rm_resolves_parent_components_after_symlinks() {
    let sb = Sandbox::home();
    let repo = sb.repo("main", &[]);
    let right = sb.p("physical/checkout ");
    let other = repo.join("checkout ");
    fs::create_dir_all(sb.p("physical/subdir")).unwrap();
    symlink(sb.p("physical/subdir"), repo.join("link")).unwrap();
    for checkout in [&right, &other] {
        sb.git(
            &repo,
            &["worktree", "add", "--detach", &checkout.to_string_lossy()],
        );
    }

    sb.sharetree_ok(&repo, &["rm", "-f", "--sync", "link/../checkout "]);
    assert!(!right.exists());
    assert!(other.join("big.bin").is_file());
    assert!(repo.join("link").symlink_metadata().unwrap().is_symlink());
}
