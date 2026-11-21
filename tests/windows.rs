#![cfg(windows)]

use std::path::Path;
use std::process::{Command, Output};

fn run(dir: &Path, config: &Path, program: &str, args: &[&str]) -> Output {
    let output = Command::new(program)
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", config.with_extension("gitconfig"))
        .env("SHARETREE_CONFIG", config)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
fn plain_and_opt_in_worktrees_match_git_and_keep_edits_isolated() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let config = root.join("sharetree.toml");
    let repo = root.join("repo");
    let contents = vec![42; 64 * 1024];

    run(root, &config, "git", &["init", "-q", "repo"]);
    std::fs::write(repo.join("data.bin"), &contents).unwrap();
    run(&repo, &config, "git", &["add", "data.bin"]);
    run(
        &repo,
        &config,
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

    for (name, flag) in [("stock", "--no-cow"), ("opt-in", "--windows-clone")] {
        let wt = root.join(name);
        run(
            &repo,
            &config,
            env!("CARGO_BIN_EXE_sharetree"),
            &["add", flag, "--detach", wt.to_str().unwrap()],
        );
        assert_eq!(std::fs::read(wt.join("data.bin")).unwrap(), contents);
        assert!(run(&wt, &config, "git", &["status", "--porcelain"])
            .stdout
            .is_empty());
        std::fs::write(wt.join("data.bin"), "edited").unwrap();
        assert_eq!(std::fs::read(repo.join("data.bin")).unwrap(), contents);
    }
}
