//! `sharetree add`: the tracked-file layer (design §8.3).
//!
//! 1. Preflight: anything unusual delegates to plain `git worktree add`.
//! 2. Probe a real reflink next to the destination.
//! 3. `git worktree add --no-checkout`, overlapped with donor analysis.
//! 4. Plan: target regular files a clean donor file holds byte-for-byte,
//!    without checkout conversions, inside the sparse cone.
//! 5. Clone in parallel; re-`fstat` the donor after each clone.
//! 6. Hash every clone in-process (sha1dc / SHA-256); drop mismatches.
//! 7. Write a v2 index of verified entries with their real lstat data.
//! 8. `git checkout -f HEAD` writes the rest (retry plainly on rejection).
//! 9. Run post-checkout like `git worktree add` would.
//!
//! Flow and index trick from josharian/git-cow-worktree (MIT); donor scoring
//! by matching entries from leostera/git-twig (MIT). See THIRD_PARTY_NOTICES.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use crate::backend::{self, Decision, Hint};
use crate::config::{Config, Volume};
use crate::{git, util, volume};

/// Files at or below btrfs's default `max_inline` (2048 bytes) live in
/// metadata and can never share an extent; above it every file costs at
/// least one 4 KiB block, which a reflink saves.
pub const DEFAULT_MIN_SIZE: u64 = 2049;

#[derive(Debug, Default, Clone)]
pub struct AddArgs {
    pub verbose: bool,
    pub quiet: bool,
    pub from: Option<PathBuf>,
    pub warm: bool,
    pub no_cow: bool,
    pub min_size: u64,
    /// Options forwarded to our internal `git worktree add --no-checkout`.
    pub forward: Vec<OsString>,
    /// The user's arguments minus sharetree's own, for a plain passthrough.
    pub git_args: Vec<OsString>,
    pub separator: bool,
    pub path: Option<OsString>,
    pub commitish: Option<OsString>,
    pub no_checkout: bool,
    pub orphan: bool,
    /// `--into-volume`: place the worktree in the configured sharetree volume.
    pub into_volume: bool,
    /// `--no-volume`: never use a volume.
    pub no_volume: bool,
    /// `--overlay`: fuse-overlayfs worktree over a base checkout (Linux).
    pub overlay: bool,
    /// `--windows-clone`: enable ReFS block cloning on Windows.
    pub windows_clone: bool,
    /// Backend already chosen (relocated into a volume, or a seed): skip
    /// selection.
    pub placed: bool,
    /// sharetree's own seed checkout: no hooks, no summary, no advice.
    pub internal: bool,
    /// Preferred donor (the volume's seed).
    pub extra_donor: Option<PathBuf>,
}

impl AddArgs {
    /// The same request for another path, optionally locked with `reason`
    /// (unless the user already passed --lock).
    pub fn relocated(&self, path: &Path, lock_reason: Option<&str>) -> AddArgs {
        let mut a = self.clone();
        if let Some(r) = lock_reason {
            if !a.forward.iter().any(|f| f == "--lock") {
                a.forward
                    .extend(["--lock".into(), "--reason".into(), OsString::from(r)]);
            }
        }
        a.path = Some(path.as_os_str().to_owned());
        a.git_args = a.forward.clone();
        if a.separator {
            a.git_args.push("--".into());
        }
        a.git_args.push(path.as_os_str().to_owned());
        if let Some(c) = &a.commitish {
            a.git_args.push(c.clone());
        }
        a.placed = true;
        a
    }
}

fn parse_size(v: Option<&OsString>) -> Result<u64> {
    let v = v
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("--min-size requires a number"))?;
    v.parse()
        .map_err(|_| anyhow!("--min-size: not a number: {v}"))
}

pub fn parse(args: &[OsString]) -> Result<AddArgs> {
    let mut a = AddArgs {
        min_size: DEFAULT_MIN_SIZE,
        ..Default::default()
    };
    let mut positional: Vec<OsString> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        match arg.to_str() {
            Some("--") => {
                a.separator = true;
                a.git_args.push(arg.clone());
                for r in &args[i + 1..] {
                    positional.push(r.clone());
                    a.git_args.push(r.clone());
                }
                break;
            }
            Some("-v" | "--verbose") => a.verbose = true,
            Some("--warm") => a.warm = true,
            Some("--no-warm") => a.warm = false,
            Some("--no-cow") => a.no_cow = true,
            Some("--into-volume") => a.into_volume = true,
            Some("--overlay") => a.overlay = true,
            Some("--windows-clone") => a.windows_clone = true,
            Some("--no-volume") => a.no_volume = true,
            Some("--from") => {
                i += 1;
                let p = args
                    .get(i)
                    .ok_or_else(|| anyhow!("--from requires a path"))?;
                a.from = Some(PathBuf::from(p));
            }
            Some(s) if s.starts_with("--from=") => a.from = Some(PathBuf::from(&s[7..])),
            Some("--min-size") => {
                i += 1;
                a.min_size = parse_size(args.get(i))?;
            }
            Some(s) if s.starts_with("--min-size=") => {
                a.min_size = parse_size(Some(&OsString::from(&s[11..])))?;
            }
            Some("--no-checkout") => {
                a.no_checkout = true;
                a.git_args.push(arg.clone());
            }
            // Never forwarded internally: it would override --no-checkout.
            Some("--checkout") => {
                a.no_checkout = false;
                a.git_args.push(arg.clone());
            }
            Some("--orphan") => {
                a.orphan = true;
                a.git_args.push(arg.clone());
            }
            Some("-q" | "--quiet") => {
                a.quiet = true;
                a.forward.push(arg.clone());
                a.git_args.push(arg.clone());
            }
            Some("-b" | "-B" | "--reason") => {
                let v = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("{} requires a value", arg.to_string_lossy()))?;
                for x in [arg, v] {
                    a.forward.push(x.clone());
                    a.git_args.push(x.clone());
                }
                i += 1;
            }
            Some(s) if s.starts_with('-') && s.len() > 1 => {
                a.forward.push(arg.clone());
                a.git_args.push(arg.clone());
            }
            _ => {
                positional.push(arg.clone());
                a.git_args.push(arg.clone());
            }
        }
        i += 1;
    }
    if positional.len() > 2 {
        bail!("unexpected argument {:?}", positional[2]);
    }
    let mut pos = positional.into_iter();
    a.path = pos.next();
    a.commitish = pos.next();
    Ok(a)
}

pub fn run(args: &[OsString]) -> Result<i32> {
    let a = parse(args)?;
    if a.path.is_none() {
        bail!("usage: sharetree add [<options>] <path> [<commit-ish>]");
    }
    let (code, made) = if a.overlay {
        #[cfg(target_os = "linux")]
        {
            if a.into_volume || a.windows_clone {
                bail!("--overlay cannot be combined with --into-volume or --windows-clone");
            }
            crate::overlay::add(&a)?
        }
        #[cfg(not(target_os = "linux"))]
        bail!("--overlay (fuse-overlayfs) is available on Linux only")
    } else {
        dispatch(&a)?
    };
    if code == 0 && !a.quiet && !a.internal {
        crate::advise::print_add_hint(&made);
    }
    Ok(code)
}

#[cfg(unix)]
fn dispatch(a: &AddArgs) -> Result<(i32, PathBuf)> {
    if a.windows_clone {
        eprintln!("sharetree: --windows-clone has no effect outside Windows");
    }
    engine::run(a)
}

/// Windows: the clone engine runs only when block cloning was asked for
/// (`--windows-clone` or `windows_clone = true`), with a loud note.
#[cfg(windows)]
fn dispatch(a: &AddArgs) -> Result<(i32, PathBuf)> {
    let enabled = a.windows_clone
        || Config::load()
            .ok()
            .and_then(|c| c.windows_clone)
            .unwrap_or(false);
    if !enabled {
        return run_without_engine(a);
    }
    if !a.quiet {
        eprintln!("sharetree: note: {}", crate::winclone::NOTE);
    }
    engine::run(a)
}

#[cfg(not(any(unix, windows)))]
fn dispatch(a: &AddArgs) -> Result<(i32, PathBuf)> {
    run_without_engine(a)
}

/// What backend selection needs to know about the volume.
pub(crate) struct Selection {
    pub decision: Decision,
    pub common: PathBuf,
    pub volume: Option<Volume>,
}

/// Backend selection (research note §7.2); see `backend.rs` for the policy.
pub(crate) fn select(a: &AddArgs, cwd: &Path, dest: &Path, probe_ok: bool) -> Result<Selection> {
    let common = git::common_dir(cwd)?;
    if a.placed || a.internal {
        return Ok(Selection {
            decision: if probe_ok {
                Decision::Reflink { seed: false }
            } else {
                Decision::Plain(Hint::None)
            },
            common,
            volume: None,
        });
    }
    let cfg = Config::load().unwrap_or_else(|e| {
        eprintln!("sharetree: warning: ignoring the config: {e:#}");
        Config::default()
    });
    let volume = cfg.volume_for(&common).cloned();
    let dest_in_volume = volume
        .as_ref()
        .and_then(|v| volume::current_state(v).mountpoint().map(Path::to_path_buf))
        .is_some_and(|mp| util::is_within(&util::nearest_existing(dest), &mp));
    let mut decision = backend::decide(
        &backend::Inputs {
            probe_ok,
            dest_in_volume,
            volume: volume.as_ref().map(|v| v.name.clone()),
            auto_volume: cfg.auto_volume_for(&common),
            into_volume: a.into_volume,
            no_volume: a.no_volume,
        },
        backend::Platform::current(),
    );
    if let Decision::Ask(v) = &decision {
        let answer = if can_prompt() {
            Some(ask_volume(v, dest, &common)?)
        } else {
            None
        };
        decision = backend::resolve_ask(v, answer);
    }
    Ok(Selection {
        decision,
        common,
        volume,
    })
}

/// Prompt only when a person is there to answer: stdin and stderr are
/// terminals, and `SHARETREE_NO_PROMPT` is not set (agents and scripts that
/// run inside a pseudo-terminal set it so `add` never waits for input).
fn can_prompt() -> bool {
    use std::io::IsTerminal;
    std::env::var_os("SHARETREE_NO_PROMPT").is_none_or(|v| v.is_empty() || v == "0")
        && std::io::stdin().is_terminal()
        && std::io::stderr().is_terminal()
}

/// Ask whether to put this worktree in volume `v`, and store an
/// "always"/"never" answer as this repository's `auto_volume`.
fn ask_volume(v: &str, dest: &Path, common: &Path) -> Result<backend::Answer> {
    use std::io::{BufRead, Write};
    let anchor = util::nearest_existing(dest.parent().unwrap_or(dest));
    let fs = fs_label(&anchor);
    eprintln!(
        "sharetree: {} ({fs}) cannot share blocks between files, so git would write a full \
         checkout there.\n\
         sharetree: volume '{v}' can hold this worktree instead (it shares blocks with a seed \
         checkout; a symlink is left at {}).",
        anchor.display(),
        dest.display()
    );
    let stdin = std::io::stdin();
    let answer = loop {
        eprint!("Use volume '{v}'? {} [n]: ", backend::ASK_CHOICES);
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            eprintln!();
            break backend::Answer::NotNow; // EOF: change nothing
        }
        match backend::parse_answer(&line) {
            Some(a) => break a,
            None => eprintln!("sharetree: please answer y, a, n or v"),
        }
    };
    if let Some(on) = answer.persist() {
        let mut cfg = Config::load()?;
        cfg.repo_mut(common).auto_volume = Some(on);
        let p = cfg.save()?;
        eprintln!(
            "sharetree: saved auto_volume = {on} for this repository in {} \
             (change it with `sharetree setup auto on|off --repo`)",
            p.display()
        );
    }
    Ok(answer)
}

fn fs_label(p: &Path) -> String {
    #[cfg(target_os = "linux")]
    {
        crate::linux::fs_name(p).to_string()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = p;
        "this filesystem".to_string()
    }
}

/// Where a worktree goes inside a volume: `<root>/<repo-id>/<name>`, or the
/// requested path itself when it already lies inside the volume.
pub(crate) struct Placement {
    pub requested: PathBuf,
    pub real: PathBuf,
    pub repo_root: PathBuf,
    pub args: AddArgs,
}

pub(crate) fn place(a: &AddArgs, cwd: &Path, common: &Path, v: &Volume) -> Result<Placement> {
    let requested = util::absolutize(Path::new(a.path.as_ref().expect("checked")), cwd);
    let mp = volume::mount_any(v, a.quiet)?;
    let main = git::list_worktrees(cwd)?
        .into_iter()
        .find(|w| w.is_main)
        .map(|w| w.path)
        .unwrap_or_else(|| common.to_path_buf());
    let repo_root = volume::worktrees_root(v, &mp).join(volume::repo_id(common, &main));
    std::fs::create_dir_all(&repo_root)
        .with_context(|| format!("creating {}", repo_root.display()))?;
    let real = if util::is_within(&util::nearest_existing(&requested), &mp) {
        requested.clone()
    } else {
        if let Ok(md) = requested.symlink_metadata() {
            let empty_dir =
                md.is_dir() && std::fs::read_dir(&requested).is_ok_and(|mut r| r.next().is_none());
            if !empty_dir {
                bail!(
                    "'{}' already exists (the worktree would live in the volume, with a symlink here)",
                    requested.display()
                );
            }
        }
        let name = requested
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "worktree".into());
        volume::unique_child(&repo_root, &name)
    };
    let reason = volume::volume_lock_reason(&v.name);
    let args = a.relocated(&real, Some(&reason));
    Ok(Placement {
        requested,
        real,
        repo_root,
        args,
    })
}

/// After a successful add into the volume: leave a symlink where the user
/// asked for the worktree, and remember the repository in the config.
pub(crate) fn finish_placement(
    p: &Placement,
    common: &Path,
    v: &Volume,
    quiet: bool,
) -> Result<()> {
    let linked = cfg!(unix) && p.real != p.requested;
    if p.real != p.requested && p.requested.is_dir() {
        let _ = std::fs::remove_dir(&p.requested); // empty, checked before
    }
    #[cfg(unix)]
    if linked {
        if let Some(parent) = p.requested.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::os::unix::fs::symlink(&p.real, &p.requested)
            .with_context(|| format!("creating the symlink {}", p.requested.display()))?;
    }
    if !quiet {
        eprintln!(
            "sharetree: the worktree lives in volume '{}' at {}{}",
            v.name,
            p.real.display(),
            if linked {
                format!(" (symlink: {})", p.requested.display())
            } else {
                String::new()
            }
        );
    }
    let mut cfg = Config::load()?;
    if cfg.repo(common).is_none() {
        cfg.repo_mut(common);
        cfg.save()?;
    }
    Ok(())
}

/// Windows (and other non-unix): no clone engine; place in the Dev Drive
/// when asked, otherwise plain git with a hint.
#[cfg(not(unix))]
fn run_without_engine(a: &AddArgs) -> Result<(i32, PathBuf)> {
    let cwd = std::env::current_dir()?;
    let dest = util::absolutize(Path::new(a.path.as_ref().expect("checked")), &cwd);
    let why = "copy-on-write worktrees are not enabled on this platform (ReFS block cloning is off by default, see README)";
    if a.no_cow || a.no_checkout || a.orphan || git::common_dir(&cwd).is_err() {
        return Ok((delegate(a, why)?, dest));
    }
    let sel = select(a, &cwd, &dest, false)?;
    match sel.decision {
        Decision::Fail(m) => bail!(m),
        Decision::IntoVolume(_) => {
            let v = sel.volume.expect("decided with a volume");
            let p = place(a, &cwd, &sel.common, &v)?;
            let code = delegate(
                &p.args,
                "placing the worktree in the Dev Drive (sharetree does not block-clone on Windows yet)",
            )?;
            if code == 0 {
                finish_placement(&p, &sel.common, &v, a.quiet)?;
            }
            Ok((code, p.real))
        }
        Decision::Plain(h) => {
            let code = delegate(a, why)?;
            if let (Some(t), false) = (backend::hint_text(&h, "NTFS"), a.quiet) {
                eprintln!("sharetree: {t}");
            }
            Ok((code, dest))
        }
        Decision::Reflink { .. } => Ok((delegate(a, why)?, dest)),
        Decision::Ask(_) => unreachable!("resolved in select()"),
    }
}

/// Run stock `git worktree add` with the user's arguments, unchanged.
pub fn delegate(a: &AddArgs, why: &str) -> Result<i32> {
    if !a.quiet {
        eprintln!("sharetree: {why}; running plain `git worktree add`");
    }
    let cwd = std::env::current_dir()?;
    let mut args: Vec<OsString> = Vec::new();
    if a.internal {
        args.extend(["-c".into(), "core.hooksPath=/dev/null".into()]);
    }
    args.extend(["worktree".into(), "add".into()]);
    args.extend(a.git_args.iter().cloned());
    let st = git::run_inherit(&cwd, &args)?;
    Ok(st.code().unwrap_or(1))
}

#[cfg(any(unix, windows))]
mod engine {
    use std::collections::{HashMap, HashSet};
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::Instant;

    use anyhow::{bail, Context, Result};

    use super::{delegate, AddArgs, Decision, DEFAULT_MIN_SIZE};
    use crate::attrs;
    use crate::backend;
    use crate::git::{self, Entry, Tree, MODE_EXEC};
    use crate::index::{self, IndexEntry, Stat};
    use crate::reflink::{self, CloneError};
    use crate::util::{self, bytes_to_path, human, Trace};
    use crate::volume;

    #[derive(Clone, Debug)]
    struct Cand {
        path: PathBuf,
        head: String,
    }

    struct Donor {
        path: PathBuf,
        tree: Tree,
        dirty: HashSet<Vec<u8>>,
        score: usize,
    }

    struct Analysis {
        sha: String,
        target: Tree,
        donor: Option<Donor>,
    }

    #[derive(Default)]
    struct SeedReport {
        donor: Option<PathBuf>,
        index_written: bool,
        planned: usize,
        cloned: usize,
        verified: usize,
        bytes: u64,
        unsupported: usize,
    }

    use super::fs_label as fs_desc;

    pub fn run(a: &AddArgs) -> Result<(i32, PathBuf)> {
        let cwd = std::env::current_dir()?;
        let dest_arg = a.path.as_ref().expect("checked by caller");
        let dest = util::absolutize(Path::new(dest_arg), &cwd);
        let plain = |why: &str| -> Result<(i32, PathBuf)> { Ok((delegate(a, why)?, dest.clone())) };
        if a.no_cow {
            return plain("--no-cow given");
        }
        if a.no_checkout {
            return plain("--no-checkout: there is nothing to clone");
        }
        if a.orphan {
            return plain("--orphan: there is nothing to clone");
        }
        if std::env::var_os("GIT_INDEX_FILE").is_some() {
            return plain("GIT_INDEX_FILE is set");
        }
        if std::env::var_os("SHARETREE_FORCE_FALLBACK").is_some() {
            return plain("SHARETREE_FORCE_FALLBACK is set");
        }
        if git::common_dir(&cwd).is_err() {
            return plain("not inside a git repository");
        }
        if git::config_bool(&cwd, "core.splitIndex")? == Some(true) {
            return plain("core.splitIndex is enabled");
        }
        for hook in ["reference-transaction", "post-index-change"] {
            if git::hook_exists(&cwd, hook)? {
                return plain(&format!("a {hook} hook is installed"));
            }
        }

        // Probe beside the destination, never inside a directory that may
        // already hold the user's files.
        let anchor = util::nearest_existing(dest.parent().unwrap_or(&dest));
        let t = Instant::now();
        let probe = reflink::probe(&anchor);
        let trace = Trace::new(a.verbose);
        trace.phase("probe", t);

        let sel = super::select(a, &cwd, &dest, probe.is_ok())?;
        trace.note(format!("backend: {:?}", sel.decision));
        match sel.decision {
            Decision::Fail(m) => bail!(m),
            Decision::Ask(_) => unreachable!("resolved in select()"),
            Decision::Plain(h) => {
                let why = probe
                    .err()
                    .unwrap_or_else(|| "a volume was not used".into());
                let code = delegate(
                    a,
                    &format!(
                        "{} ({}) cannot reflink: {why}",
                        anchor.display(),
                        fs_desc(&anchor)
                    ),
                )?;
                if !a.quiet {
                    if let Some(t) = backend::hint_text(&h, &fs_desc(&anchor)) {
                        eprintln!("sharetree: {t}");
                    }
                }
                return Ok((code, dest));
            }
            Decision::IntoVolume(_) => {
                let v = sel.volume.expect("decided with a volume");
                let p = super::place(a, &cwd, &sel.common, &v)?;
                let mut a2 = p.args.clone();
                a2.extra_donor = Some(ensure_seed(&cwd, &p.repo_root, a, &trace)?);
                let (code, _) = run(&a2)?;
                if code == 0 {
                    super::finish_placement(&p, &sel.common, &v, a.quiet)?;
                }
                return Ok((code, p.real));
            }
            Decision::Reflink { seed: true } => {
                // Already inside the volume: lock it and use the seed.
                let v = sel.volume.expect("decided with a volume");
                let p = super::place(a, &cwd, &sel.common, &v)?;
                let mut a2 = p.args.clone();
                a2.extra_donor = Some(ensure_seed(&cwd, &p.repo_root, a, &trace)?);
                let (code, _) = run(&a2)?;
                if code == 0 {
                    super::finish_placement(&p, &sel.common, &v, true)?;
                }
                return Ok((code, p.real));
            }
            Decision::Reflink { seed: false } => {}
        }
        Ok((create(a, &cwd, &dest, &anchor, &trace)?, dest))
    }

    /// The volume's seed for this repository: a detached, locked worktree at
    /// `<repo_root>/.sharetree-seed`. Created on first use (one full checkout,
    /// or reflinked from worktrees already in the volume), then advanced to
    /// the requested commit when it is clean, so the new worktree shares
    /// nearly every file with it.
    fn ensure_seed(cwd: &Path, repo_root: &Path, a: &AddArgs, trace: &Trace) -> Result<PathBuf> {
        let seed = repo_root.join(volume::SEED_DIR);
        let rev = a
            .commitish
            .clone()
            .unwrap_or_else(|| OsString::from("HEAD"));
        let commit = git::rev_parse_commit(cwd, &rev)
            .or_else(|_| git::rev_parse_commit(cwd, OsStr::new("HEAD")))?;
        let wts = git::list_worktrees(cwd)?;
        if let Some(w) = wts
            .iter()
            .find(|w| util::canon(&w.path) == util::canon(&seed))
        {
            if seed.join(".git").is_file() {
                if w.head.as_deref() != Some(commit.as_str()) {
                    let clean =
                        git::output(&seed, &["status", "--porcelain", "--untracked-files=no"])
                            .is_ok_and(|o| o.is_empty());
                    let t = Instant::now();
                    if clean {
                        let ok = git::cmd(&seed)
                            .args([
                                "-c",
                                "core.hooksPath=/dev/null",
                                "checkout",
                                "-q",
                                "--detach",
                                &commit,
                            ])
                            .stdin(std::process::Stdio::null())
                            .status()
                            .is_ok_and(|s| s.success());
                        if !ok {
                            eprintln!(
                                "sharetree: warning: could not advance the seed {}",
                                seed.display()
                            );
                        }
                    } else {
                        eprintln!(
                            "sharetree: warning: the seed {} has local changes; leaving it at its commit",
                            seed.display()
                        );
                    }
                    trace.phase("advance seed", t);
                }
                return Ok(seed);
            }
            // Registered but gone: drop the stale registration.
            git::output(
                cwd,
                &[
                    OsStr::new("worktree"),
                    OsStr::new("remove"),
                    OsStr::new("--force"),
                    OsStr::new("--force"),
                    seed.as_os_str(),
                ],
            )?;
        }
        if !a.quiet {
            eprintln!(
                "sharetree: creating the volume's seed checkout {} (once per repository; later worktrees share its blocks)",
                seed.display()
            );
        }
        let base = AddArgs {
            quiet: true,
            internal: true,
            min_size: DEFAULT_MIN_SIZE,
            forward: vec!["-q".into(), "--detach".into()],
            commitish: Some(OsString::from(&commit)),
            ..Default::default()
        };
        let sa = base.relocated(&seed, Some(volume::SEED_LOCK_REASON));
        let t = Instant::now();
        let (code, _) = run(&sa)?;
        trace.phase("seed", t);
        if code != 0 {
            bail!("creating the seed checkout {} failed", seed.display());
        }
        Ok(seed)
    }

    /// The reflink engine proper: register, analyze donors, clone, verify,
    /// seed the index, check out the rest.
    fn create(a: &AddArgs, cwd: &Path, dest: &Path, anchor: &Path, trace: &Trace) -> Result<i32> {
        let cwd = cwd.to_path_buf();
        let dest = dest.to_path_buf();
        let dest_arg = a.path.as_ref().expect("checked by caller");
        let dev =
            util::device_id(anchor).context("cannot identify the destination's filesystem")?;

        let wts = git::list_worktrees(&cwd)?;
        let cands = match &a.from {
            Some(from) => {
                let from = util::absolutize(from, &cwd);
                let top =
                    git::toplevel(&from).with_context(|| format!("--from {}", from.display()))?;
                let head = git::rev_parse_commit(&top, OsStr::new("HEAD"))?;
                if util::device_id(&top) != Some(dev) {
                    return delegate(
                        a,
                        "the --from worktree is on a different filesystem than the destination",
                    );
                }
                vec![Cand { path: top, head }]
            }
            None => {
                let mut c = candidates(&wts, &cwd, &dest, dev);
                // The volume's seed goes first: it is clean and at (or near)
                // the target commit.
                if let Some(seed) = &a.extra_donor {
                    if let Some(w) = wts
                        .iter()
                        .find(|w| util::canon(&w.path) == util::canon(seed))
                    {
                        if let Some(head) = &w.head {
                            if util::device_id(&w.path) == Some(dev) {
                                c.retain(|x| util::canon(&x.path) != util::canon(seed));
                                c.insert(
                                    0,
                                    Cand {
                                        path: w.path.clone(),
                                        head: head.clone(),
                                    },
                                );
                            }
                        }
                    }
                }
                c
            }
        };
        if cands.is_empty() {
            return delegate(a, "no donor worktree on the same filesystem");
        }
        trace.note(format!(
            "candidates: {}",
            cands
                .iter()
                .map(|c| c.path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));

        // Register, overlapped with the analysis (which reads only the object
        // store and donor worktrees, never the new worktree).
        let guess_rev = a
            .commitish
            .clone()
            .unwrap_or_else(|| OsString::from("HEAD"));
        let guess = git::rev_parse_commit(&cwd, &guess_rev).ok();
        let mut add_args: Vec<OsString> =
            vec!["worktree".into(), "add".into(), "--no-checkout".into()];
        add_args.extend(a.forward.iter().cloned());
        if a.separator {
            add_args.push("--".into());
        }
        add_args.push(dest_arg.clone());
        if let Some(c) = &a.commitish {
            add_args.push(c.clone());
        }
        let explicit = a.from.is_some();
        let t = Instant::now();
        let (status, early) = std::thread::scope(|s| {
            let h = s.spawn(|| {
                // Concurrent `git worktree add` calls race: one can read another's
                // half-written `worktrees/<id>/commondir` and die with "failed to
                // read ... commondir" (reproduced with stock git, ~1 in 800).
                // Serialize our own registrations; the lock is advisory, so it
                // only orders sharetree against sharetree.
                let _lock = register_lock(&cwd);
                git::run_inherit(&cwd, &add_args)
            });
            let early = guess
                .as_deref()
                .map(|sha| analyze(&cwd, sha, &cands, explicit, trace));
            (h.join().expect("git worktree add thread"), early)
        });
        let status = status?;
        if !status.success() {
            return Ok(status.code().unwrap_or(1));
        }
        trace.phase("register+scan", t);

        let target = git::rev_parse_commit(&dest, OsStr::new("HEAD"))?;
        let analysis = match early {
            Some(Ok(an)) if an.sha == target => Ok(an),
            _ => analyze(&cwd, &target, &cands, explicit, trace),
        };

        let report = match analysis {
            Ok(an) => seed(&dest, &an, a, trace).unwrap_or_else(|e| {
                eprintln!("sharetree: warning: seeding skipped: {e:#}");
                SeedReport::default()
            }),
            Err(e) => {
                eprintln!("sharetree: warning: donor analysis failed: {e:#}");
                SeedReport::default()
            }
        };

        // Finalize: git writes everything we did not vouch for.
        let t = Instant::now();
        if let Err(first) = checkout(&dest) {
            if !report.index_written {
                return Err(first);
            }
            eprintln!("sharetree: warning: git rejected the seeded index ({first:#}); retrying a plain checkout");
            let idx = git::git_path(&dest, "index")?;
            for p in [
                idx.clone(),
                PathBuf::from(format!("{}.lock", idx.display())),
            ] {
                match fs::remove_file(&p) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e).context("removing the seeded index"),
                }
            }
            checkout(&dest)?;
        }
        trace.phase("checkout", t);

        // Belt and braces (git-twig RFD0024): the index must equal the commit.
        if !git::succeeds(&dest, &["diff-index", "--cached", "--quiet", "HEAD", "--"]) {
            eprintln!("sharetree: warning: index differs from HEAD after checkout; resetting");
            git::output(
                &dest,
                &[
                    "-c",
                    "core.hooksPath=/dev/null",
                    "reset",
                    "-q",
                    "--hard",
                    "HEAD",
                ],
            )?;
        }

        let zero = "0".repeat(target.len());
        let hook_code = if a.internal {
            0
        } else {
            let h = git::run_inherit(
                &dest,
                &[
                    "hook",
                    "run",
                    "--ignore-missing",
                    "post-checkout",
                    "--",
                    &zero,
                    &target,
                    "1",
                ],
            )?;
            if h.success() {
                0
            } else {
                h.code().unwrap_or(1)
            }
        };

        if !a.quiet {
            summary(&dest, &report);
        }
        trace.total();

        if a.warm {
            match &report.donor {
                Some(donor) if report.verified > 0 || report.planned == 0 => {
                    #[cfg(unix)]
                    crate::warm::run(donor, &dest, &wts, a.quiet, trace)?;
                    #[cfg(not(unix))]
                    {
                        let _ = (donor, &wts);
                        eprintln!("sharetree: --warm is not available on this platform");
                    }
                }
                _ => eprintln!("sharetree: --warm skipped: no donor worktree we could reflink from (caches are never byte-copied)"),
            }
        }
        Ok(hook_code)
    }

    /// Exclusive advisory lock on `<common-dir>/sharetree-register.lock`, held for
    /// the duration of one `git worktree add`. Best effort: any failure to lock
    /// just means we run unserialized, as stock git does.
    fn register_lock(cwd: &Path) -> Option<fs::File> {
        let dir = git::common_dir(cwd).ok()?;
        let f = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join("sharetree-register.lock"))
            .ok()?;
        f.lock().ok()?;
        Some(f)
    }

    fn summary(dest: &Path, r: &SeedReport) {
        let total = git::ls_files_stage(dest).map(|v| v.len()).unwrap_or(0);
        match &r.donor {
            None => eprintln!(
                "sharetree: no usable donor; git wrote all {total} files (same as `git worktree add`)"
            ),
            Some(d) => {
                let mut msg = format!(
                    "sharetree: reflinked {} of {} files ({}) from {}; git wrote the other {}",
                    r.verified,
                    total,
                    human(r.bytes),
                    d.display(),
                    total.saturating_sub(r.verified)
                );
                if r.cloned > r.verified {
                    msg.push_str(&format!(
                        " ({} clones failed verification)",
                        r.cloned - r.verified
                    ));
                }
                if r.unsupported > 0 {
                    msg.push_str(&format!(
                        " ({} files could not be reflinked)",
                        r.unsupported
                    ));
                }
                eprintln!("{msg}");
            }
        }
    }

    /// cwd's worktree, the main worktree, then the most recently modified
    /// others (git-cow-worktree `candidatePool`), minus bare, prunable,
    /// missing and other-device ones (git-twig RFD0016).
    fn candidates(wts: &[git::Worktree], cwd: &Path, dest: &Path, dev: u64) -> Vec<Cand> {
        let mut first = Vec::new();
        let mut others = Vec::new();
        for w in wts {
            let Some(head) = &w.head else { continue };
            if w.bare || w.prunable || util::canon(&w.path) == util::canon(dest) {
                continue;
            }
            let Ok(md) = fs::metadata(&w.path) else {
                continue;
            };
            if util::device_id(&w.path) != Some(dev) {
                continue;
            }
            let c = Cand {
                path: w.path.clone(),
                head: head.clone(),
            };
            if util::is_within(cwd, &w.path) {
                first.insert(0, c);
            } else if w.is_main {
                first.push(c);
            } else {
                let mtime = md.modified().ok();
                others.push((mtime, c));
            }
        }
        // A worktree nested inside another (e.g. .worktrees/x inside main)
        // is "within" both; keep the deepest one first.
        first.sort_by_key(|c| {
            std::cmp::Reverse(
                c.path.components().count() * usize::from(util::is_within(cwd, &c.path)),
            )
        });
        others.sort_by_key(|o| std::cmp::Reverse(o.0));
        first
            .into_iter()
            .chain(others.into_iter().map(|(_, c)| c))
            .take(5)
            .collect()
    }

    fn analyze(
        repo: &Path,
        sha: &str,
        cands: &[Cand],
        explicit: bool,
        trace: &Trace,
    ) -> Result<Analysis> {
        let t = Instant::now();
        let mut heads: Vec<&str> = vec![sha];
        for c in cands {
            if !heads.contains(&c.head.as_str()) {
                heads.push(&c.head);
            }
        }
        let mut trees: HashMap<String, Tree> = std::thread::scope(|s| {
            let hs: Vec<_> = heads
                .iter()
                .map(|h| s.spawn(move || (h.to_string(), git::ls_tree(repo, h))))
                .collect();
            hs.into_iter()
                .filter_map(|h| h.join().ok())
                .filter_map(|(h, t)| t.ok().map(|t| (h, t)))
                .collect()
        });
        let target = match trees.remove(sha) {
            Some(t) => t,
            None => git::ls_tree(repo, sha)
                .with_context(|| format!("cannot list the target tree {sha}"))?,
        };
        let score_of = |tree: &Tree| {
            target
                .entries
                .iter()
                .filter(|(p, e)| {
                    e.is_regular() && tree.entries.get(*p).is_some_and(|d| d.oid == e.oid)
                })
                .count()
        };
        let mut ranked: Vec<(usize, usize, &Cand)> = cands
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let score = if c.head == sha {
                    target.regular_count()
                } else {
                    trees.get(&c.head).map_or(0, score_of)
                };
                (score, i, c)
            })
            .collect();
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        trace.phase("score donors", t);

        let mut donor = None;
        for (score, _, c) in ranked.into_iter().take(3) {
            if score == 0 {
                break;
            }
            let t = Instant::now();
            let tree = if c.head == sha {
                target.clone()
            } else {
                match trees.remove(&c.head) {
                    Some(t) => t,
                    None => git::ls_tree(repo, &c.head)?,
                }
            };
            // Tracked files that differ from the donor's HEAD (by its stat
            // cache: a filter only, every clone is hashed later).
            let dirty: HashSet<Vec<u8>> =
                match git::output(&c.path, &["diff-index", "-z", "--name-only", "HEAD", "--"]) {
                    Ok(out) => git::split_z(&out).into_iter().collect(),
                    Err(e) => {
                        trace.note(format!("skipping donor {}: {e:#}", c.path.display()));
                        continue;
                    }
                };
            trace.phase("donor status", t);
            // A worktree mid-checkout reads as mostly dirty: useless as a donor.
            if !explicit && dirty.len() * 2 > tree.regular_count() {
                trace.note(format!(
                    "skipping donor {}: {} of {} tracked files differ from its HEAD",
                    c.path.display(),
                    dirty.len(),
                    tree.regular_count()
                ));
                continue;
            }
            donor = Some(Donor {
                path: c.path.clone(),
                tree,
                dirty,
                score,
            });
            break;
        }
        Ok(Analysis {
            sha: sha.to_string(),
            target,
            donor,
        })
    }

    struct Planned<'a> {
        dst: &'a [u8],
        src: &'a [u8],
        entry: &'a Entry,
    }

    fn seed(dest: &Path, an: &Analysis, a: &AddArgs, trace: &Trace) -> Result<SeedReport> {
        let mut report = SeedReport::default();
        let Some(d) = &an.donor else {
            trace.note("no donor with matching files; plain checkout");
            return Ok(report);
        };
        report.donor = Some(d.path.clone());
        let t = Instant::now();

        // Clean donor files by blob id, to also reuse renamed/moved files.
        let mut by_oid: HashMap<&str, &[u8]> = HashMap::new();
        for (p, e) in &d.tree.entries {
            if e.is_regular() && !d.dirty.contains(p) {
                by_oid.entry(e.oid.as_str()).or_insert(p.as_slice());
            }
        }
        let mut plan: Vec<Planned> = Vec::new();
        for (p, e) in &an.target.entries {
            if !e.is_regular() || e.size < a.min_size {
                continue;
            }
            let src = match d.tree.entries.get(p) {
                Some(de) if de.oid == e.oid && de.is_regular() && !d.dirty.contains(p) => {
                    p.as_slice()
                }
                _ => match by_oid.get(e.oid.as_str()) {
                    Some(sp) => sp,
                    None => continue,
                },
            };
            plan.push(Planned {
                dst: p,
                src,
                entry: e,
            });
        }
        plan.sort_by(|x, y| x.dst.cmp(y.dst));

        if git::config_bool(dest, "core.sparseCheckout")? == Some(true) && !plan.is_empty() {
            let mut input = Vec::new();
            for p in &plan {
                input.extend_from_slice(p.dst);
                input.push(0);
            }
            let out =
                git::output_with_input(dest, &["sparse-checkout", "check-rules", "-z"], &input)?;
            let keep: HashSet<Vec<u8>> = git::split_z(&out).into_iter().collect();
            plan.retain(|p| keep.contains(p.dst));
        }
        if !plan.is_empty() {
            let paths: Vec<&[u8]> = plan.iter().map(|p| p.dst).collect();
            let raw = attrs::raw_paths(dest, &an.sha, &paths)?;
            plan.retain(|p| raw.contains(p.dst));
        }
        report.planned = plan.len();
        trace.phase("plan", t);
        trace.note(format!(
            "donor {} matches {} files; planned {} clones (>= {} bytes, no conversions)",
            d.path.display(),
            d.score,
            plan.len(),
            a.min_size
        ));
        if plan.is_empty() {
            return Ok(report);
        }

        // Parent directories, created the way git creates them (0777 & ~umask).
        let t = Instant::now();
        let mut dirs: Vec<&[u8]> = plan
            .iter()
            .filter_map(|p| p.dst.iter().rposition(|&b| b == b'/').map(|i| &p.dst[..i]))
            .collect();
        dirs.dedup();
        for dir in dirs {
            fs::create_dir_all(dest.join(bytes_to_path(dir)))
                .with_context(|| format!("mkdir {}", String::from_utf8_lossy(dir)))?;
        }
        trace.phase("mkdir", t);

        // Clone.
        let t = Instant::now();
        let next = AtomicUsize::new(0);
        let stop = AtomicBool::new(false);
        let ok_count = AtomicUsize::new(0);
        let unsupported = AtomicUsize::new(0);
        let cloned: Mutex<Vec<usize>> = Mutex::new(Vec::with_capacity(plan.len()));
        std::thread::scope(|s| {
            for _ in 0..util::workers(plan.len()) {
                s.spawn(|| {
                    let mut mine = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= plan.len() || stop.load(Ordering::Relaxed) {
                            break;
                        }
                        let p = &plan[i];
                        let create_mode = if p.entry.mode == MODE_EXEC {
                            0o777
                        } else {
                            0o666
                        };
                        match reflink::clone_tracked(
                            &d.path.join(bytes_to_path(p.src)),
                            &dest.join(bytes_to_path(p.dst)),
                            create_mode,
                            p.entry.size,
                        ) {
                            Ok(()) => {
                                ok_count.fetch_add(1, Ordering::Relaxed);
                                mine.push(i);
                            }
                            Err(CloneError::Unsupported(_)) => {
                                let n = unsupported.fetch_add(1, Ordering::Relaxed) + 1;
                                // The probe passed, so this is per-file (NOCOW
                                // flag, ZFS dirty data...). Give up only if
                                // nothing at all can be shared.
                                if n >= 64 && ok_count.load(Ordering::Relaxed) == 0 {
                                    stop.store(true, Ordering::Relaxed);
                                }
                            }
                            Err(_) => {}
                        }
                    }
                    cloned.lock().expect("lock").extend(mine);
                });
            }
        });
        let mut cloned = cloned.into_inner().expect("lock");
        cloned.sort_unstable();
        report.cloned = cloned.len();
        report.unsupported = unsupported.load(Ordering::Relaxed);
        trace.phase("clone", t);

        // Verify: hash every clone's own bytes in-process against the target
        // blob. Measured alternatives, all slower or no faster on Btrfs and
        // XFS (README, "Benchmarks"): hashing each clone as soon as it is
        // made (cloning slowed down more than the overlap saved), and letting
        // git write the other files meanwhile (it slowed the reads as much).
        let t = Instant::now();
        let fmt = git::object_format(dest)?;
        let next = AtomicUsize::new(0);
        let verified: Mutex<Vec<(usize, Stat)>> = Mutex::new(Vec::with_capacity(cloned.len()));
        std::thread::scope(|s| {
            for _ in 0..hash_workers(cloned.len()) {
                s.spawn(|| {
                    let mut mine = Vec::new();
                    loop {
                        let k = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&i) = cloned.get(k) else { break };
                        let p = &plan[i];
                        let full = dest.join(bytes_to_path(p.dst));
                        match index::hex_to_raw(&p.entry.oid)
                            .and_then(|w| verify_clone(&full, p.entry, &w, fmt))
                        {
                            Some(st) => mine.push((i, st)),
                            None => {
                                let _ = fs::remove_file(&full);
                            }
                        }
                    }
                    verified.lock().expect("lock").extend(mine);
                });
            }
        });
        let mut verified = verified.into_inner().expect("lock");
        verified.sort_unstable_by_key(|v| v.0);
        trace.phase("verify", t);

        // A verified clone enters the index only if its stat data has not
        // moved since it was hashed.
        let mut entries = Vec::with_capacity(verified.len());
        for (i, st) in &verified {
            let p = &plan[*i];
            let full = dest.join(bytes_to_path(p.dst));
            let still = fs::symlink_metadata(&full)
                .ok()
                .map(|m| Stat::from_metadata(&m));
            if still == Some(*st) {
                report.bytes += p.entry.size;
                entries.push(IndexEntry {
                    path: p.dst.to_vec(),
                    mode: p.entry.mode,
                    oid: index::hex_to_raw(&p.entry.oid).context("bad object id")?,
                    stat: *st,
                });
            } else {
                let _ = fs::remove_file(&full);
            }
        }
        report.verified = entries.len();

        if entries.is_empty() {
            report.bytes = 0;
            return Ok(report);
        }

        let t = Instant::now();
        let idx = git::git_path(dest, "index")?;
        if idx.exists() {
            bail!("{} already exists", idx.display());
        }
        index::write(&idx, &mut entries, fmt)?;
        report.index_written = true;
        trace.phase("write index", t);
        Ok(report)
    }

    /// Hashing a fresh clone is a cold read (its page cache is empty even
    /// when the donor's is warm), so more readers than cloners pay off.
    fn hash_workers(n_items: usize) -> usize {
        let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
        cpus.clamp(4, 16).min(n_items.max(1))
    }

    /// Hash the clone at `path` in-process and compare with the target blob.
    /// Returns its stat data when type, size, exec bit and content all match
    /// and the stat data did not move while it was read.
    fn verify_clone(
        path: &Path,
        entry: &Entry,
        want: &[u8],
        fmt: git::ObjectFormat,
    ) -> Option<Stat> {
        let mut f = reflink::open_no_follow(path).ok()?;
        let m1 = f.metadata().ok()?;
        if !reflink::is_plain_file(&m1)
            || m1.len() != entry.size
            || !util::exec_bit_matches(&m1, entry.mode == MODE_EXEC)
        {
            return None;
        }
        let oid = crate::hash::blob_oid(&mut f, entry.size, fmt).ok()??;
        if oid != want {
            return None;
        }
        let s1 = Stat::from_metadata(&m1);
        let s2 = Stat::from_metadata(&f.metadata().ok()?);
        (s1 == s2).then_some(s1)
    }

    fn checkout(dest: &Path) -> Result<()> {
        let out = git::cmd(dest)
            .args([
                "-c",
                "checkout.workers=0",
                "-c",
                "core.hooksPath=/dev/null",
                "checkout",
                "-q",
                "-f",
                "HEAD",
            ])
            .stdin(std::process::Stdio::null())
            .output()
            .context("failed to run git checkout")?;
        if !out.status.success() {
            bail!(
                "git checkout -f HEAD: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn splits_sharetree_and_git_options() {
        let a = parse(&os(&[
            "-b", "feat", "--from", "../main", "--warm", "-v", "--lock", "wt", "HEAD~1",
        ]))
        .unwrap();
        assert_eq!(a.forward, os(&["-b", "feat", "--lock"]));
        assert_eq!(a.git_args, os(&["-b", "feat", "--lock", "wt", "HEAD~1"]));
        assert_eq!(a.path, Some("wt".into()));
        assert_eq!(a.commitish, Some("HEAD~1".into()));
        assert!(a.warm && a.verbose);
        assert_eq!(a.from, Some(PathBuf::from("../main")));
    }

    #[test]
    fn checkout_flag_is_never_forwarded() {
        let a = parse(&os(&["--no-checkout", "--checkout", "wt"])).unwrap();
        assert!(!a.no_checkout);
        assert!(a.forward.is_empty());
    }

    #[test]
    fn separator_and_min_size() {
        let a = parse(&os(&["--min-size=10", "--", "-weird", "main"])).unwrap();
        assert_eq!(a.min_size, 10);
        assert!(a.separator);
        assert_eq!(a.path, Some("-weird".into()));
        assert!(parse(&os(&["a", "b", "c"])).is_err());
    }
}
