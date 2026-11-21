#![cfg(unix)]

mod common;

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::time::{Duration, SystemTime};

use common::*;

fn set_old_mtime(p: &Path) -> SystemTime {
    let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    fs::File::options()
        .write(true)
        .open(p)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(t))
        .unwrap();
    t
}

fn cache_repo(sb: &Sandbox) -> std::path::PathBuf {
    let main = sb.repo("main", &[]);
    write(
        &main.join(".gitignore"),
        b"node_modules/\n.venv/\nbuild/\n.env\n.env.local\ntarget/\n*.log\n",
    );
    sb.commit_all(&main, "ignores");
    write(&main.join("node_modules/pkg/index.js"), &bytes(1, 20_000));
    write(
        &main.join("node_modules/pkg/index.js.map"),
        &bytes(2, 9_000),
    );
    fs::create_dir_all(main.join("node_modules/.bin")).unwrap();
    std::os::unix::fs::symlink(
        main.join("node_modules/pkg/index.js"),
        main.join("node_modules/.bin/abs"),
    )
    .unwrap();
    std::os::unix::fs::symlink("../pkg/index.js", main.join("node_modules/.bin/rel")).unwrap();
    std::os::unix::fs::symlink("/usr/bin/env", main.join("node_modules/.bin/outside")).unwrap();
    write(&main.join(".venv/pyvenv.cfg"), b"home = /usr/bin\n");
    write(&main.join(".venv/bin/python"), &bytes(3, 5000));
    write(
        &main.join("build/CMakeCache.txt"),
        b"CMAKE_HOME_DIRECTORY:INTERNAL=/somewhere\n",
    );
    write(&main.join("build/obj.o"), &bytes(4, 5000));
    write(&main.join(".env"), b"SECRET=1\n");
    write(&main.join(".env.local"), b"SECRET=2\n");
    write(&main.join("target/debug/app"), &bytes(5, 64_000));
    fs::set_permissions(
        main.join("target/debug/app"),
        fs::Permissions::from_mode(0o751),
    )
    .unwrap();
    set_old_mtime(&main.join("target/debug/app"));
    write(&main.join("target/py/pyvenv.cfg"), b"x");
    write(&main.join("debug.log"), b"log");
    main
}

#[test]
fn warm_clones_caches_and_vetoes_non_relocatable_paths() {
    let Some(sb) = Sandbox::reflink() else { return };
    let main = cache_repo(&sb);
    let cow = sb.p("cow");
    let err = sb.sharetree_ok(
        &main,
        &["add", "--warm", "--detach", &cow.to_string_lossy()],
    );
    eprintln!("{err}");
    assert_eq!(
        fs::read(cow.join("node_modules/pkg/index.js")).unwrap(),
        bytes(1, 20_000)
    );
    assert_eq!(
        fs::read_link(cow.join("node_modules/.bin/abs")).unwrap(),
        cow.join("node_modules/pkg/index.js")
    );
    assert_eq!(
        fs::read_link(cow.join("node_modules/.bin/rel")).unwrap(),
        Path::new("../pkg/index.js")
    );
    assert_eq!(
        fs::read_link(cow.join("node_modules/.bin/outside")).unwrap(),
        Path::new("/usr/bin/env")
    );
    let a = fs::metadata(main.join("target/debug/app")).unwrap();
    let b = fs::metadata(cow.join("target/debug/app")).unwrap();
    assert_eq!(
        (a.mtime(), a.mode() & 0o7777),
        (b.mtime(), b.mode() & 0o7777)
    );
    for p in [".venv", "build", ".env", ".env.local", "target/py"] {
        assert!(!cow.join(p).exists(), "{p} must not be cloned");
    }
    assert!(
        err.contains("pyvenv.cfg") && err.contains("CMakeCache.txt") && err.contains(".env"),
        "{err}"
    );
    assert!(sb.git(&cow, &["status", "--porcelain"]).is_empty());
    fs::write(cow.join("node_modules/pkg/index.js"), b"edited").unwrap();
    assert_eq!(
        fs::read(main.join("node_modules/pkg/index.js")).unwrap(),
        bytes(1, 20_000)
    );
}

#[test]
fn warm_policy_selects_and_skips() {
    let Some(sb) = Sandbox::reflink() else { return };
    let main = cache_repo(&sb);
    write(
        &main.join(".sharetree.toml"),
        b"[warm]\nclone = [\"node_modules\"]\nskip = [\"*.map\"]\n",
    );
    let cow = sb.p("cow");
    sb.sharetree_ok(
        &main,
        &["add", "--warm", "--detach", &cow.to_string_lossy()],
    );
    assert!(cow.join("node_modules/pkg/index.js").exists());
    assert!(!cow.join("node_modules/pkg/index.js.map").exists());
    assert!(!cow.join("target").exists());
    assert!(!cow.join("debug.log").exists());
}

#[test]
fn warm_skips_other_worktrees_nested_in_the_donor() {
    let Some(sb) = Sandbox::reflink() else { return };
    let main = cache_repo(&sb);
    write(&main.join(".git/info/exclude"), b".worktrees/\n");
    sb.sharetree_ok(&main, &["add", "--detach", ".worktrees/a"]);
    let err = sb.sharetree_ok(&main, &["add", "--warm", "--detach", ".worktrees/b"]);
    assert!(!main.join(".worktrees/b/.worktrees").exists(), "{err}");
    assert!(main.join(".worktrees/b/node_modules/pkg/index.js").exists());
}

#[test]
fn warm_preserves_a_tracked_symlink_to_an_external_directory() {
    let Some(sb) = Sandbox::reflink() else { return };
    let main = sb.repo("main", &[]);
    write(&main.join(".gitignore"), b"nested/cache/\n");
    write(&main.join("nested/tracked"), b"tracked\n");
    sb.commit_all(&main, "add ignored cache policy");
    write(&main.join("nested/cache/asset"), &bytes(14, 32_000));

    let outside = sb.p("outside");
    fs::create_dir(&outside).unwrap();
    let editor = sb.p("editor");
    sb.git(
        &main,
        &["worktree", "add", "--detach", editor.to_str().unwrap()],
    );
    fs::remove_dir_all(editor.join("nested")).unwrap();
    std::os::unix::fs::symlink(&outside, editor.join("nested")).unwrap();
    let target = sb.commit_all(&editor, "replace directory with link");

    let dest = sb.p("warm");
    sb.sharetree_ok(
        &main,
        &[
            "add",
            "--warm",
            "--detach",
            "--from",
            main.to_str().unwrap(),
            dest.to_str().unwrap(),
            &target,
        ],
    );
    assert_eq!(fs::read_link(dest.join("nested")).unwrap(), outside);
    assert!(!outside.join("cache").exists());
}
