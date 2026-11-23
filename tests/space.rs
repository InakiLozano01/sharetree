#![cfg(unix)]

mod common;

use std::fs;
use std::path::Path;

use common::*;

fn big_repo(sb: &Sandbox) -> std::path::PathBuf {
    let main = sb.repo("main", &[]);
    for i in 0..16u64 {
        write(
            &main.join(format!("assets/blob{i}.bin")),
            &bytes(1000 + i, 1024 * 1024),
        );
    }
    sb.commit_all(&main, "assets");
    main
}

#[test]
fn writes_do_not_leak_between_worktrees() {
    let Some(sb) = Sandbox::reflink() else { return };
    let main = sb.repo("main", &[]);
    let cow = sb.p("cow");
    let err = sb.sharetree_ok(&main, &["add", "--detach", &cow.to_string_lossy()]);
    assert!(reflinked(&err) > 0, "{err}");
    let orig = fs::read(main.join("big.bin")).unwrap();

    let mut d = orig.clone();
    d[12345] ^= 1;
    fs::write(cow.join("big.bin"), &d).unwrap();
    assert_eq!(fs::read(main.join("big.bin")).unwrap(), orig);
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = fs::OpenOptions::new()
            .write(true)
            .open(cow.join("src/mod2/data2.bin"))
            .unwrap();
        f.seek(SeekFrom::Start(10)).unwrap();
        f.write_all(b"X").unwrap();
    }
    assert_ne!(
        fs::read(main.join("src/mod2/data2.bin")).unwrap(),
        fs::read(cow.join("src/mod2/data2.bin")).unwrap()
    );
    fs::write(main.join("bin/tool.sh"), b"changed").unwrap();
    assert_eq!(fs::read(cow.join("bin/tool.sh")).unwrap(), bytes(8, 9000));
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(
        cow.join("docs/readme.md"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    assert_eq!(
        fs::metadata(main.join("docs/readme.md"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o644
    );
    eprintln!("{err}Write and mode changes stayed private to each worktree");
}

fn exclusive(sb_path: &Path) -> Option<u64> {
    btrfs_exclusive(&content_paths(sb_path))
}

#[test]
fn btrfs_exclusive_bytes_are_near_zero() {
    let Some(sb) = Sandbox::btrfs() else { return };
    let main = big_repo(&sb);
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
    assert!(reflinked(&err) > 0, "{err}");
    let Some(stock) = exclusive(&sb.p("stock")) else {
        assert!(!require_btrfs(), "`btrfs filesystem du` is required");
        eprintln!("SKIP: `btrfs filesystem du` unavailable");
        return;
    };
    let cow = exclusive(&sb.p("cow")).expect("measure Btrfs exclusive bytes for cloned worktree");
    eprintln!("exclusive: stock={stock} cow={cow}\n{err}");
    assert!(stock > 15 * 1024 * 1024, "stock exclusive {stock}");
    assert!(cow < 256 * 1024, "cow exclusive {cow}");

    let out = sb.sharetree(&sb.p("cow"), &["du", "--raw"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let line = String::from_utf8_lossy(&out.stdout).into_owned();
    let excl: u64 = line.split('\t').nth(1).unwrap().parse().unwrap();
    assert_eq!(excl, cow, "{line}");
    let apparent: u64 = line.split('\t').next().unwrap().parse().unwrap();
    assert!(
        apparent > 15 * 1024 * 1024,
        "du should still report the full size: {line}"
    );
}

#[test]
fn reshare_recovers_sharing_after_git_rewrites() {
    let Some(sb) = Sandbox::btrfs() else { return };
    let main = big_repo(&sb);
    let stock = sb.p("stock");
    sb.git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            &stock.to_string_lossy(),
        ],
    );
    let Some(before) = exclusive(&stock) else {
        assert!(!require_btrfs(), "`btrfs filesystem du` is required");
        eprintln!("SKIP: `btrfs filesystem du` unavailable");
        return;
    };
    let snap = snapshot(&stock);
    let index_path = std::path::PathBuf::from(
        sb.git(
            &stock,
            &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        )
        .trim(),
    );
    let index_before = fs::read(&index_path).unwrap();

    for checkout in [&main, &stock] {
        for i in 0..16 {
            let path = checkout.join(format!("assets/blob{i}.bin"));
            let replacement = checkout.join("replacement");
            fs::copy(&path, &replacement).unwrap();
            fs::rename(replacement, path).unwrap();
        }
        assert!(!sb
            .git_raw(checkout, &["diff-files", "--quiet"])
            .status
            .success());
    }

    let cow = sb.p("after-move");
    let add_result = sb.sharetree_ok(&main, &["add", "--detach", &cow.to_string_lossy()]);
    assert_eq!(reflinked(&add_result), 31, "{add_result}");

    let err = sb.sharetree_ok(&stock, &["reshare", "--from", &main.to_string_lossy()]);
    assert!(err.contains("shared 31 files"), "{err}");
    let after = exclusive(&stock).unwrap();
    eprintln!("reshare: exclusive {before} -> {after}");
    assert!(before > 15 * 1024 * 1024 && after < 256 * 1024);

    assert_eq!(snapshot(&stock), snap);
    assert!(sb
        .git(
            &stock,
            &["--no-optional-locks", "status", "--porcelain", "-uno"]
        )
        .is_empty());
    let index_after = fs::read(&index_path).unwrap();
    assert_eq!(index_before, index_after);

    let err = sb.sharetree_ok(&stock, &["reshare", "--from", &main.to_string_lossy()]);
    assert!(
        err.contains("shared 0 files") && err.contains("31 already shared"),
        "{err}"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn reshare_all_inventories_each_checkout_once_and_recovers_sharing() {
    use std::os::unix::fs::PermissionsExt;

    let Some(sb) = Sandbox::btrfs() else { return };
    let main = sb.repo("main", &[]);
    let left = sb.p("left");
    let right = sb.p("right");
    let peer_blob = bytes(701, 512 * 1024);
    for (checkout, name) in [(&left, "left.bin"), (&right, "right.bin")] {
        sb.git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                checkout.to_str().unwrap(),
            ],
        );
        write(&checkout.join(name), &peer_blob);
        sb.commit_all(checkout, "different path for the same blob");
    }
    write(&main.join("untracked.bin"), &peer_blob);

    let measured = [
        left.join("big.bin"),
        right.join("big.bin"),
        right.join("right.bin"),
    ];
    let Some(before) = btrfs_exclusive(&measured) else {
        assert!(!require_btrfs(), "`btrfs filesystem du` is required");
        eprintln!("SKIP: `btrfs filesystem du` unavailable");
        return;
    };
    assert!(
        before >= 1024 * 1024,
        "exclusive bytes before reshare: {before}"
    );
    let checkouts = [&main, &left, &right];
    let snapshots = checkouts.map(|p| snapshot(p));
    let indexes = checkouts.map(|p| {
        std::path::PathBuf::from(
            sb.git(
                p,
                &["rev-parse", "--path-format=absolute", "--git-path", "index"],
            )
            .trim(),
        )
    });
    let index_contents = indexes.each_ref().map(|p| fs::read(p).unwrap());

    let shim = sb.p("git-shim");
    let git = shim.join("git");
    let calls = sb.p("git-calls");
    write(
        &git,
        br#"#!/bin/sh
case "$1" in
    ls-files|status) printf '%s\t%s\n' "$(pwd -P)" "$1" >>"$SHARETREE_TEST_GIT_CALLS" ;;
esac
export PATH="$SHARETREE_TEST_GIT_PATH"
exec git "$@"
"#,
    );
    fs::set_permissions(&git, fs::Permissions::from_mode(0o755)).unwrap();
    let original_path = std::env::var_os("PATH").unwrap();
    let patch =
        std::env::join_paths(std::iter::once(shim).chain(std::env::split_paths(&original_path)))
            .unwrap();
    let out = sb
        .sharetree_cmd(&right)
        .env("PATH", patch)
        .env("SHARETREE_TEST_GIT_PATH", original_path)
        .env("SHARETREE_TEST_GIT_CALLS", &calls)
        .args(["reshare", "--all", "--dry-run", "--min-size", "262144"])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    assert!(
        err.contains("3 candidate files") && err.contains("0 already shared"),
        "{err}"
    );
    let log = fs::read_to_string(calls).unwrap();
    let mut expected = Vec::new();
    for checkout in checkouts {
        let checkout = checkout.canonicalize().unwrap();
        for command in ["ls-files", "status"] {
            expected.push(format!("{}\t{command}", checkout.display()));
        }
    }
    let mut observed: Vec<_> = log.lines().collect();
    observed.sort_unstable();
    expected.sort_unstable();
    assert_eq!(observed, expected);
    assert_eq!(btrfs_exclusive(&measured), Some(before));

    let args = ["reshare", "--all", "--min-size", "262144"];
    let err = sb.sharetree_ok(&right, &args);
    assert!(
        err.contains("shared 3 files") && err.contains("across 3 worktree(s)"),
        "{err}"
    );
    assert_eq!(btrfs_exclusive(&measured), Some(0));
    let untracked = btrfs_exclusive(&[main.join("untracked.bin")]);
    assert!(
        untracked.is_some_and(|n| n >= peer_blob.len() as u64),
        "{untracked:?}"
    );
    assert_eq!(checkouts.map(|p| snapshot(p)), snapshots);

    let err = sb.sharetree_ok(&left, &args);
    assert!(
        err.contains("shared 0 files") && err.contains("3 already shared"),
        "{err}"
    );
    assert_eq!(
        indexes.each_ref().map(|p| fs::read(p).unwrap()),
        index_contents
    );
}

#[test]
#[cfg(target_os = "linux")]
fn reshare_all_reports_candidates_before_kernel_comparison() {
    let Some(sb) = Sandbox::btrfs() else { return };
    let main = sb.repo("main", &[]);
    let peer = sb.p("peer");
    sb.git(
        &main,
        &["worktree", "add", "-q", "--detach", peer.to_str().unwrap()],
    );
    sb.git(&peer, &["update-index", "--assume-unchanged", "big.bin"]);
    let changed = bytes(701, 300_000);
    fs::write(peer.join("big.bin"), &changed).unwrap();

    let dry = sb.sharetree_ok(&main, &["reshare", "--all", "-n", "--min-size", "262144"]);
    assert!(dry.contains("1 candidate files"), "{dry}");
    let err = sb.sharetree_ok(&main, &["reshare", "--all", "--min-size", "262144"]);
    assert!(
        err.contains("shared 0 files") && err.contains("1 differ"),
        "{err}"
    );
    assert_eq!(fs::read(main.join("big.bin")).unwrap(), bytes(100, 300_000));
    assert_eq!(fs::read(peer.join("big.bin")).unwrap(), changed);
}

#[test]
#[cfg(target_os = "linux")]
fn reshare_never_merges_different_content() {
    let Some(sb) = Sandbox::reflink() else { return };
    let main = sb.repo("main", &[]);
    let wt = sb.p("wt");
    sb.git(
        &main,
        &["worktree", "add", "-q", "--detach", &wt.to_string_lossy()],
    );
    let mut d = fs::read(wt.join("big.bin")).unwrap();
    d[0] ^= 0xff;
    fs::write(wt.join("big.bin"), &d).unwrap();
    sb.sharetree_ok(&wt, &["reshare"]);
    assert_eq!(fs::read(wt.join("big.bin")).unwrap(), d);
    assert_eq!(fs::read(main.join("big.bin")).unwrap(), bytes(100, 300_000));
}

#[test]
#[cfg(target_os = "linux")]
fn reshare_rejects_an_explicit_donor_on_a_different_filesystem() {
    let Some(donor_box) = Sandbox::tmpfs() else {
        return;
    };
    let target_box = Sandbox::home();
    let donor = donor_box.repo("donor", &[]);
    let target = target_box.repo("target", &[]);
    let out = target_box.sharetree(&target, &["reshare", "--from", donor.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("same filesystem"));
}

#[test]
#[cfg(target_os = "linux")]
fn reshare_isolates_failed_filters() {
    let Some(sb) = Sandbox::btrfs() else { return };
    let main = big_repo(&sb);
    let peer = sb.p("peer");
    sb.git(
        &main,
        &["worktree", "add", "-q", "--detach", peer.to_str().unwrap()],
    );
    write(
        &main.join(".git/info/attributes"),
        b"big.bin filter=broken\n",
    );
    sb.git(&main, &["config", "filter.broken.clean", "false"]);
    sb.git(&main, &["config", "filter.broken.required", "true"]);
    write(&peer.join("big.bin"), &bytes(100, 300_000));
    let status = sb.git_raw(
        &peer,
        &["--no-optional-locks", "status", "--porcelain", "-uno"],
    );
    assert!(!status.status.success());
    let result = sb.sharetree_ok(&peer, &["reshare", "--from", main.to_str().unwrap()]);
    assert!(result.contains("shared 30 files"), "{result}");
}

#[test]
#[cfg(target_os = "linux")]
fn reshare_all_tries_an_alternate_donor() {
    let Some(sb) = Sandbox::btrfs() else { return };
    let main = sb.repo("main", &[]);
    let a = sb.p("a");
    let b = sb.p("b");
    for peer in [&a, &b] {
        sb.git(
            &main,
            &["worktree", "add", "-q", "--detach", peer.to_str().unwrap()],
        );
    }
    sb.git(&main, &["update-index", "--assume-unchanged", "big.bin"]);
    let changed = bytes(777, 300_000);
    write(&main.join("big.bin"), &changed);
    let result = sb.sharetree_ok(&main, &["reshare", "--all", "--min-size", "262144"]);
    assert!(
        result.contains("shared 1 files") && result.contains("1 differ"),
        "{result}"
    );
    assert_eq!(fs::read(main.join("big.bin")).unwrap(), changed);
}

#[test]
#[cfg(target_os = "linux")]
fn reshare_reports_confirmed_bytes_before_a_later_mismatch() {
    let Some(sb) = Sandbox::btrfs() else { return };
    let main = sb.repo("main", &[]);
    write(&main.join("large"), &bytes(781, 17 * 1024 * 1024));
    sb.commit_all(&main, "large");
    let peer = sb.p("peer");
    sb.git(
        &main,
        &["worktree", "add", "-q", "--detach", peer.to_str().unwrap()],
    );
    sb.git(&peer, &["update-index", "--assume-unchanged", "large"]);
    let mut different = fs::read(peer.join("large")).unwrap();
    *different.last_mut().unwrap() ^= 1;
    write(&peer.join("large"), &different);
    let result = sb.sharetree_ok(
        &peer,
        &[
            "reshare",
            "--from",
            main.to_str().unwrap(),
            "--min-size",
            "1048576",
        ],
    );
    assert!(
        result.contains("shared 1 files (16.0 MiB)") && result.contains("1 differ"),
        "{result}"
    );
    assert_eq!(fs::read(peer.join("large")).unwrap(), different);
}
