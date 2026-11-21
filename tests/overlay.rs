#![cfg(target_os = "linux")]

mod common;

use std::path::Path;
use std::process::{Command, Stdio};

use common::*;

#[test]
fn removes_legacy_overlay_with_custom_lock_and_protects_its_base() {
    let sb = Sandbox::home();
    let main = sb.repo("main", &[]);
    let base = sb.p("base");
    let overlay = sb.p("overlay");
    let upper = sb.p("layers/upper");
    let work = sb.p("layers/work");
    sb.git(
        &main,
        &[
            "worktree",
            "add",
            "--detach",
            "--lock",
            "--reason",
            "sharetree overlay base: read-only lower layer of overlay worktrees; do not edit",
            &base.to_string_lossy(),
        ],
    );
    sb.git(
        &main,
        &[
            "worktree",
            "add",
            "--detach",
            "--no-checkout",
            "--lock",
            "--reason",
            "custom reason",
            &overlay.to_string_lossy(),
        ],
    );
    std::fs::create_dir_all(&upper).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    std::fs::rename(overlay.join(".git"), upper.join(".git")).unwrap();
    write_overlay_config(&sb, &main, &overlay, &base, None);

    let blocked = sb.sharetree(&main, &["rm", "-ff", &base.to_string_lossy()]);
    assert!(!blocked.status.success());
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("still used"));
    sb.sharetree_ok(&main, &["rm", "-ff", &overlay.to_string_lossy()]);
    assert_eq!(sb.git(&main, &["worktree", "list"]).lines().count(), 1);
    assert!(!base.exists() && !overlay.exists() && !sb.p("layers").exists());
}

#[test]
fn resumes_cleanup_after_overlay_registration_is_gone() {
    let sb = Sandbox::home();
    let main = sb.repo("main", &[]);
    let overlay = sb.p("overlay");
    write_overlay_config(&sb, &main, &overlay, &sb.p("base"), Some("removing"));
    write(&sb.p("layers/upper/pending"), b"pending sweep");
    write(&overlay.join("outside-overlay"), b"keep this host file");
    let other = sb.repo("other", &[]);
    assert!(!sb
        .sharetree(&other, &["rm", &overlay.to_string_lossy()])
        .status
        .success());
    let out = sb.sharetree(&main, &["mount", "--all"]);
    assert!(out.status.success() && out.stdout.is_empty());
    let out = sb.sharetree(&main, &["mount", &overlay.to_string_lossy()]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unfinished"));

    sb.sharetree_ok(&main, &["rm", &overlay.to_string_lossy()]);
    assert!(!sb.p("layers").exists());
    assert_eq!(
        std::fs::read(overlay.join("outside-overlay")).unwrap(),
        b"keep this host file"
    );
    assert!(!std::fs::read_to_string(sb.config_path())
        .unwrap()
        .contains("[[overlay]]"));
}

fn write_overlay_config(sb: &Sandbox, main: &Path, path: &Path, base: &Path, phase: Option<&str>) {
    let mut cfg = format!(
        "[[overlay]]\npath={path:?}\ncommon_dir={:?}\nlower={base:?}\nupper={:?}\nwork={:?}\n",
        main.join(".git"),
        sb.p("layers/upper"),
        sb.p("layers/work")
    );
    if let Some(phase) = phase {
        cfg.push_str(&format!("phase={phase:?}\n"));
    }
    write(&sb.config_path(), cfg.as_bytes());
}

fn on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()))
}

#[test]
fn missing_fuse_overlayfs_fails_clearly() {
    if on_path("fuse-overlayfs") {
        eprintln!("SKIP missing_fuse_overlayfs_fails_clearly: fuse-overlayfs is installed");
        return;
    }
    let sb = Sandbox::home();
    let main = sb.repo("main", &[]);
    let out = sb.sharetree(&main, &["add", "--overlay", &sb.p("ovl").to_string_lossy()]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(err.contains("fuse-overlayfs"), "{err}");
    assert!(!sb.p("ovl").exists(), "nothing created");
    assert!(
        !sb.git(&main, &["worktree", "list"]).contains("ovl"),
        "nothing registered"
    );
}

struct Unmount(Vec<std::path::PathBuf>);

impl Drop for Unmount {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = Command::new("fusermount3")
                .arg("-u")
                .arg(p)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

fn mounted(p: &Path) -> bool {
    let want = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    std::fs::read_to_string("/proc/self/mountinfo")
        .unwrap_or_default()
        .lines()
        .any(|l| {
            l.split(' ').nth(4) == Some(&*want.to_string_lossy()) && l.contains("fuse-overlayfs")
        })
}

#[test]
fn overlay_worktree_end_to_end() {
    for tool in ["fuse-overlayfs", "fusermount3"] {
        if !on_path(tool) {
            eprintln!("SKIP overlay_worktree_end_to_end: {tool} is not installed");
            return;
        }
    }
    if !Path::new("/dev/fuse").exists() {
        eprintln!("SKIP overlay_worktree_end_to_end: no /dev/fuse");
        return;
    }
    let Some(sb) = Sandbox::tmpfs() else { return };
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
    let ovl = sb.p("ovl");
    let _guard = Unmount(vec![ovl.clone(), sb.p("ovl2")]);
    let out = sb
        .sharetree_cmd(&main)
        .env("SHARETREE_NO_UNIT", "1")
        .args(["add", "--overlay", "--detach", &ovl.to_string_lossy()])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() && err.contains("fuse") && err.contains("ermission") {
        eprintln!("SKIP overlay_worktree_end_to_end: cannot mount here: {err}");
        return;
    }
    assert!(out.status.success(), "{err}");
    assert!(mounted(&ovl), "{err}");
    assert!(
        err.contains("copies the whole file"),
        "costs are printed: {err}"
    );
    assert_equivalent(&sb, &sb.p("stock"), &ovl);
    let list = sb.git(&main, &["worktree", "list", "--porcelain"]);
    assert!(list.contains("locked sharetree overlay"), "{list}");
    assert!(list.contains("locked sharetree overlay base"), "{list}");

    let ovl2 = sb.p("ovl2");
    let out = sb
        .sharetree_cmd(&main)
        .env("SHARETREE_NO_UNIT", "1")
        .args(["add", "--overlay", "--detach", &ovl2.to_string_lossy()])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!String::from_utf8_lossy(&out.stderr).contains("creating the overlay base"));
    write(&ovl.join("big.bin"), b"changed\n");
    assert_ne!(std::fs::read(ovl2.join("big.bin")).unwrap(), b"changed\n");

    sb.sharetree_ok(&main, &["mount", "--unmount", "--all"]);
    assert!(!mounted(&ovl) && !ovl.join(".git").exists());
    sb.sharetree_ok(&main, &["mount", "--all"]);
    assert!(mounted(&ovl));
    assert_eq!(std::fs::read(ovl.join("big.bin")).unwrap(), b"changed\n");

    let out = sb.sharetree(&main, &["rm", &ovl.to_string_lossy()]);
    assert!(!out.status.success());
    let mut busy = Command::new("sleep")
        .arg("60")
        .current_dir(&ovl)
        .spawn()
        .unwrap();
    let out = sb.sharetree(&main, &["rm", "-f", &ovl.to_string_lossy()]);
    busy.kill().unwrap();
    busy.wait().unwrap();
    assert!(!out.status.success(), "a busy mount cannot be removed");
    assert!(mounted(&ovl));
    assert_eq!(std::fs::read(ovl.join("big.bin")).unwrap(), b"changed\n");
    sb.sharetree_ok(&main, &["rm", &ovl.to_string_lossy()]);
    assert!(!mounted(&ovl) && !ovl.exists());
    sb.sharetree_ok(&main, &["rm", &ovl2.to_string_lossy()]);
    let list = sb.git(&main, &["worktree", "list", "--porcelain"]);
    assert!(
        !list.contains("overlay"),
        "base removed with the last overlay: {list}"
    );
}
