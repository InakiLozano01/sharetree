//! `sharetree add --warm`: the ignored-cache layer (design §8.4).
//!
//! Reflinks ignored paths (node_modules, target/, ...) from the donor so the
//! first build in the new worktree starts warm. Rules:
//!
//! * reflink only; if the filesystem refuses, nothing is copied at all;
//! * never: directories holding `pyvenv.cfg` or `CMakeCache.txt` (they embed
//!   absolute paths; oh-my-pi `prune_nonrelocatable_clone_dirs`), `.env*`
//!   files (secrets), nested repositories/worktrees, other worktrees and the
//!   destination itself (oh-my-pi "clone holes", lane);
//! * absolute symlinks that point into the donor are retargeted into the new
//!   worktree (lane `cow.rs` retarget);
//! * a `.sharetree.toml` policy narrows what is cloned and adds skips.
//!
//! ```toml
//! [warm]
//! clone = ["node_modules", "target/release"]  # default: every ignored path
//! skip  = ["*.log", "target/debug/incremental"]
//! ```
//!
//! A pattern without `/` matches a name at any depth; with `/` it is anchored
//! at the worktree root. `*` and `?` match within one path component.

use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::git::{self, Worktree};
use crate::reflink::{self, CloneError};
use crate::util::{self, bytes_to_path, human, Trace};

pub const POLICY_FILE: &str = ".sharetree.toml";

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PolicyFile {
    warm: WarmPolicy,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WarmPolicy {
    pub clone: Vec<String>,
    pub skip: Vec<String>,
}

#[derive(Debug, Clone)]
struct Pattern {
    comps: Vec<Vec<u8>>,
    anchored: bool,
}

impl Pattern {
    fn parse(s: &str) -> Option<Pattern> {
        let s = s.trim().trim_start_matches("./").trim_matches('/');
        if s.is_empty() {
            return None;
        }
        Some(Pattern {
            anchored: s.contains('/'),
            comps: s.split('/').map(|c| c.as_bytes().to_vec()).collect(),
        })
    }

    fn matches(&self, rel: &[&[u8]]) -> bool {
        if self.anchored {
            rel.len() == self.comps.len() && self.comps.iter().zip(rel).all(|(p, c)| glob(p, c))
        } else {
            rel.last().is_some_and(|c| glob(&self.comps[0], c))
        }
    }

    /// Could something below `rel` match?
    fn could_contain(&self, rel: &[&[u8]]) -> bool {
        if !self.anchored {
            return true;
        }
        rel.len() < self.comps.len() && self.comps.iter().zip(rel).all(|(p, c)| glob(p, c))
    }
}

/// `*` and `?` within one path component.
fn glob(p: &[u8], s: &[u8]) -> bool {
    let (mut pi, mut si) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while si < s.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            pi += 1;
            mark = si;
        } else if let Some(st) = star {
            pi = st + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

pub fn load_policy(dest: &Path, donor: &Path) -> Result<WarmPolicy> {
    for root in [dest, donor] {
        let f = root.join(POLICY_FILE);
        if let Ok(text) = fs::read_to_string(&f) {
            let p: PolicyFile =
                toml::from_str(&text).with_context(|| format!("parsing {}", f.display()))?;
            return Ok(p.warm);
        }
    }
    Ok(WarmPolicy::default())
}

#[derive(Default)]
struct Stats {
    files: u64,
    bytes: u64,
    links: u64,
    retargeted: u64,
    skipped: Vec<(String, &'static str)>,
    roots: Vec<String>,
}

enum Abort {
    Unsupported(String),
    Io(anyhow::Error),
}

struct Ctx<'a> {
    donor: &'a Path,
    donor_spellings: Vec<PathBuf>,
    dest: &'a Path,
    holes: Vec<PathBuf>,
    clone: Vec<Pattern>,
    skip: Vec<Pattern>,
    created: Vec<PathBuf>,
}

fn comps(rel: &[u8]) -> Vec<&[u8]> {
    rel.split(|&b| b == b'/')
        .filter(|c| !c.is_empty())
        .collect()
}

impl Ctx<'_> {
    /// Reasons a path is never cloned, checked at every depth.
    fn veto(&self, rel: &[u8], src: &Path, md: &fs::Metadata) -> Option<&'static str> {
        let cs = comps(rel);
        let canon = util::canon(src);
        if self.holes.iter().any(|h| canon.starts_with(h)) {
            return Some("another worktree");
        }
        if self.skip.iter().any(|p| p.matches(&cs)) {
            return Some("skip policy");
        }
        let name = cs.last().copied().unwrap_or_default();
        if md.is_dir() {
            if src.join("pyvenv.cfg").exists() {
                return Some("pyvenv.cfg: venvs hold absolute paths");
            }
            if src.join("CMakeCache.txt").exists() {
                return Some("CMakeCache.txt: CMake trees hold absolute paths");
            }
            if src.join(".git").symlink_metadata().is_ok() {
                return Some("nested repository or worktree");
            }
        } else if name.starts_with(b".env") {
            return Some("untracked .env* file (secrets)");
        }
        None
    }

    fn retarget(&self, link: &Path) -> Option<PathBuf> {
        if !link.is_absolute() {
            return None;
        }
        self.donor_spellings
            .iter()
            .find_map(|root| link.strip_prefix(root).ok())
            .map(|rel| self.dest.join(rel))
    }

    /// Selection walk: find the paths the policy asks for.
    fn select(&mut self, rel: &[u8], st: &mut Stats) -> Result<(), Abort> {
        let src = self.donor.join(bytes_to_path(rel));
        let Ok(md) = src.symlink_metadata() else {
            return Ok(());
        };
        let display = String::from_utf8_lossy(rel).into_owned();
        if let Some(why) = self.veto(rel, &src, &md) {
            st.skipped.push((display, why));
            return Ok(());
        }
        let cs = comps(rel);
        let selected = self.clone.is_empty() || self.clone.iter().any(|p| p.matches(&cs));
        if selected {
            let dst = self.dest.join(bytes_to_path(rel));
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent).map_err(|e| Abort::Io(e.into()))?;
            }
            let fresh = dst.symlink_metadata().is_err();
            if fresh {
                self.created.push(dst.clone());
            }
            self.clone_tree(rel, &src, &md, &dst, st)?;
            // A directory that only held vetoed paths (e.g. `.worktrees/`
            // holding other worktrees) leaves nothing behind.
            if fresh && md.is_dir() && fs::read_dir(&dst).is_ok_and(|mut d| d.next().is_none()) {
                let _ = fs::remove_dir(&dst);
                self.created.pop();
            } else {
                st.roots.push(display);
            }
            return Ok(());
        }
        if md.is_dir() && self.clone.iter().any(|p| p.could_contain(&cs)) {
            let entries = fs::read_dir(&src).map_err(|e| Abort::Io(e.into()))?;
            let mut names: Vec<Vec<u8>> = entries
                .filter_map(|e| e.ok())
                .map(|e| util::path_to_bytes(Path::new(&e.file_name())))
                .collect();
            names.sort();
            for n in names {
                let mut child = rel.to_vec();
                child.push(b'/');
                child.extend_from_slice(&n);
                self.select(&child, st)?;
            }
        }
        Ok(())
    }

    fn clone_tree(
        &mut self,
        rel: &[u8],
        src: &Path,
        md: &fs::Metadata,
        dst: &Path,
        st: &mut Stats,
    ) -> Result<(), Abort> {
        let ft = md.file_type();
        if ft.is_dir() {
            match dst.symlink_metadata() {
                Ok(m) if m.is_dir() => {}
                Ok(_) => return Ok(()), // a tracked file sits there; git wins
                Err(_) => fs::DirBuilder::new()
                    .mode(md.mode() & 0o7777)
                    .create(dst)
                    .map_err(|e| Abort::Io(e.into()))?,
            }
            let entries = fs::read_dir(src).map_err(|e| Abort::Io(e.into()))?;
            for e in entries {
                let e = e.map_err(|e| Abort::Io(e.into()))?;
                let name = util::path_to_bytes(Path::new(&e.file_name()));
                let mut child = rel.to_vec();
                child.push(b'/');
                child.extend_from_slice(&name);
                let csrc = e.path();
                let Ok(cmd) = csrc.symlink_metadata() else {
                    continue;
                };
                if let Some(why) = self.veto(&child, &csrc, &cmd) {
                    st.skipped
                        .push((String::from_utf8_lossy(&child).into_owned(), why));
                    continue;
                }
                self.clone_tree(&child, &csrc, &cmd, &dst.join(e.file_name()), st)?;
            }
            // Keep the directory's own permissions even if it pre-existed.
            let _ = fs::set_permissions(dst, fs::Permissions::from_mode(md.mode() & 0o7777));
        } else if ft.is_symlink() {
            if dst.symlink_metadata().is_ok() {
                return Ok(());
            }
            let target = fs::read_link(src).map_err(|e| Abort::Io(e.into()))?;
            let target = match self.retarget(&target) {
                Some(t) => {
                    st.retargeted += 1;
                    t
                }
                None => target,
            };
            std::os::unix::fs::symlink(&target, dst).map_err(|e| Abort::Io(e.into()))?;
            st.links += 1;
        } else if ft.is_file() {
            if dst.symlink_metadata().is_ok() {
                return Ok(());
            }
            match reflink::clone_preserving(src, dst) {
                Ok(n) => {
                    st.files += 1;
                    st.bytes += n;
                }
                Err(CloneError::Unsupported(e)) => {
                    return Err(Abort::Unsupported(format!("{}: {e}", src.display())));
                }
                Err(CloneError::SourceChanged) => {}
                Err(CloneError::Io(e)) => {
                    return Err(Abort::Io(
                        anyhow::Error::from(e).context(format!("cloning {}", src.display())),
                    ));
                }
            }
        }
        Ok(())
    }
}

pub fn run(donor: &Path, dest: &Path, wts: &[Worktree], quiet: bool, trace: &Trace) -> Result<()> {
    let t = Instant::now();
    let policy = load_policy(dest, donor)?;
    let out = git::output(
        donor,
        &[
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
        ],
    )?;
    let mut entries = git::split_z(&out);
    entries.sort();

    let mut donor_spellings = vec![donor.to_path_buf()];
    let c = util::canon(donor);
    if c != donor {
        donor_spellings.push(c);
    }
    let donor_canon = util::canon(donor);
    let mut holes: Vec<PathBuf> = wts
        .iter()
        .map(|w| util::canon(&w.path))
        .filter(|p| *p != donor_canon)
        .collect();
    holes.push(util::canon(dest));
    holes.push(util::canon(&git::common_dir(donor)?));

    let mut ctx = Ctx {
        donor,
        donor_spellings,
        dest,
        holes,
        clone: policy
            .clone
            .iter()
            .filter_map(|s| Pattern::parse(s))
            .collect(),
        skip: policy
            .skip
            .iter()
            .filter_map(|s| Pattern::parse(s))
            .collect(),
        created: Vec::new(),
    };
    let mut st = Stats::default();
    let mut result = Ok(());
    for e in &entries {
        let rel = e.strip_suffix(b"/").unwrap_or(e);
        if let Err(abort) = ctx.select(rel, &mut st) {
            result = Err(abort);
            break;
        }
    }
    if let Err(abort) = result {
        for p in ctx.created.iter().rev() {
            let _ = if p.is_dir() {
                fs::remove_dir_all(p)
            } else {
                fs::remove_file(p)
            };
        }
        match abort {
            Abort::Unsupported(why) => {
                eprintln!("sharetree: --warm stopped, nothing copied: reflink refused ({why}); caches are never byte-copied");
                return Ok(());
            }
            Abort::Io(e) => bail!("--warm failed (partial clones removed): {e:#}"),
        }
    }
    trace.phase("warm", t);
    if !quiet {
        if st.roots.is_empty() {
            eprintln!(
                "sharetree: warm: no ignored paths selected in {}",
                donor.display()
            );
        } else {
            let mut roots = st.roots.clone();
            roots.truncate(8);
            eprintln!(
                "sharetree: warm: reflinked {} files ({}) and {} symlinks ({} retargeted) from {}",
                st.files,
                human(st.bytes),
                st.links,
                st.retargeted,
                roots.join(", ") + if st.roots.len() > 8 { ", ..." } else { "" }
            );
            eprintln!("sharetree: warm: caches diverge as builds rewrite them; `sharetree du` shows the real cost");
        }
        for (p, why) in st
            .skipped
            .iter()
            .filter(|(_, w)| *w != "another worktree")
            .take(10)
        {
            eprintln!("sharetree: warm: skipped {p} ({why})");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globbing() {
        assert!(glob(b"*.log", b"a.log"));
        assert!(!glob(b"*.log", b"a.logx"));
        assert!(glob(b"node_?odules", b"node_modules"));
        assert!(glob(b"*", b""));
    }

    #[test]
    fn patterns() {
        let p = Pattern::parse("node_modules").unwrap();
        assert!(p.matches(&comps(b"a/b/node_modules")));
        assert!(p.could_contain(&comps(b"a")));
        let q = Pattern::parse("target/release/").unwrap();
        assert!(q.matches(&comps(b"target/release")));
        assert!(!q.matches(&comps(b"x/target/release")));
        assert!(q.could_contain(&comps(b"target")));
        assert!(!q.could_contain(&comps(b"build")));
    }

    #[test]
    fn policy_parses() {
        let p: PolicyFile =
            toml::from_str("[warm]\nclone=[\"node_modules\"]\nskip=[\"*.log\"]\n").unwrap();
        assert_eq!(p.warm.clone, vec!["node_modules"]);
        assert!(toml::from_str::<PolicyFile>("[warm]\nbogus=1\n").is_err());
    }
}
