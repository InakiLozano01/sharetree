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
