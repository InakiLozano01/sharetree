#![cfg(target_os = "linux")]

mod common;

use std::process::Command;

use common::Sandbox;

#[test]
fn grouped_tools_preserve_arguments_and_remove_worktrees() {
    let sandbox = Sandbox::home();
    let repo = sandbox.repo("benchmark repo", &[]);
    let check_argv = r#"
if [[ $# != 8 || $1 != 'two words' || $2 != '*.bin' || $3 != --tool || -n $4 ]]; then
    printf 'unexpected argument: <%s>\n' "$@" >&2
    exit 99
fi
shift 4
exec git worktree "$@"
"#;

    let output = Command::new("bash")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/bench/compare.sh"))
        .arg(&repo)
        .args([
            "1",
            "--tool",
            "argv",
            "bash",
            "-c",
            check_argv,
            "benchmark",
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
    let worktrees = sandbox.git(&repo, &["worktree", "list", "--porcelain"]);
    assert_eq!(
        worktrees
            .lines()
            .filter(|line| line.starts_with("worktree "))
            .count(),
        1,
        "{worktrees}"
    );
    assert!(sandbox.root().read_dir().unwrap().all(|entry| !entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".sharetree-compare.")));
}
