//! `sharetree add --overlay`: fuse-overlayfs worktrees over a base checkout.
//! The end-to-end test runs only where fuse-overlayfs is installed and can
//! mount without root; elsewhere it prints SKIP. It always unmounts and
//! removes what it created.

#![cfg(target_os = "linux")]

mod common;

use std::path::Path;
use std::process::{Command, Stdio};

use common::*;

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

/// Unmount anything left mounted under the sandbox.
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
    // /tmp is tmpfs here: no reflinks, the case the overlay is for. Data
    // (bases, upper/work) goes to the sandbox's XDG_DATA_HOME on the same fs.
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

    // A second overlay shares the base; edits stay private.
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

    // Unmount and remount (what the login unit does).
    sb.sharetree_ok(&main, &["mount", "--unmount", "--all"]);
    assert!(!mounted(&ovl) && !ovl.join(".git").exists());
    sb.sharetree_ok(&main, &["mount", "--all"]);
    assert!(mounted(&ovl));
    assert_eq!(std::fs::read(ovl.join("big.bin")).unwrap(), b"changed\n");

    // rm refuses a dirty overlay, then cleans up everything with -f.
    let out = sb.sharetree(&main, &["rm", &ovl.to_string_lossy()]);
    assert!(!out.status.success());
    sb.sharetree_ok(&main, &["rm", "-f", &ovl.to_string_lossy()]);
    assert!(!mounted(&ovl) && !ovl.exists());
    sb.sharetree_ok(&main, &["rm", &ovl2.to_string_lossy()]);
    let list = sb.git(&main, &["worktree", "list", "--porcelain"]);
    assert!(
        !list.contains("overlay"),
        "base removed with the last overlay: {list}"
    );
}
