//! Test harness: sandboxed git (no user/system config), repos on a chosen
//! filesystem, and a drop-in comparison against stock `git worktree add`.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

pub const BIN: &str = env!("CARGO_BIN_EXE_sharetree");

/// Filesystem type name of `p` (`stat -f -c %T`).
pub fn fs_type(p: &Path) -> String {
    let out = Command::new("stat")
        .args(["-f", "-c", "%T"])
        .arg(p)
        .output()
        .expect("stat");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

pub struct Sandbox {
    pub dir: tempfile::TempDir,
    home: PathBuf,
}

impl Sandbox {
    fn new_in(base: &Path) -> Sandbox {
        fs::create_dir_all(base).unwrap();
        let dir = tempfile::Builder::new()
            .prefix("sharetree-test-")
            .tempdir_in(base)
            .unwrap();
        let home = dir.path().join("home");
        fs::create_dir_all(&home).unwrap();
        fs::write(
            home.join(".gitconfig"),
            "[user]\n\tname = T\n\temail = t@example.com\n[init]\n\tdefaultBranch = main\n[advice]\n\tdetachedHead = false\n[commit]\n\tgpgsign = false\n",
        )
        .unwrap();
        Sandbox { dir, home }
    }

    /// Under the target dir: on this host that is Btrfs /home, not tmpfs /tmp.
    pub fn home() -> Sandbox {
        Sandbox::new_in(Path::new(env!("CARGO_TARGET_TMPDIR")))
    }

    /// Sandbox on a filesystem that supports reflinks, or None (skip).
    pub fn reflink() -> Option<Sandbox> {
        let sb = Sandbox::home();
        let t = fs_type(sb.root());
        if t == "btrfs" || t == "xfs" {
            Some(sb)
        } else {
            eprintln!(
                "SKIP: {} is {t}, not a reflink filesystem",
                sb.root().display()
            );
            None
        }
    }

    /// Sandbox on /tmp when it is tmpfs (no reflinks), or None (skip).
    pub fn tmpfs() -> Option<Sandbox> {
        let t = fs_type(Path::new("/tmp"));
        if t != "tmpfs" {
            eprintln!("SKIP: /tmp is {t}, not tmpfs");
            return None;
        }
        Some(Sandbox::new_in(Path::new("/tmp")))
    }

    /// The sandbox's `$XDG_CONFIG_HOME/sharetree/config.toml`.
    pub fn config_path(&self) -> PathBuf {
        self.home.join(".config/sharetree/config.toml")
    }

    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    pub fn p(&self, rel: &str) -> PathBuf {
        self.root().join(rel)
    }

    fn env(&self, c: &mut Command) {
        c.env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_CONFIG_GLOBAL")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("SHARETREE_TRACE")
            .env_remove("SHARETREE_CONFIG")
            .env_remove("SHARETREE_NO_PROMPT")
            .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z");
    }

    pub fn git_cmd(&self, dir: &Path) -> Command {
        let mut c = Command::new("git");
        c.current_dir(dir);
        self.env(&mut c);
        c
    }

    pub fn git<S: AsRef<OsStr>>(&self, dir: &Path, args: &[S]) -> String {
        let out = self
            .git_cmd(dir)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?} in {}: {}",
            args.iter()
                .map(|a| a.as_ref().to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            dir.display(),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    pub fn git_raw<S: AsRef<OsStr>>(&self, dir: &Path, args: &[S]) -> Output {
        self.git_cmd(dir)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    pub fn sharetree_cmd(&self, dir: &Path) -> Command {
        let mut c = Command::new(BIN);
        c.current_dir(dir);
        self.env(&mut c);
        c
    }

    pub fn sharetree<S: AsRef<OsStr>>(&self, dir: &Path, args: &[S]) -> Output {
        self.sharetree_cmd(dir)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// sharetree that must succeed; returns stderr.
    pub fn sharetree_ok<S: AsRef<OsStr>>(&self, dir: &Path, args: &[S]) -> String {
        let out = self.sharetree(dir, args);
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(
            out.status.success(),
            "sharetree failed: {err}\n{}",
            String::from_utf8_lossy(&out.stdout)
        );
        err
    }

    /// A repository with a representative mix of tracked content.
    pub fn repo(&self, name: &str, init_args: &[&str]) -> PathBuf {
        let r = self.p(name);
        fs::create_dir_all(&r).unwrap();
        let mut args = vec!["init", "-q"];
        args.extend_from_slice(init_args);
        self.git(&r, &args);
        populate(&r);
        self.git(&r, &["add", "-A"]);
        self.git(&r, &["commit", "-qm", "initial"]);
        r
    }

    pub fn commit_all(&self, repo: &Path, msg: &str) -> String {
        self.git(repo, &["add", "-A"]);
        self.git(repo, &["commit", "-qm", msg]);
        self.git(repo, &["rev-parse", "HEAD"]).trim().to_string()
    }
}

/// Deterministic pseudo-random bytes.
pub fn bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

pub fn write(p: &Path, data: &[u8]) {
    if let Some(d) = p.parent() {
        fs::create_dir_all(d).unwrap();
    }
    fs::write(p, data).unwrap();
}

/// Files above and below the clone threshold, nested dirs, an exec file, an
/// empty file, relative symlinks.
pub fn populate(r: &Path) {
    for i in 0..12u64 {
        write(
            &r.join(format!("src/mod{i}/data{i}.bin")),
            &bytes(i, 4096 + (i as usize) * 3000),
        );
    }
    write(&r.join("big.bin"), &bytes(100, 300_000));
    write(&r.join("small.txt"), b"tiny\n");
    write(&r.join("empty"), b"");
    write(&r.join("docs/readme.md"), &bytes(7, 2500));
    write(&r.join("bin/tool.sh"), &bytes(8, 9000));
    fs::set_permissions(r.join("bin/tool.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("big.bin", r.join("link-to-big")).unwrap();
    std::os::unix::fs::symlink("../src/mod1", r.join("docs/link-dir")).unwrap();
}

#[derive(Debug, PartialEq, Eq)]
pub enum Node {
    Dir(u32),
    File(u32, Vec<u8>),
    Link(PathBuf),
}

/// Every path under a worktree except its top-level `.git`.
pub fn snapshot(root: &Path) -> BTreeMap<Vec<u8>, Node> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<Vec<u8>, Node>) {
        for e in fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let p = e.path();
            if dir == root && e.file_name() == ".git" {
                continue;
            }
            let rel = p
                .strip_prefix(root)
                .unwrap()
                .as_os_str()
                .as_bytes()
                .to_vec();
            let md = p.symlink_metadata().unwrap();
            let mode = md.permissions().mode() & 0o7777;
            if md.file_type().is_symlink() {
                out.insert(rel, Node::Link(fs::read_link(&p).unwrap()));
            } else if md.is_dir() {
                out.insert(rel, Node::Dir(mode));
                walk(root, &p, out);
            } else {
                out.insert(rel, Node::File(mode, fs::read(&p).unwrap()));
            }
        }
    }
    let mut m = BTreeMap::new();
    walk(root, root, &mut m);
    m
}

/// The drop-in invariant: files, modes, links, HEAD, index entries and
/// `git status` equal those of a stock worktree; and the index's stat data
/// is valid (no rehash needed on first status).
pub fn assert_equivalent(sb: &Sandbox, stock: &Path, cow: &Path) {
    let a = snapshot(stock);
    let b = snapshot(cow);
    if a != b {
        let ka: Vec<_> = a
            .keys()
            .map(|k| String::from_utf8_lossy(k).into_owned())
            .collect();
        let kb: Vec<_> = b
            .keys()
            .map(|k| String::from_utf8_lossy(k).into_owned())
            .collect();
        for (k, v) in &a {
            if b.get(k) != Some(v) {
                panic!(
                    "worktrees differ at {:?}\nstock keys: {ka:?}\ncow keys: {kb:?}",
                    String::from_utf8_lossy(k)
                );
            }
        }
        panic!("worktrees differ: cow has extra paths\nstock keys: {ka:?}\ncow keys: {kb:?}");
    }
    assert_eq!(
        sb.git(stock, &["rev-parse", "HEAD"]),
        sb.git(cow, &["rev-parse", "HEAD"]),
        "HEAD"
    );
    assert_eq!(
        sb.git(stock, &["ls-files", "-s", "-z"]),
        sb.git(cow, &["ls-files", "-s", "-z"]),
        "index entries"
    );
    // Stat-valid index: diff-files does not refresh, so any entry whose stat
    // data does not match the file would be listed.
    let df = sb.git_raw(cow, &["diff-files", "--quiet"]);
    assert!(
        df.status.success(),
        "cow index has stale stat data: {}",
        sb.git(cow, &["diff-files", "--name-only"])
    );
    let st = |d: &Path| {
        sb.git(
            d,
            &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        )
    };
    assert_eq!(st(stock), st(cow), "git status");
    assert!(sb
        .git_raw(cow, &["diff-index", "--cached", "--quiet", "HEAD"])
        .status
        .success());
}

/// Parse "reflinked N of M files" from sharetree's summary.
pub fn reflinked(stderr: &str) -> usize {
    stderr
        .lines()
        .find_map(|l| {
            l.split("reflinked ")
                .nth(1)
                .and_then(|r| r.split(' ').next())
                .and_then(|n| n.parse().ok())
        })
        .unwrap_or(0)
}

/// `btrfs filesystem du -s --raw` Exclusive bytes of the paths (summed), or
/// None when not on Btrfs / tool missing.
pub fn btrfs_exclusive(paths: &[PathBuf]) -> Option<u64> {
    let _ = Command::new("sync").status();
    let out = Command::new("btrfs")
        .args(["filesystem", "du", "-s", "--raw"])
        .args(paths)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut sum = 0;
    for l in text.lines().skip(1) {
        sum += l.split_whitespace().nth(1)?.parse::<u64>().ok()?;
    }
    Some(sum)
}

/// Top-level entries of a worktree except `.git` (to exclude the object store).
pub fn content_paths(root: &Path) -> Vec<PathBuf> {
    fs::read_dir(root)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name() != ".git")
        .map(|e| e.path())
        .collect()
}
