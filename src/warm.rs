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

    fn could_contain(&self, rel: &[&[u8]]) -> bool {
        if !self.anchored {
            return true;
        }
        rel.len() < self.comps.len() && self.comps.iter().zip(rel).all(|(p, c)| glob(p, c))
    }
}

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
    created: Vec<(PathBuf, bool)>,
}

fn comps(rel: &[u8]) -> Vec<&[u8]> {
    rel.split(|&b| b == b'/')
        .filter(|c| !c.is_empty())
        .collect()
}

impl Ctx<'_> {
    fn destination_parents(&mut self, rel: &Path) -> Result<bool, Abort> {
        let mut directory = self.dest.to_path_buf();
        for part in rel.parent().into_iter().flat_map(Path::components) {
            let std::path::Component::Normal(name) = part else {
                return Ok(false);
            };
            directory.push(name);
            match directory.symlink_metadata() {
                Ok(m) if m.is_dir() => {}
                Ok(_) => return Ok(false),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    fs::create_dir(&directory).map_err(|e| Abort::Io(e.into()))?;
                    self.created.push((directory.clone(), true));
                }
                Err(e) => return Err(Abort::Io(e.into())),
            }
        }
        Ok(true)
    }

    fn rollback(&mut self, start: usize) -> std::io::Result<()> {
        for (path, directory) in &self.created[start..] {
            if *directory {
                if let Ok(m) = path.symlink_metadata() {
                    if m.is_dir() {
                        let _ =
                            fs::set_permissions(path, fs::Permissions::from_mode(m.mode() | 0o700));
                    }
                }
            }
        }
        let mut result = Ok(());
        for (path, directory) in self.created.drain(start..).rev() {
            let removed = if directory {
                fs::remove_dir(&path)
            } else {
                fs::remove_file(&path)
            };
            if let Err(e) = removed {
                if e.kind() != std::io::ErrorKind::NotFound && result.is_ok() {
                    result = Err(e);
                }
            }
        }
        result
    }

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
            let relpath = bytes_to_path(rel);
            let start = self.created.len();
            if !self.destination_parents(&relpath)? {
                self.rollback(start).map_err(|e| Abort::Io(e.into()))?;
                st.skipped
                    .push((display, "destination parent is not a directory"));
                return Ok(());
            }
            let dst = self.dest.join(relpath);
            let before = st.files + st.links;
            self.clone_tree(rel, &src, &md, &dst, st)?;
            if before == st.files + st.links {
                self.rollback(start).map_err(|e| Abort::Io(e.into()))?;
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
            let created = match dst.symlink_metadata() {
                Ok(m) if m.is_dir() => false,
                Ok(_) => return Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    fs::DirBuilder::new()
                        .mode(0o700)
                        .create(dst)
                        .map_err(|e| Abort::Io(e.into()))?;
                    self.created.push((dst.to_owned(), true));
                    true
                }
                Err(e) => return Err(Abort::Io(e.into())),
            };
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
            if created {
                fs::set_permissions(dst, fs::Permissions::from_mode(md.mode() & 0o7777))
                    .map_err(|e| Abort::Io(e.into()))?;
            }
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
            self.created.push((dst.to_owned(), false));
            st.links += 1;
        } else if ft.is_file() {
            if dst.symlink_metadata().is_ok() {
                return Ok(());
            }
            match reflink::clone_preserving(src, dst) {
                Ok(n) => {
                    self.created.push((dst.to_owned(), false));
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
        .filter(|p| *p != donor_canon && p.starts_with(&donor_canon))
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
        let cleanup = ctx.rollback(0);
        match abort {
            Abort::Unsupported(why) => {
                cleanup.with_context(|| {
                    format!("--warm stopped and cleanup failed after reflink was refused ({why})")
                })?;
                eprintln!("sharetree: --warm stopped, nothing copied: reflink refused ({why}); caches are never byte-copied");
                return Ok(());
            }
            Abort::Io(e) => {
                cleanup
                    .with_context(|| format!("--warm cleanup failed after clone error ({e:#})"))?;
                bail!(
                    "--warm failed (partial clones removed): {e:#}\nfile clones rolled back = {}",
                    st.files
                );
            }
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
    fn rollback_preserves_existing_cache_directory_and_mode() {
        let dir = tempfile::tempdir().unwrap();
        let donor = dir.path().join("donor");
        let dest = dir.path().join("dest");
        let donor_cache = donor.join("tracked/cache");
        fs::create_dir_all(donor_cache.join("nested")).unwrap();
        fs::set_permissions(&donor_cache, fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink("asset", donor_cache.join("nested/link")).unwrap();
        let cache = dest.join("tracked/cache");
        fs::create_dir_all(&cache).unwrap();
        fs::set_permissions(&cache, fs::Permissions::from_mode(0o750)).unwrap();
        let original = cache.join("original");
        fs::write(&original, "keep").unwrap();

        let mut ctx = Ctx {
            donor: &donor,
            donor_spellings: vec![],
            dest: &dest,
            holes: vec![],
            clone: vec![],
            skip: vec![],
            created: vec![],
        };
        assert!(ctx.select(b"tracked/cache", &mut Stats::default()).is_ok());
        assert_eq!(
            fs::read_link(cache.join("nested/link")).unwrap(),
            Path::new("asset")
        );
        let mode = || fs::metadata(&cache).unwrap().mode() & 0o7777;
        assert_eq!(mode(), 0o750);
        ctx.rollback(0).unwrap();
        assert_eq!(fs::read(original).unwrap(), b"keep");
        assert!(!cache.join("nested").exists());
        assert_eq!(mode(), 0o750);
    }

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
