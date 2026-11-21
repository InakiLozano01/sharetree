#![cfg(target_os = "linux")]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use common::Sandbox;

#[test]
fn grouped_tools_preserve_arguments_and_remove_worktrees() {
    let sandbox = Sandbox::home();
    let repo = sandbox.repo("benchmark repo", &[]);
    let other = sandbox.p("existing worktree");
    sandbox.git(
        &repo,
        &["worktree", "add", "--detach", other.to_str().unwrap()],
    );
    let before = sandbox.git(&repo, &["worktree", "list", "--porcelain"]);
    let tool = sandbox.p("benchmark program");
    fs::write(
        &tool,
        r#"#!/usr/bin/env bash
if [[ $# != 8 || $1 != 'two words' || $2 != '*.bin' || $3 != --tool || -n $4 || $SHARETREE_BENCH_VALUE != '*.bin' ]]; then
    printf 'unexpected argument: <%s>\n' "$@" >&2
    exit 99
fi
shift 4
exec git worktree "$@"
"#,
    ).unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();

    let mut command = Command::new("bash");
    sandbox.env(&mut command);
    let output = command
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/bench/compare.sh"))
        .arg(&repo)
        .args(["1", "--tool", "argv", "env", "SHARETREE_BENCH_VALUE=*.bin"])
        .arg(&tool)
        .args([
            "two words",
            "*.bin",
            "--tool",
            "",
            "",
            "--tool",
            "stock",
            "git",
        ])
        .env("SETTLE", "0")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "benchmark failed: {}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let labels: Vec<_> = stdout
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    assert_eq!(labels, ["argv", "stock"], "{stdout}");
    assert_eq!(
        sandbox.git(&repo, &["worktree", "list", "--porcelain"]),
        before
    );
    assert!(other.join(".git").is_file());
    assert!(sandbox.root().read_dir().unwrap().all(|entry| !entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".sharetree-compare.")));
}
