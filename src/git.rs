//! Thin wrappers around the `git` executable. sharetree reimplements only
//! two small pieces of git: the v2 index writer (`index.rs`) and blob
//! hashing (`hash.rs`).

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use anyhow::{bail, Context, Result};

use crate::util::bytes_to_path;

pub const MODE_REGULAR: u32 = 0o100644;
pub const MODE_EXEC: u32 = 0o100755;

/// These overrides can make changing the subprocess directory insufficient
/// to select the intended worktree, or change what our Git queries see.
pub(crate) fn context_override() -> Option<&'static str> {
    [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_IMPLICIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_CONFIG",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_GRAFT_FILE",
        "GIT_SHALLOW_FILE",
    ]
    .into_iter()
    .find(|var| std::env::var_os(var).is_some())
}

pub fn cmd(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.current_dir(dir);
    c
}

/// A read-only git invocation: never takes optional locks (so `status` and
/// friends do not rewrite a donor's index behind its owner's back).
pub fn read_cmd(dir: &Path) -> Command {
    let mut c = cmd(dir);
    c.env("GIT_OPTIONAL_LOCKS", "0");
    c
}

fn describe<S: AsRef<OsStr>>(args: &[S]) -> String {
    args.iter()
        .map(|a| a.as_ref().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Run git, capture stdout, fail with git's stderr on a non-zero exit.
pub fn output<S: AsRef<OsStr>>(dir: &Path, args: &[S]) -> Result<Vec<u8>> {
    let out = read_cmd(dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("failed to run git (is it installed and on PATH?)")?;
    if !out.status.success() {
        bail!(
            "git {} (in {}): {}",
            describe(args),
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

pub fn output_str<S: AsRef<OsStr>>(dir: &Path, args: &[S]) -> Result<String> {
    Ok(String::from_utf8_lossy(&output(dir, args)?)
        .trim_end_matches(['\n', '\r'])
        .to_string())
}

/// Run git with `input` on stdin (written from a separate thread so a large
/// input cannot deadlock against a large output).
pub fn output_with_input<S: AsRef<OsStr>>(dir: &Path, args: &[S], input: &[u8]) -> Result<Vec<u8>> {
    let mut child = read_cmd(dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to run git")?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let out = std::thread::scope(|s| {
        s.spawn(move || {
            let _ = stdin.write_all(input);
        });
        child.wait_with_output()
    })?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            describe(args),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

/// Run git with inherited stdio (the user sees git's own messages).
pub fn run_inherit<S: AsRef<OsStr>>(dir: &Path, args: &[S]) -> Result<ExitStatus> {
    cmd(dir).args(args).status().context("failed to run git")
}

/// Run git quietly and report whether it succeeded.
pub fn succeeds<S: AsRef<OsStr>>(dir: &Path, args: &[S]) -> bool {
    read_cmd(dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

pub fn rev_parse_commit(dir: &Path, rev: &OsStr) -> Result<String> {
    let mut spec = OsString::from(rev);
    spec.push("^{commit}");
    output_str(
        dir,
        &[
            OsStr::new("rev-parse"),
            OsStr::new("--verify"),
            OsStr::new("-q"),
            &spec,
        ],
    )
}

pub fn common_dir(dir: &Path) -> Result<PathBuf> {
    let s = output(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    Ok(bytes_to_path(trim_nl(&s)))
}

pub fn git_path(dir: &Path, name: &str) -> Result<PathBuf> {
    let s = output(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-path", name],
    )?;
    Ok(bytes_to_path(trim_nl(&s)))
}

pub fn toplevel(dir: &Path) -> Result<PathBuf> {
    let s = output(dir, &["rev-parse", "--show-toplevel"])?;
    Ok(bytes_to_path(trim_nl(&s)))
}

pub fn trim_nl(b: &[u8]) -> &[u8] {
    let mut end = b.len();
    while end > 0 && (b[end - 1] == b'\n' || b[end - 1] == b'\r') {
        end -= 1;
    }
    &b[..end]
}

/// A Git config value, preserving its whitespace and distinguishing unset
/// (exit 1) from a present empty value.
pub fn config_get(dir: &Path, key: &str) -> Result<Option<String>> {
    let out = read_cmd(dir)
        .args(["config", "-z", "--get", key])
        .stdin(Stdio::null())
        .output()
        .context("failed to run git config")?;
    match out.status.code() {
        Some(0) => {
            let value = out
                .stdout
                .strip_suffix(b"\0")
                .context("unterminated git config value")?;
            Ok(Some(String::from_utf8_lossy(value).into_owned()))
        }
        Some(1) => Ok(None),
        _ => bail!(
            "git config --get {key}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
}

pub fn config_bool(dir: &Path, key: &str) -> Result<Option<bool>> {
    let out = read_cmd(dir)
        .args(["config", "--type=bool", "--get", key])
        .stdin(Stdio::null())
        .output()
        .context("failed to run git config")?;
    match out.status.code() {
        Some(0) => Ok(Some(trim_nl(&out.stdout) == b"true")),
        Some(1) => Ok(None),
        _ => bail!(
            "git config --type=bool --get {key}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectFormat {
    Sha1,
    Sha256,
}

impl ObjectFormat {
    pub fn raw_len(self) -> usize {
        match self {
            ObjectFormat::Sha1 => 20,
            ObjectFormat::Sha256 => 32,
        }
    }
}

pub fn object_format(dir: &Path) -> Result<ObjectFormat> {
    match output_str(dir, &["rev-parse", "--show-object-format"])?.as_str() {
        "sha1" => Ok(ObjectFormat::Sha1),
        "sha256" => Ok(ObjectFormat::Sha256),
        other => bail!("unsupported object format {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Worktrees

#[derive(Clone, Debug, Default)]
pub struct Worktree {
    pub path: PathBuf,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub bare: bool,
    pub detached: bool,
    pub locked: bool,
    pub lock_reason: Option<String>,
    pub prunable: bool,
    pub is_main: bool,
}

pub fn list_worktrees(dir: &Path) -> Result<Vec<Worktree>> {
    let out = output(dir, &["worktree", "list", "--porcelain", "-z"])?;
    Ok(parse_worktrees(&out))
}

/// Parse `git worktree list --porcelain -z`: NUL-terminated fields, records
/// separated by an empty field. Paths are kept as raw bytes.
pub fn parse_worktrees(out: &[u8]) -> Vec<Worktree> {
    let mut wts = Vec::new();
    let mut cur: Option<Worktree> = None;
    for field in out.split(|&b| b == 0) {
        if field.is_empty() {
            if let Some(wt) = cur.take() {
                wts.push(wt);
            }
            continue;
        }
        let wt = cur.get_or_insert_with(Worktree::default);
        if let Some(p) = field.strip_prefix(b"worktree ") {
            wt.path = bytes_to_path(p);
        } else if let Some(h) = field.strip_prefix(b"HEAD ") {
            let h = String::from_utf8_lossy(h).into_owned();
            if !h.bytes().all(|c| c == b'0') {
                wt.head = Some(h);
            }
        } else if let Some(b) = field.strip_prefix(b"branch ") {
            wt.branch = Some(String::from_utf8_lossy(b).into_owned());
        } else if field == b"bare" {
            wt.bare = true;
        } else if field == b"detached" {
            wt.detached = true;
        } else if field == b"locked" {
            wt.locked = true;
        } else if let Some(r) = field.strip_prefix(b"locked ") {
            wt.locked = true;
            wt.lock_reason = Some(String::from_utf8_lossy(r).into_owned());
        } else if field == b"prunable" || field.starts_with(b"prunable ") {
            wt.prunable = true;
        }
    }
    if let Some(wt) = cur.take() {
        wts.push(wt);
    }
    if let Some(first) = wts.first_mut() {
        first.is_main = true;
    }
    wts.retain(|w| !w.path.as_os_str().is_empty());
    wts
}

// ---------------------------------------------------------------------------
// Trees

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub mode: u32,
    pub oid: String,
    pub size: u64,
}

impl Entry {
    pub fn is_regular(&self) -> bool {
        self.mode == MODE_REGULAR || self.mode == MODE_EXEC
    }
}

/// Every blob and gitlink of a commit, keyed by raw path bytes.
#[derive(Clone, Debug, Default)]
pub struct Tree {
    pub entries: HashMap<Vec<u8>, Entry>,
}

impl Tree {
    pub fn regular_count(&self) -> usize {
        self.entries.values().filter(|e| e.is_regular()).count()
    }
}

/// `git ls-tree -r -l -z <rev>`: "<mode> SP <type> SP <oid> SP+ <size> TAB <path>".
pub fn ls_tree(dir: &Path, rev: &str) -> Result<Tree> {
    let out = output(dir, &["ls-tree", "-r", "-l", "-z", rev])?;
    parse_ls_tree(&out)
}

pub fn parse_ls_tree(out: &[u8]) -> Result<Tree> {
    let mut tree = Tree::default();
    for rec in out.split(|&b| b == 0) {
        if rec.is_empty() {
            continue;
        }
        let Some(tab) = rec.iter().position(|&b| b == b'\t') else {
            bail!(
                "unexpected ls-tree record {:?}",
                String::from_utf8_lossy(rec)
            );
        };
        let head = std::str::from_utf8(&rec[..tab])?;
        let path = rec[tab + 1..].to_vec();
        let mut f = head.split_ascii_whitespace();
        let (Some(mode), Some(_typ), Some(oid), Some(size)) =
            (f.next(), f.next(), f.next(), f.next())
        else {
            bail!("unexpected ls-tree record {head:?}");
        };
        let mode = u32::from_str_radix(mode, 8)?;
        let size = size.parse::<u64>().unwrap_or(0);
        tree.entries.insert(
            path,
            Entry {
                mode,
                oid: oid.to_string(),
                size,
            },
        );
    }
    Ok(tree)
}

/// Stage-0 entries of a worktree's index: `git ls-files -s -z`.
pub fn ls_files_stage(dir: &Path) -> Result<Vec<(Vec<u8>, u32, String)>> {
    let out = output(dir, &["ls-files", "-s", "-z"])?;
    let mut v = Vec::new();
    for rec in out.split(|&b| b == 0) {
        if rec.is_empty() {
            continue;
        }
        let Some(tab) = rec.iter().position(|&b| b == b'\t') else {
            continue;
        };
        let head = String::from_utf8_lossy(&rec[..tab]);
        let mut f = head.split_ascii_whitespace();
        let (Some(mode), Some(oid), Some(stage)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        if stage != "0" {
            continue;
        }
        let Ok(mode) = u32::from_str_radix(mode, 8) else {
            continue;
        };
        v.push((rec[tab + 1..].to_vec(), mode, oid.to_string()));
    }
    Ok(v)
}

/// Split NUL-terminated output into owned records.
pub fn split_z(out: &[u8]) -> Vec<Vec<u8>> {
    out.split(|&b| b == 0)
        .filter(|r| !r.is_empty())
        .map(|r| r.to_vec())
        .collect()
}

/// Whether Git can run this hook from the repository.
pub fn hook_exists(worktree: &Path, name: &str) -> Result<bool> {
    let hook = git_path(worktree, &format!("hooks/{name}"))?;
    let Ok(md) = hook.metadata() else {
        return Ok(false);
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(md.is_file() && md.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    Ok(md.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_worktree_porcelain() {
        let out = b"worktree /r\0HEAD 1111111111111111111111111111111111111111\0branch refs/heads/main\0\0worktree /r/w\0HEAD 2222222222222222222222222222222222222222\0detached\0locked reason\0\0worktree /gone\0HEAD 0000000000000000000000000000000000000000\0prunable gitdir file points to non-existent location\0\0";
        let w = parse_worktrees(out);
        assert_eq!(w.len(), 3);
        assert!(w[0].is_main && !w[1].is_main);
        assert_eq!(w[0].branch.as_deref(), Some("refs/heads/main"));
        assert!(w[1].detached && w[1].locked);
        assert!(w[2].prunable && w[2].head.is_none());
    }

    #[test]
    fn parses_ls_tree_with_odd_names() {
        let out = b"100644 blob aaaa    12\ta b\tc\x00100755 blob bbbb 7\tx\n\xff\x00160000 commit cccc       -\tsub\0";
        let t = parse_ls_tree(out).unwrap();
        assert_eq!(t.entries[&b"a b\tc".to_vec()].size, 12);
        assert_eq!(t.entries[&b"x\n\xff".to_vec()].mode, MODE_EXEC);
        assert_eq!(t.entries[&b"sub".to_vec()].mode, 0o160000);
        assert_eq!(t.regular_count(), 2);
    }
}
