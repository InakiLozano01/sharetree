#![cfg(unix)]

mod common;

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use common::*;

fn compare_at(sb: &Sandbox, from: &Path, rev: &str, stock: &str, cow: &str) -> String {
    let err = sb.sharetree_ok(
        from,
        &["add", "--detach", &sb.p(cow).to_string_lossy(), rev],
    );
    sb.git(
        from,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            &sb.p(stock).to_string_lossy(),
            rev,
        ],
    );
    assert_equivalent(sb, &sb.p(stock), &sb.p(cow));
    err
}

#[test]
fn identity() {
    let sb = if std::env::var("SHARETREE_REQUIRE_APFS").as_deref() == Ok("1") {
        let sb = Sandbox::home();
        let root = sb.root();
        assert_eq!(
            fs_type(root),
            "apfs",
            "SHARETREE_REQUIRE_APFS=1 requires the CLI identity fixture at {} to be on APFS",
            root.display()
        );
        sb
    } else {
        let Some(sb) = Sandbox::reflink() else { return };
        sb
    };
    let main = sb.repo("main", &[]);
    let err = compare_at(&sb, &main, "HEAD", "stock", "cow");
    assert_eq!(reflinked(&err), 15, "{err}");
}

#[test]
fn donor_one_commit_behind() {
    let Some(sb) = Sandbox::reflink() else { return };
    let main = sb.repo("main", &[]);
    let before = sb.git(&main, &["rev-parse", "HEAD"]).trim().to_string();
    write(&main.join("src/mod3/data3.bin"), &bytes(999, 20_000));
    fs::remove_file(main.join("src/mod4/data4.bin")).unwrap();
    write(&main.join("new/file.bin"), &bytes(77, 12_000));
    fs::rename(main.join("src/mod5/data5.bin"), main.join("moved.bin")).unwrap();
    let after = sb.commit_all(&main, "second");
    sb.git(&main, &["checkout", "-q", "--detach", &before]);
    let err = compare_at(&sb, &main, &after, "stock", "cow");
    assert_eq!(reflinked(&err), 13, "{err}");
}

#[test]
fn donor_reuses_blobs_when_all_paths_are_renamed() {
    let Some(sb) = Sandbox::reflink() else { return };
    let main = sb.repo("main", &[]);
    let before = sb.git(&main, &["rev-parse", "HEAD"]).trim().to_string();
    fs::create_dir(main.join("renamed")).unwrap();
    for entry in fs::read_dir(&main).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if name != ".git" && name != "renamed" {
            fs::rename(entry.path(), main.join("renamed").join(name)).unwrap();
        }
    }
    let after = sb.commit_all(&main, "move all paths");
    sb.git(&main, &["checkout", "-q", "--detach", &before]);
    let err = compare_at(&sb, &main, &after, "stock", "cow");
    assert_eq!(reflinked(&err), 15, "{err}");
}

#[test]
fn ranks_donors_after_excluding_locally_modified_files() {
    let Some(sb) = Sandbox::reflink() else { return };
    let main = sb.repo("main", &[]);
    let clean = sb.p("clean");
    sb.git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            &clean.to_string_lossy(),
        ],
    );
    write(&main.join("big.bin"), &bytes(500, 300_000));
    let err = sb.sharetree_ok(&main, &["add", "--detach", &sb.p("cow").to_string_lossy()]);
    assert_eq!(reflinked(&err), 15, "{err}");
    assert!(err.contains(&format!("from {}", clean.display())), "{err}");
    assert_equivalent(&sb, &clean, &sb.p("cow"));
}

#[test]
fn ranks_a_clean_fourth_donor_above_three_dirty_donors() {
    let Some(sb) = Sandbox::reflink() else { return };
    let main = sb.repo("main", &[]);
    write(&main.join("big.bin"), &bytes(120, 4 * 1024 * 1024));
    write(&main.join("medium-a.bin"), &bytes(121, 300_000));
    write(&main.join("medium-b.bin"), &bytes(122, 300_000));
    sb.commit_all(&main, "add three large files");
    for path in ["../one", "../two", "../clean"] {
        sb.git(&main, &["worktree", "add", "-q", "--detach", path]);
    }
    let clean = sb.p("clean");
    sb.git(&clean, &["rm", "-q", "big.bin"]);
    sb.git(&clean, &["commit", "-qm", "remove the largest file"]);
    for donor in [&main, &sb.p("one"), &sb.p("two")] {
        for path in ["big.bin", "medium-a.bin", "medium-b.bin"] {
            write(&donor.join(path), b"local edit\n");
        }
    }

    let err = sb.sharetree_ok(&main, &["add", "--min-size=200000", "--detach", "../cow"]);
    assert_eq!(reflinked(&err), 2, "{err}");
    assert!(err.contains(&format!("from {}", clean.display())), "{err}");
    sb.git(&main, &["worktree", "add", "-q", "--detach", "../stock"]);
    assert_equivalent(&sb, &sb.p("stock"), &sb.p("cow"));
}

#[test]
fn ranks_donors_without_counting_target_checkout_conversions() {
    let Some(sb) = Sandbox::reflink() else { return };
    let main = sb.repo("main", &[]);
    write(&main.join("big.bin"), &b"$Id$\n".repeat(100_000));
    write(&main.join("medium-a.bin"), &bytes(130, 240_000));
    write(&main.join("medium-b.bin"), &bytes(131, 240_000));
    write(&main.join(".gitattributes"), b"big.bin ident\n");
    sb.commit_all(&main, "convert the largest blob at checkout");
    let clean = sb.p("clean");
    sb.git(&main, &["worktree", "add", "-q", "--detach", "../clean"]);
    sb.git(&clean, &["rm", "-q", "big.bin", "empty", "small.txt"]);
    sb.git(&clean, &["commit", "-qm", "remove the converted file"]);
    for path in ["medium-a.bin", "medium-b.bin"] {
        write(&main.join(path), b"local edit\n");
    }

    let err = sb.sharetree_ok(&main, &["add", "--min-size=200000", "--detach", "../cow"]);
    assert_eq!(reflinked(&err), 2, "{err}");
    assert!(err.contains(&format!("from {}", clean.display())), "{err}");
    sb.git(&main, &["worktree", "add", "-q", "--detach", "../stock"]);
    assert_equivalent(&sb, &sb.p("stock"), &sb.p("cow"));
}

#[test]
fn ranks_donors_without_counting_paths_excluded_by_the_destination() {
    let Some(sb) = Sandbox::reflink() else { return };
    let main = sb.repo("main", &[]);
    write(&main.join("omit/large.bin"), &bytes(140, 800_000));
    write(&main.join("keep/medium-a.bin"), &bytes(141, 240_000));
    write(&main.join("keep/medium-b.bin"), &bytes(142, 240_000));
    sb.commit_all(&main, "add files inside and outside the sparse checkout");
    sb.git(&main, &["worktree", "add", "-q", "--detach", "../dense"]);
    sb.git(&main, &["worktree", "add", "-q", "--detach", "../clean"]);
    let dense = sb.p("dense");
    sb.git(&dense, &["rm", "-qr", "keep"]);
    sb.git(&dense, &["commit", "-qm", "remove the included files"]);
    let clean = sb.p("clean");
    sb.git(
        &clean,
        &["rm", "-q", "omit/large.bin", "empty", "small.txt"],
    );
    sb.git(&clean, &["commit", "-qm", "remove excluded files"]);
    sb.git(&main, &["sparse-checkout", "set", "--no-cone", "/keep/"]);
    for path in ["keep/medium-a.bin", "keep/medium-b.bin"] {
        write(&main.join(path), b"local edit\n");
    }
    assert!(dense.join("omit/large.bin").is_file());

    let err = compare_at(&sb, &main, "HEAD", "stock", "cow");
    assert_eq!(reflinked(&err), 2, "{err}");
    assert!(err.contains(&format!("from {}", clean.display())), "{err}");
    assert!(!sb.p("cow/omit").exists());
}

#[test]
fn ranks_donors_by_eligible_bytes() {
    let Some(sb) = Sandbox::reflink() else { return };
    let main = sb.repo("main", &[]);
    write(&main.join("medium-a.bin"), &bytes(150, 120_000));
    write(&main.join("medium-b.bin"), &bytes(151, 120_000));
    sb.commit_all(&main, "add two medium files");
    let large = sb.p("large");
    sb.git(&main, &["worktree", "add", "-q", "--detach", "../large"]);
    sb.git(&large, &["rm", "-q", "medium-a.bin", "medium-b.bin"]);
    sb.git(&large, &["commit", "-qm", "remove the medium files"]);
    write(&main.join("big.bin"), b"local edit\n");

    let err = sb.sharetree_ok(&main, &["add", "--min-size=100000", "--detach", "../cow"]);
    assert_eq!(reflinked(&err), 1, "{err}");
    assert!(err.contains(&format!("from {}", large.display())), "{err}");
    sb.git(&main, &["worktree", "add", "-q", "--detach", "../stock"]);
    assert_equivalent(&sb, &sb.p("stock"), &sb.p("cow"));
}

#[test]
fn stat_clean_but_modified_donor() {
    let Some(sb) = Sandbox::reflink() else { return };
    let main = sb.repo("main", &[]);
    sb.git(&main, &["config", "core.trustctime", "false"]);
    sb.git(&main, &["config", "core.checkStat", "minimal"]);
    let f = main.join("big.bin");
    let mtime = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
    fs::File::options()
        .write(true)
        .open(&f)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(mtime))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    sb.git(&main, &["update-index", "--refresh"]);
    let mut data = fs::read(&f).unwrap();
    data[1000] ^= 0xff;
    fs::write(&f, &data).unwrap();
    fs::File::options()
        .write(true)
        .open(&f)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(mtime))
        .unwrap();
    assert_eq!(
        sb.git(&main, &["diff-index", "--name-only", "HEAD"]),
        "",
        "donor must look clean"
    );
    let err = compare_at(&sb, &main, "HEAD", "stock", "cow");
    assert!(err.contains("1 clones failed verification"), "{err}");
    assert_eq!(reflinked(&err), 14, "{err}");
}

#[test]
fn dirty_donor() {
    let Some(sb) = Sandbox::reflink() else { return };
    let main = sb.repo("main", &[]);
    write(&main.join("src/mod0/data0.bin"), &bytes(5, 7000));
    fs::remove_file(main.join("src/mod1/data1.bin")).unwrap();
    fs::set_permissions(main.join("big.bin"), fs::Permissions::from_mode(0o755)).unwrap();
    write(&main.join("untracked.bin"), &bytes(6, 9000));
    let err = compare_at(&sb, &main, "HEAD", "stock", "cow");
    assert_eq!(reflinked(&err), 12, "{err}");
    assert!(!sb.p("cow/untracked.bin").exists());
}

#[test]
fn exec_bit_change() {
    let Some(sb) = Sandbox::reflink() else { return };
    let main = sb.repo("main", &[]);
    let before = sb.git(&main, &["rev-parse", "HEAD"]).trim().to_string();
    fs::set_permissions(main.join("big.bin"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(main.join("bin/tool.sh"), fs::Permissions::from_mode(0o644)).unwrap();
    let after = sb.commit_all(&main, "modes");
    sb.git(&main, &["checkout", "-q", "--detach", &before]);
    let err = compare_at(&sb, &main, &after, "stock", "cow");
    assert_eq!(reflinked(&err), 15, "{err}");
    let m = |p: &str| fs::metadata(sb.p(p)).unwrap().permissions().mode() & 0o777;
    assert_eq!(m("cow/big.bin"), m("stock/big.bin"));
    assert_ne!(m("cow/big.bin") & 0o100, 0);
}

#[test]
fn symlinks_are_written_by_git() {
    let Some(sb) = Sandbox::reflink() else { return };
    let main = sb.repo("main", &[]);
    compare_at(&sb, &main, "HEAD", "stock", "cow");
    assert_eq!(
        fs::read_link(sb.p("cow/link-to-big")).unwrap(),
        Path::new("big.bin")
    );
    assert_eq!(
        fs::read_link(sb.p("cow/docs/link-dir")).unwrap(),
        Path::new("../src/mod1")
    );
}

#[test]
fn gitlinks() {
    let Some(sb) = Sandbox::reflink() else { return };
    let main = sb.repo("main", &[]);
    let head = sb.git(&main, &["rev-parse", "HEAD"]).trim().to_string();
    sb.git(
        &main,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{head},vendor/sub"),
        ],
    );
    sb.git(&main, &["commit", "-qm", "gitlink"]);
    compare_at(&sb, &main, "HEAD", "stock", "cow");
    assert!(sb.p("cow/vendor/sub").is_dir());
}

#[test]
fn sparse_checkout() {
    let Some(sb) = Sandbox::reflink() else { return };
    let main = sb.repo("main", &[]);
    sb.git(
        &main,
        &["sparse-checkout", "set", "--cone", "src/mod2", "bin"],
    );
    let err = compare_at(&sb, &main, "HEAD", "stock", "cow");
    assert!(!sb.p("cow/src/mod3").exists());
    assert!(sb.p("cow/src/mod2/data2.bin").exists());
    assert_eq!(reflinked(&err), 3, "{err}");
}

#[test]
fn crlf_and_text_attributes() {
    let Some(sb) = Sandbox::reflink() else { return };
    let main = sb.repo("main", &[]);
    let text: Vec<u8> = (0..400)
        .flat_map(|i| format!("line {i} of some text\n").into_bytes())
        .collect();
    write(&main.join("t/crlf.txt"), &text);
    write(&main.join("t/auto.txt"), &text);
    write(&main.join("t/plain.txt"), &text);
    write(
        &main.join(".gitattributes"),
        b"crlf.txt text eol=crlf\nauto.txt text=auto\n",
    );
    sb.commit_all(&main, "attrs");
    let err = compare_at(&sb, &main, "HEAD", "stock", "cow");
    assert!(fs::read(sb.p("cow/t/crlf.txt"))
        .unwrap()
        .windows(2)
        .any(|w| w == b"\r\n"));
    assert_eq!(reflinked(&err), 17, "{err}");
}

#[test]
fn autocrlf_true_delegates_text_files() {
    let Some(sb) = Sandbox::reflink() else { return };
    let main = sb.repo("main", &[]);
    let text: Vec<u8> = (0..400)
        .flat_map(|i| format!("line {i}\n").into_bytes())
        .collect();
    write(&main.join("notes.txt"), &text);
    sb.commit_all(&main, "text");
    sb.git(&main, &["config", "core.autocrlf", "true"]);
    compare_at(&sb, &main, "HEAD", "stock", "cow");
    assert!(fs::read(sb.p("cow/notes.txt"))
        .unwrap()
        .windows(2)
        .any(|w| w == b"\r\n"));
}

#[test]
fn smudge_filter_paths_are_left_to_git() {
    let Some(sb) = Sandbox::reflink() else { return };
    let main = sb.repo("main", &[]);
    sb.git(&main, &["config", "filter.upper.smudge", "tr a-z A-Z"]);
    sb.git(&main, &["config", "filter.upper.clean", "tr A-Z a-z"]);
    let text: Vec<u8> = (0..500)
        .flat_map(|i| format!("lower case {i}\n").into_bytes())
        .collect();
    write(
        &main.join(".gitattributes"),
        b"*.up filter=upper\n*.po filter=gettext-no-location\n",
    );
    write(&main.join("x.up"), &text);
    write(&main.join("x.po"), &text);
    sb.commit_all(&main, "filter");
    let err = compare_at(&sb, &main, "HEAD", "stock", "cow");
    assert!(fs::read_to_string(sb.p("cow/x.up"))
        .unwrap()
        .starts_with("LOWER CASE 0"));
    assert_eq!(
        reflinked(&err),
        16,
        "x.po reused, x.up written by git: {err}"
    );
}

#[test]
fn sha256_repository() {
    let Some(sb) = Sandbox::reflink() else { return };
    let main = sb.repo("main", &["--object-format=sha256"]);
    let err = compare_at(&sb, &main, "HEAD", "stock", "cow");
    assert_eq!(reflinked(&err), 15, "{err}");
    assert_eq!(
        sb.git(&sb.p("cow"), &["rev-parse", "HEAD"]).trim().len(),
        64
    );
}

#[test]
fn non_utf8_and_awkward_names() {
    let Some(sb) = Sandbox::reflink() else { return };
    let main = sb.repo("main", &[]);
    let names: &[&[u8]] = &[
        #[cfg(not(target_os = "macos"))]
        b"caf\xe9.bin",
        b"with\nnewline.bin",
        b"\"quoted\".bin",
        b"back\\slash.bin",
        b"sp ace.bin",
        b"tab\there.bin",
    ];
    for (i, n) in names.iter().enumerate() {
        write(
            &main.join(OsStr::from_bytes(n)),
            &bytes(500 + i as u64, 5000),
        );
    }
    sb.commit_all(&main, "names");
    let err = compare_at(&sb, &main, "HEAD", "stock", "cow");
    assert_eq!(reflinked(&err), 15 + names.len(), "{err}");
}

#[test]
fn destination_inside_source() {
    let Some(sb) = Sandbox::reflink() else { return };
    let main = sb.repo("main", &[]);
    sb.git(
        &main,
        &["worktree", "add", "-q", "--detach", "nested/stock"],
    );
    let err = sb.sharetree_ok(&main, &["add", "--detach", "nested/cow"]);
    assert_eq!(reflinked(&err), 15, "{err}");
    assert_equivalent(&sb, &main.join("nested/stock"), &main.join("nested/cow"));
    assert!(!main.join("nested/cow/nested").exists());
}

#[test]
fn destination_resolves_parent_after_symlink() {
    let Some(sb) = Sandbox::reflink() else { return };
    let main = sb.repo("main", &[]);
    fs::create_dir_all(sb.p("real/child")).unwrap();
    std::os::unix::fs::symlink(sb.p("real/child"), sb.p("alias")).unwrap();
    let stock = sb.p("alias/../stock");
    let cow = sb.p("alias/../cow");
    let err = sb.sharetree_ok(&main, &["add", "--detach", &cow.to_string_lossy()]);
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
    assert_eq!(reflinked(&err), 15, "{err}");
    assert_equivalent(&sb, &sb.p("real/stock"), &sb.p("real/cow"));
}

#[test]
fn new_branch_and_dwim_branch() {
    let Some(sb) = Sandbox::reflink() else { return };
    let main = sb.repo("main", &[]);
    sb.git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "s1",
            &sb.p("stock").to_string_lossy(),
        ],
    );
    let err = sb.sharetree_ok(&main, &["add", "-b", "c1", &sb.p("cow").to_string_lossy()]);
    assert_eq!(reflinked(&err), 15, "{err}");
    assert_equivalent(&sb, &sb.p("stock"), &sb.p("cow"));
    assert_eq!(
        sb.git(&sb.p("cow"), &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/c1"
    );
    sb.sharetree_ok(&main, &["add", &sb.p("feature").to_string_lossy()]);
    assert_eq!(
        sb.git(&sb.p("feature"), &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/feature"
    );
    assert_equivalent(&sb, &sb.p("stock"), &sb.p("feature"));
}

#[test]
fn bundled_short_options_preserve_branch_arguments_and_quiet() {
    let sb = Sandbox::home();
    let main = sb.repo("main", &[]);

    for (branch, options, no_cow) in [
        ("separate", &["-qb", "separate"][..], false),
        ("attached", &["-fqBattached"][..], false),
        ("fallback", &["-qB", "fallback"][..], true),
    ] {
        let dest = sb.p(branch);
        let dest_arg = dest.to_string_lossy();
        let mut args = vec!["add"];
        if no_cow {
            args.push("--no-cow");
        }
        args.extend_from_slice(options);
        args.extend([dest_arg.as_ref(), "HEAD"]);

        let err = sb.sharetree_ok(&main, &args);
        assert!(err.is_empty(), "{err}");
        assert_eq!(
            sb.git(&dest, &["symbolic-ref", "HEAD"]).trim(),
            format!("refs/heads/{branch}")
        );
        assert_equivalent(&sb, &main, &dest);
    }
}

#[test]
fn donor_is_another_linked_worktree() {
    let Some(sb) = Sandbox::reflink() else { return };
    let main = sb.repo("main", &[]);
    let w1 = sb.p("w1");
    sb.sharetree_ok(&main, &["add", "--detach", &w1.to_string_lossy()]);
    write(&w1.join("src/mod0/data0.bin"), &bytes(31, 6000));
    sb.git(&w1, &["commit", "-qam", "on w1"]);
    let rev = sb.git(&w1, &["rev-parse", "HEAD"]).trim().to_string();
    sb.git(
        &w1,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            &sb.p("stock").to_string_lossy(),
            &rev,
        ],
    );
    let err = sb.sharetree_ok(
        &w1,
        &[
            "add",
            "-v",
            "--detach",
            &sb.p("cow").to_string_lossy(),
            &rev,
        ],
    );
    assert!(err.contains(&format!("from {}", w1.display())), "{err}");
    assert_eq!(reflinked(&err), 15, "{err}");
    assert_equivalent(&sb, &sb.p("stock"), &sb.p("cow"));
}

#[test]
fn concurrent_creates() {
    let Some(sb) = Sandbox::reflink() else { return };
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
    let children: Vec<_> = (0..4)
        .map(|i| {
            sb.sharetree_cmd(&main)
                .args(["add", "--detach", &sb.p(&format!("c{i}")).to_string_lossy()])
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for (i, c) in children.into_iter().enumerate() {
        let out = c.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_equivalent(&sb, &sb.p("stock"), &sb.p(&format!("c{i}")));
    }
}

#[test]
fn rejected_seed_index_falls_back_to_plain_checkout() {
    let Some(sb) = Sandbox::reflink() else { return };
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
    let shim = sb.p("git-shim");
    let git = shim.join("git");
    let marker = sb.p("index-corrupted");
    write(
        &git,
        br#"#!/bin/sh
set -eu
export PATH="$SHARETREE_TEST_GIT_PATH"
for arg do
    if [ "$arg" = reset ] && [ ! -e "$SHARETREE_TEST_MARKER" ]; then
        index=$(git rev-parse --path-format=absolute --git-path index)
        test -s "$index"
        printf 'invalid index' >"$index"
        : >"$SHARETREE_TEST_MARKER"
        break
    fi
done
exec git "$@"
"#,
    );
    fs::set_permissions(&git, fs::Permissions::from_mode(0o755)).unwrap();
    let original_path = std::env::var_os("PATH").unwrap();
    let patched_path =
        std::env::join_paths(std::iter::once(shim).chain(std::env::split_paths(&original_path)))
            .unwrap();
    let out = sb
        .sharetree_cmd(&main)
        .env("PATH", patched_path)
        .env("SHARETREE_TEST_GIT_PATH", original_path)
        .env("SHARETREE_TEST_MARKER", &marker)
        .args(["add", "--detach", &sb.p("cow").to_string_lossy()])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    assert!(marker.is_file(), "{err}");
    assert!(err.contains("retrying a plain checkout"), "{err}");
    assert!(err.contains("reflinked 0 of"), "{err}");
    assert_equivalent(&sb, &sb.p("stock"), &sb.p("cow"));
}

fn install_hook(repo: &Path, name: &str, body: &str) {
    let h = repo.join(".git/hooks").join(name);
    fs::create_dir_all(h.parent().unwrap()).unwrap();
    fs::write(&h, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&h, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn post_checkout_hook_gets_stock_arguments() {
    let Some(sb) = Sandbox::reflink() else { return };
    let main = sb.repo("main", &[]);
    let log = sb.p("hook.log");
    install_hook(
        &main,
        "post-checkout",
        &format!("echo \"$*\" >> '{}'", log.display()),
    );
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
    sb.sharetree_ok(&main, &["add", "--detach", &sb.p("cow").to_string_lossy()]);
    let lines: Vec<String> = fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0], lines[1]);
    assert!(
        lines[0].starts_with("0000000000000000000000000000000000000000 ")
            && lines[0].ends_with(" 1")
    );
}

#[test]
fn failed_checkout_removes_locked_worktree_and_keeps_branch() {
    let Some(sb) = Sandbox::reflink() else { return };
    let main = sb.repo("main", &[]);
    write(&main.join(".gitattributes"), b"big.bin filter=broken\n");
    sb.commit_all(&main, "require a filter");
    sb.git(&main, &["config", "filter.broken.smudge", "false"]);
    sb.git(&main, &["config", "filter.broken.required", "true"]);
    install_hook(&main, "post-checkout", "touch hook-ran");
    let stock = sb.git_raw(
        &main,
        &[
            "worktree",
            "add",
            "-b",
            "stock",
            "--lock",
            &sb.p("stock").to_string_lossy(),
        ],
    );
    let cow = sb.sharetree(
        &main,
        &["add", "-b", "cow", "--lock", &sb.p("cow").to_string_lossy()],
    );
    let stderr = String::from_utf8_lossy(&cow.stderr);
    assert!(!stock.status.success());
    assert_eq!(stock.status.code(), cow.status.code(), "{stderr}");
    assert_eq!(
        stderr.matches("smudge filter broken failed").count(),
        1,
        "{stderr}"
    );
    assert!(!sb.p("stock").exists());
    assert!(!sb.p("cow").exists());
    let worktrees = sb.git(&main, &["worktree", "list", "--porcelain"]);
    assert_eq!(worktrees.matches("worktree ").count(), 1, "{worktrees}");
    assert_eq!(
        sb.git(&main, &["rev-parse", "refs/heads/stock"]),
        sb.git(&main, &["rev-parse", "refs/heads/cow"])
    );
}

#[test]
fn failed_post_checkout_hook_keeps_worktree() {
    let Some(sb) = Sandbox::reflink() else { return };
    let main = sb.repo("main", &[]);
    install_hook(&main, "post-checkout", "exit 42");
    let cow = sb.sharetree(&main, &["add", "--detach", &sb.p("cow").to_string_lossy()]);
    let stderr = String::from_utf8_lossy(&cow.stderr);
    assert_eq!(cow.status.code(), Some(42), "{stderr}");
    assert_eq!(reflinked(&stderr), 15, "{stderr}");
    let worktrees = sb.git(&main, &["worktree", "list", "--porcelain"]);
    assert_eq!(worktrees.matches("worktree ").count(), 2, "{worktrees}");
}

#[test]
fn explicit_git_worktree_never_resets_the_source() {
    let Some(sb) = Sandbox::reflink() else { return };
    let main = sb.repo("main", &[]);
    let before = sb.git(&main, &["rev-parse", "HEAD"]).trim().to_string();
    write(&main.join("big.bin"), b"new commit");
    let after = sb.commit_all(&main, "second");
    write(&main.join("big.bin"), b"uncommitted source edit");
    let stock = sb.p("stock");
    sb.git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            &stock.to_string_lossy(),
            &before,
        ],
    );
    let cow = sb.p("cow");
    let out = sb
        .sharetree_cmd(&main)
        .env("GIT_DIR", main.join(".git"))
        .env("GIT_WORK_TREE", &main)
        .args(["add", "--detach", &cow.to_string_lossy(), &before])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains("GIT_DIR is set"), "{stderr}");
    assert_eq!(
        fs::read(main.join("big.bin")).unwrap(),
        b"uncommitted source edit"
    );
    assert_eq!(sb.git(&main, &["rev-parse", "HEAD"]).trim(), after);
    assert_equivalent(&sb, &stock, &cow);
}

#[test]
fn reference_transaction_hook_delegates() {
    let Some(sb) = Sandbox::reflink() else { return };
    let main = sb.repo("main", &[]);
    install_hook(&main, "reference-transaction", "exit 0");
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
        err.contains("reference-transaction hook") && err.contains("plain `git worktree add`"),
        "{err}"
    );
    assert_equivalent(&sb, &sb.p("stock"), &sb.p("cow"));
}

#[test]
fn registration_lock_failure_stops_quiet_add_before_registration() {
    let Some(sb) = Sandbox::reflink() else { return };
    let main = sb.repo("main", &[]);
    let dest = sb.p("cow");
    let worktrees = sb.git(&main, &["worktree", "list", "--porcelain"]);
    fs::create_dir(main.join(".git/sharetree-register.lock")).unwrap();

    let out = sb.sharetree(&main, &["add", "-q", "-b", "cow", &dest.to_string_lossy()]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("cannot lock worktree registration"), "{err}");
    assert!(err.contains("sharetree-register.lock"), "{err}");
    assert!(!dest.exists());
    assert_eq!(
        sb.git(&main, &["worktree", "list", "--porcelain"]),
        worktrees
    );
    assert!(sb.git(&main, &["branch", "--list", "cow"]).is_empty());
}

#[test]
fn relative_hooks_path_resolves_from_worktree_root() {
    let Some(sb) = Sandbox::reflink() else { return };
    let main = sb.repo("main", &[]);
    let hooks = main.join("hooks");
    fs::create_dir(&hooks).unwrap();
    let hook = hooks.join("reference-transaction");
    write(&hook, b"#!/bin/sh\nexit 0\n");
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    sb.git(&main, &["config", "core.hooksPath", "hooks"]);

    let err = sb.sharetree_ok(
        &main.join("src"),
        &["add", "--detach", &sb.p("cow").to_string_lossy()],
    );
    assert!(
        err.contains("a reference-transaction hook is installed"),
        "{err}"
    );
}

#[test]
fn passthrough_cases_delegate() {
    let Some(sb) = Sandbox::reflink() else { return };
    let main = sb.repo("main", &[]);
    let err = sb.sharetree_ok(
        &main,
        &[
            "add",
            "--no-checkout",
            "--detach",
            &sb.p("nc").to_string_lossy(),
        ],
    );
    assert!(err.contains("--no-checkout"), "{err}");
    assert!(!sb.p("nc/big.bin").exists());
    sb.git(&main, &["config", "core.splitIndex", "true"]);
    let err = sb.sharetree_ok(&main, &["add", "--detach", &sb.p("si").to_string_lossy()]);
    assert!(err.contains("core.splitIndex"), "{err}");
    let out = sb.sharetree(&main, &["add", "--detach", &sb.p("si").to_string_lossy()]);
    assert!(!out.status.success());
}
