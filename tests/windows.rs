#![cfg(windows)]

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationByHandleW;

fn run(dir: &Path, config: &Path, program: &str, args: &[&str]) -> Output {
    let output = Command::new(program)
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", config.with_extension("gitconfig"))
        .env("SHARETREE_CONFIG", config)
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("SHARETREE_FORCE_FALLBACK")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn init_repo(root: &Path, config: &Path, contents: &[u8]) -> PathBuf {
    let repo = root.join("repo");

    run(root, config, "git", &["init", "-q", "repo"]);
    fs::write(repo.join("data.bin"), contents).unwrap();
    run(&repo, config, "git", &["add", "data.bin"]);
    run(
        &repo,
        config,
        "git",
        &[
            "-c",
            "user.name=Sharetree Test",
            "-c",
            "user.email=test@example.test",
            "commit",
            "-qm",
            "initial",
        ],
    );
    repo
}

#[test]
fn plain_and_opt_in_worktrees_match_git_and_keep_edits_isolated() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let config = root.join("sharetree.toml");
    let contents = vec![42; 64 * 1024];
    let repo = init_repo(root, &config, &contents);

    for (name, flag) in [("stock", "--no-cow"), ("opt-in", "--windows-clone")] {
        let wt = root.join(name);
        run(
            &repo,
            &config,
            env!("CARGO_BIN_EXE_sharetree"),
            &["add", flag, "--detach", wt.to_str().unwrap()],
        );
        assert_eq!(fs::read(wt.join("data.bin")).unwrap(), contents);
        assert!(run(&wt, &config, "git", &["status", "--porcelain"])
            .stdout
            .is_empty());
        fs::write(wt.join("data.bin"), "edited").unwrap();
        assert_eq!(fs::read(repo.join("data.bin")).unwrap(), contents);
    }
}

fn require_refs(path: &Path) {
    let file = File::open(path).unwrap();
    let mut filesystem = [0u16; 32];
    let ok = unsafe {
        GetVolumeInformationByHandleW(
            file.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            filesystem.as_mut_ptr(),
            filesystem.len() as u32,
        )
    };
    assert_ne!(
        ok,
        0,
        "querying {}: {}",
        path.display(),
        io::Error::last_os_error()
    );
    assert_eq!(
        String::from_utf16_lossy(&filesystem).trim_end_matches('\0'),
        "ReFS",
        "SHARETREE_TEST_REFS_DIR must point to a directory on ReFS"
    );
}

#[test]
#[ignore = "requires SHARETREE_TEST_REFS_DIR on an existing ReFS volume"]
fn refs_worktree_uses_verified_clones_and_keeps_edits_isolated() {
    let parent = std::env::var_os("SHARETREE_TEST_REFS_DIR")
        .filter(|path| !path.is_empty())
        .expect("set SHARETREE_TEST_REFS_DIR to an existing writable directory on ReFS");
    let parent = std::path::absolute(parent).unwrap();
    let tmp = tempfile::Builder::new()
        .prefix("sharetree-refs-")
        .tempdir_in(&parent)
        .unwrap_or_else(|error| {
            panic!("creating a test directory in {}: {error}", parent.display())
        });
    let root = tmp.path();
    let config = root.join("sharetree.toml");
    let contents: Vec<u8> = (0..64 * 1024).map(|i| i as u8).collect();
    let repo = init_repo(root, &config, &contents);
    let donor_file = repo.join("data.bin");
    require_refs(&donor_file);

    let out = run(
        &repo,
        &config,
        env!("CARGO_BIN_EXE_sharetree"),
        &["add", "--windows-clone", "--detach", "-v", "../cloned"],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr
            .lines()
            .any(|line| line.starts_with("sharetree: reflinked 1 of 1 files ")),
        "expected one verified ReFS clone: {stderr}"
    );

    run(
        &repo,
        &config,
        "git",
        &["worktree", "add", "--detach", "../stock"],
    );
    let cloned = root.join("cloned");
    let stock = root.join("stock");
    for args in [["rev-parse", "HEAD"], ["ls-files", "--stage"]] {
        assert_eq!(
            run(&cloned, &config, "git", &args).stdout,
            run(&stock, &config, "git", &args).stdout,
            "git {args:?}"
        );
    }
    for wt in [&cloned, &stock] {
        assert_eq!(fs::read(wt.join("data.bin")).unwrap(), contents);
        assert!(run(wt, &config, "git", &["status", "--porcelain"])
            .stdout
            .is_empty());
    }

    let cloned_file = cloned.join("data.bin");
    OpenOptions::new()
        .write(true)
        .open(&cloned_file)
        .unwrap()
        .write_all(b"edited")
        .unwrap();
    assert!(fs::read(cloned_file).unwrap().starts_with(b"edited"));
    assert_eq!(fs::read(donor_file).unwrap(), contents);
    assert_eq!(fs::read(stock.join("data.bin")).unwrap(), contents);
}
