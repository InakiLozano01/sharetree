#![cfg(target_os = "linux")]

mod common;

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use common::*;

fn configure_volume(sb: &Sandbox, extra: &str) {
    let image = sb.p("missing-volume.img");
    write(
        &sb.config_path(),
        format!(
            "{extra}[[volume]]\nname = \"default\"\nkind = \"btrfs\"\nimage = \"{}\"\n\
             label = \"sharetree\"\nmount = \"udisks\"\n",
            image.display()
        )
        .as_bytes(),
    );
}

fn config(sb: &Sandbox) -> String {
    std::fs::read_to_string(sb.config_path()).unwrap()
}

#[test]
fn no_terminal_means_plain_git_and_a_hint() {
    let Some(sb) = Sandbox::tmpfs() else { return };
    configure_volume(&sb, "");
    let before = config(&sb);
    let main = sb.repo("main", &[]);
    let wt = sb.p("wt");
    let err = sb.sharetree_ok(&main, &["add", "--detach", &wt.to_string_lossy()]);
    assert!(err.contains("running plain `git worktree add`"), "{err}");
    assert!(
        err.contains("you have not chosen") && err.contains("no terminal to ask on"),
        "{err}"
    );
    assert!(
        err.contains("--into-volume") && err.contains("sharetree setup auto on --repo"),
        "{err}"
    );
    assert!(wt.is_dir() && !wt.is_symlink(), "a plain worktree");
    assert_eq!(config(&sb), before, "nothing is saved without an answer");

    sb.sharetree_ok(&main, &["setup", "auto", "off", "--repo"]);
    let err = sb.sharetree_ok(&main, &["add", "--detach", &sb.p("wt2").to_string_lossy()]);
    assert!(!err.contains("hint:"), "{err}");
    sb.sharetree_ok(&main, &["setup", "auto", "ask", "--repo"]);
    let err = sb.sharetree_ok(&main, &["add", "--detach", &sb.p("wt3").to_string_lossy()]);
    assert!(err.contains("no terminal to ask on"), "{err}");
}

#[test]
fn no_prompt_env_suppresses_the_question() {
    let Some(sb) = Sandbox::tmpfs() else { return };
    configure_volume(&sb, "");
    let main = sb.repo("main", &[]);
    let Some(out) = in_pty(&sb, &main, &[("SHARETREE_NO_PROMPT", "1")], "a\n") else {
        return;
    };
    assert!(out.contains("no terminal to ask on"), "{out}");
    assert!(!out.contains("Use volume"), "{out}");
    assert!(!config(&sb).contains("auto_volume"));
}

fn in_pty(sb: &Sandbox, dir: &Path, env: &[(&str, &str)], input: &str) -> Option<String> {
    if Command::new("script").arg("--version").output().is_err() {
        eprintln!("SKIP: util-linux `script` is not installed");
        return None;
    }
    let cmd = format!(
        "{} add --detach {}",
        BIN,
        sb.p("wt").to_string_lossy().replace(' ', "\\ ")
    );
    let template = sb.sharetree_cmd(dir);
    let mut s = Command::new("script");
    s.current_dir(dir)
        .args(["-qec", &cmd, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in template.get_envs() {
        match v {
            Some(v) => s.env(k, v),
            None => s.env_remove(k),
        };
    }
    for (k, v) in env {
        s.env(k, v);
    }
    let mut child = s.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    Some(format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    ))
}

#[test]
fn terminal_answers_never_and_always_are_saved() {
    let Some(sb) = Sandbox::tmpfs() else { return };
    configure_volume(&sb, "");
    let main = sb.repo("main", &[]);

    let Some(out) = in_pty(&sb, &main, &[], "n\n") else {
        return;
    };
    assert!(out.contains("Use volume 'default'?"), "{out}");
    assert!(sb.p("wt").is_dir() && !sb.p("wt").is_symlink(), "{out}");
    assert!(!config(&sb).contains("auto_volume"), "{}", config(&sb));
    sb.sharetree_ok(&main, &["rm", "--sync", &sb.p("wt").to_string_lossy()]);

    let out = in_pty(&sb, &main, &[], "maybe\nv\n").unwrap();
    assert!(out.contains("please answer"), "{out}");
    assert!(out.contains("saved auto_volume = false"), "{out}");
    assert!(sb.p("wt").is_dir() && !sb.p("wt").is_symlink(), "{out}");
    let cfg = config(&sb);
    assert!(
        cfg.contains("[[repo]]") && cfg.contains("auto_volume = false"),
        "{cfg}"
    );
    sb.sharetree_ok(&main, &["rm", "--sync", &sb.p("wt").to_string_lossy()]);

    let out = in_pty(&sb, &main, &[], "").unwrap();
    assert!(!out.contains("Use volume"), "{out}");
    sb.sharetree_ok(&main, &["rm", "--sync", &sb.p("wt").to_string_lossy()]);

    sb.sharetree_ok(&main, &["setup", "auto", "ask", "--repo"]);
    let out = in_pty(&sb, &main, &[], "a\n").unwrap();
    assert!(out.contains("saved auto_volume = true"), "{out}");
    assert!(out.contains("missing"), "{out}");
    assert!(!sb.p("wt").exists(), "{out}");
    assert!(config(&sb).contains("auto_volume = true"));
}
