use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};

use crate::backend::{self, Decision, Hint};
use crate::config::{Config, Volume};
use crate::util::Trace;
use crate::{git, util, volume};

pub const DEFAULT_MIN_SIZE: u64 = 2049;

#[derive(Debug, Default, Clone)]
pub struct AddArgs {
    pub verbose: bool,
    pub quiet: bool,
    pub from: Option<PathBuf>,
    pub warm: bool,
    pub no_cow: bool,
    pub min_size: u64,
    pub forward: Vec<OsString>,
    pub git_args: Vec<OsString>,
    pub separator: bool,
    pub path: Option<OsString>,
    pub commitish: Option<OsString>,
    pub no_checkout: bool,
    pub orphan: bool,
    pub into_volume: bool,
    pub no_volume: bool,
    pub overlay: bool,
    pub windows_clone: bool,
    pub internal: bool,
}

impl AddArgs {
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
            Some(s) if s.starts_with("--no-c") && "--no-checkout".starts_with(s) => {
                a.no_checkout = true;
                a.git_args.push(arg.clone());
            }
            Some(s) if s.starts_with("--c") && "--checkout".starts_with(s) => {
                a.no_checkout = false;
                a.git_args.push(arg.clone());
            }
            Some(s) if s.starts_with("--o") && "--orphan".starts_with(s) => {
                a.orphan = true;
                a.git_args.push(arg.clone());
            }
            Some(s) if s.starts_with("--no-o") && "--no-orphan".starts_with(s) => {
                a.orphan = false;
                a.git_args.push(arg.clone());
            }
            Some("-q" | "--quiet") => {
                a.quiet = true;
                a.forward.push(arg.clone());
                a.git_args.push(arg.clone());
            }
            Some(s)
                if matches!(s, "-b" | "-B")
                    || (s.starts_with("--reas") && "--reason".starts_with(s)) =>
            {
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
    if code == 0 && !a.quiet {
        crate::advise::print_add_hint(&made);
    }
    Ok(code)
}

const NO_CLONE: &str =
    "copy-on-write worktrees are not enabled on this platform (ReFS block cloning is off by default, see README)";

fn dispatch(a: &AddArgs) -> Result<(i32, PathBuf)> {
    let cwd = std::env::current_dir()?;
    let dest = cwd.join(a.path.as_ref().expect("checked by caller"));
    let plain =
        |why: &str| -> Result<(i32, PathBuf)> { Ok((delegate(a, &cwd, why)?, dest.clone())) };

    let cfg = cfg!(windows).then(Config::load);
    let clone_enabled = cfg!(unix)
        || (cfg!(windows)
            && (a.windows_clone || matches!(&cfg, Some(Ok(c)) if c.windows_clone == Some(true))));
    if a.windows_clone && cfg!(unix) {
        eprintln!("sharetree: --windows-clone has no effect outside Windows");
    }
    #[cfg(windows)]
    if clone_enabled && !a.quiet {
        eprintln!("sharetree: note: {}", crate::winclone::NOTE);
    }

    let opt_out = if a.no_cow {
        Some("--no-cow given")
    } else if a.no_checkout {
        Some("--no-checkout: there is nothing to clone")
    } else if a.orphan {
        Some("--orphan: there is nothing to clone")
    } else {
        None
    };
    if let Some(why) = opt_out.filter(|_| clone_enabled) {
        return plain(why);
    }
    if let Some(var) = git::context_override() {
        return plain(&format!("{var} is set"));
    }
    if opt_out.is_some() {
        return plain(NO_CLONE);
    }
    if clone_enabled && std::env::var_os("SHARETREE_FORCE_FALLBACK").is_some() {
        return plain("SHARETREE_FORCE_FALLBACK is set");
    }
    let Ok(common) = git::common_dir(&cwd) else {
        return plain(if clone_enabled {
            "not inside a git repository"
        } else {
            NO_CLONE
        });
    };
    if clone_enabled {
        if git::config_bool(&cwd, "core.splitIndex")? == Some(true) {
            return plain("core.splitIndex is enabled");
        }
        for hook in ["reference-transaction", "post-index-change"] {
            if git::hook_exists(&cwd, hook)? {
                return plain(&format!("a {hook} hook is installed"));
            }
        }
    }

    let trace = Trace::new(a.verbose);
    let probe = CloneProbe::at(&dest, clone_enabled, &trace);
    let cfg = cfg.unwrap_or_else(Config::load).unwrap_or_else(|e| {
        eprintln!("sharetree: warning: ignoring the config: {e:#}");
        Config::default()
    });
    let volume = cfg.volume_for(&common);
    let dest_in_volume =
        volume.is_some_and(|v| in_mount(&dest, volume::current_state(v).mountpoint()));
    let decision = backend::decide(
        &backend::Inputs {
            probe_ok: probe.succeeded(),
            dest_in_volume,
            volume,
            auto_volume: cfg.auto_volume_for(&common),
            into_volume: a.into_volume,
            no_volume: a.no_volume,
        },
        backend::Platform::current(),
    );
    trace.note(format!("backend: {decision:?}"));
    let fallback = |hint| Ok((probe.fallback(a, &cwd, &hint)?, dest.clone()));
    let v = match decision {
        Decision::Fail(msg) => bail!(msg),
        Decision::Reflink => {
            let code = probe.create(a, &cwd, &dest, None, &trace)?;
            return Ok((code, dest));
        }
        Decision::Plain(hint) => return fallback(hint),
        Decision::Volume(v) => v,
        Decision::Ask(v) => {
            if !can_prompt() {
                return fallback(Hint::VolumeUndecided(v.name.clone()));
            }
            if !ask_volume(&v.name, &dest, &common)?.use_volume() {
                return fallback(Hint::None);
            }
            v
        }
    };

    let placement = Placement::prepare(a, &cwd, &common, &dest, v)?;
    let code = if clone_enabled {
        #[cfg(any(unix, windows))]
        {
            let donor = engine::ensure_seed(&cwd, &placement.repo_root, a, &trace)?;
            let relocated_probe =
                (placement.real != dest).then(|| CloneProbe::at(&placement.real, true, &trace));
            relocated_probe.as_ref().unwrap_or(&probe).create(
                &placement.args,
                &cwd,
                &placement.real,
                Some(&donor),
                &trace,
            )?
        }
        #[cfg(not(any(unix, windows)))]
        unreachable!("the clone engine is unavailable")
    } else {
        delegate(
            &placement.args,
            &cwd,
            "placing the worktree in the Dev Drive (sharetree does not block-clone on Windows yet)",
        )?
    };
    if code == 0 {
        let quiet = a.quiet || (dest_in_volume && probe.succeeded());
        placement.finish(v, quiet)?;
    }
    Ok((code, placement.real))
}

fn in_mount(dest: &Path, mountpoint: Option<&Path>) -> bool {
    mountpoint.is_some_and(|mp| util::is_within(&util::nearest_existing(dest), mp))
}

enum CloneProbe {
    Succeeded(PathBuf),
    Failed { anchor: PathBuf, reason: String },
    Disabled,
}

impl CloneProbe {
    fn at(dest: &Path, enabled: bool, trace: &Trace) -> Self {
        if !enabled {
            return Self::Disabled;
        }
        let anchor = util::nearest_existing(dest.parent().unwrap_or(dest));
        let t = Instant::now();
        #[cfg(any(unix, windows))]
        let result = crate::reflink::probe(&anchor);
        #[cfg(not(any(unix, windows)))]
        let result = Err(NO_CLONE.to_owned());
        trace.phase("probe", t);
        match result {
            Ok(()) => Self::Succeeded(anchor),
            Err(reason) => Self::Failed { anchor, reason },
        }
    }

    fn succeeded(&self) -> bool {
        matches!(self, Self::Succeeded(_))
    }

    fn fallback(&self, a: &AddArgs, cwd: &Path, hint: &Hint) -> Result<i32> {
        let (reason, fs) = match self {
            Self::Failed { anchor, reason } => {
                let fs = fs_label(anchor);
                (
                    format!("{} ({fs}) cannot reflink: {reason}", anchor.display()),
                    fs,
                )
            }
            _ => (NO_CLONE.to_owned(), "NTFS"),
        };
        let code = delegate(a, cwd, &reason)?;
        if !a.quiet {
            if let Some(t) = backend::hint_text(hint, fs) {
                eprintln!("sharetree: {t}");
            }
        }
        Ok(code)
    }

    fn create(
        &self,
        a: &AddArgs,
        cwd: &Path,
        dest: &Path,
        donor: Option<&Path>,
        trace: &Trace,
    ) -> Result<i32> {
        #[cfg(any(unix, windows))]
        if let Self::Succeeded(anchor) = self {
            return engine::create(a, cwd, dest, anchor, donor, trace);
        }
        self.fallback(a, cwd, &Hint::None)
    }
}

fn can_prompt() -> bool {
    use std::io::IsTerminal;
    std::env::var_os("SHARETREE_NO_PROMPT").is_none_or(|v| v.is_empty() || v == "0")
        && std::io::stdin().is_terminal()
        && std::io::stderr().is_terminal()
}

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
            break backend::Answer::NotNow;
        }
        match backend::parse_answer(&line) {
            Some(a) => break a,
            None => eprintln!("sharetree: please answer y, a, n or v"),
        }
    };
    if let Some(on) = answer.persist() {
        Config::update(|cfg| {
            cfg.repo_mut(common).auto_volume = Some(on);
            Ok(())
        })?;
        let p = crate::config::config_path().unwrap_or_default();
        eprintln!(
            "sharetree: saved auto_volume = {on} for this repository in {} \
             (change it with `sharetree setup auto on|off --repo`)",
            p.display()
        );
    }
    Ok(answer)
}

fn fs_label(p: &Path) -> &'static str {
    #[cfg(target_os = "linux")]
    {
        crate::linux::fs_name(p)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = p;
        "this filesystem"
    }
}

struct Placement {
    requested: PathBuf,
    real: PathBuf,
    repo_root: PathBuf,
    args: AddArgs,
    _volume_lock: std::fs::File,
}

impl Placement {
    fn prepare(
        a: &AddArgs,
        cwd: &Path,
        common: &Path,
        requested: &Path,
        v: &Volume,
    ) -> Result<Self> {
        let volume_lock = crate::config::lock_volume_use()?;
        let cfg = Config::load()?;
        let current = cfg
            .volume_for(common)
            .filter(|current| current.name == v.name && current.image == v.image)
            .with_context(|| format!("volume '{}' changed while preparing the worktree", v.name))?;
        let mp = volume::mount_any(current, a.quiet)?;
        let main = git::list_worktrees(cwd)?
            .into_iter()
            .find(|w| w.is_main)
            .map(|w| w.path)
            .unwrap_or_else(|| common.to_path_buf());
        let repo_root = volume::worktrees_root(current, &mp).join(volume::repo_id(common, &main));
        std::fs::create_dir_all(&repo_root)
            .with_context(|| format!("creating {}", repo_root.display()))?;
        let real = if in_mount(requested, Some(&mp)) {
            requested.to_path_buf()
        } else {
            if let Ok(md) = requested.symlink_metadata() {
                let empty_dir = md.is_dir()
                    && std::fs::read_dir(requested).is_ok_and(|mut r| r.next().is_none());
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
        Ok(Self {
            requested: requested.to_path_buf(),
            real,
            repo_root,
            args,
            _volume_lock: volume_lock,
        })
    }

    fn finish(&self, v: &Volume, quiet: bool) -> Result<()> {
        let linked = cfg!(unix) && self.real != self.requested;
        if self.real != self.requested && self.requested.is_dir() {
            let _ = std::fs::remove_dir(&self.requested);
        }
        #[cfg(unix)]
        if linked {
            if let Some(parent) = self.requested.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::os::unix::fs::symlink(&self.real, &self.requested)
                .with_context(|| format!("creating the symlink {}", self.requested.display()))?;
        }
        if !quiet {
            eprintln!(
                "sharetree: the worktree lives in volume '{}' at {}{}",
                v.name,
                self.real.display(),
                if linked {
                    format!(" (symlink: {})", self.requested.display())
                } else {
                    String::new()
                }
            );
        }
        Ok(())
    }
}

fn delegate(a: &AddArgs, cwd: &Path, why: &str) -> Result<i32> {
    if !a.quiet {
        eprintln!("sharetree: {why}; running plain `git worktree add`");
    }
    let mut args: Vec<OsString> = Vec::new();
    if a.internal {
        args.extend(["-c".into(), "core.hooksPath=/dev/null".into()]);
    }
    args.extend(["worktree".into(), "add".into()]);
    args.extend(a.git_args.iter().cloned());
    let st = git::run_inherit(cwd, &args)?;
    Ok(st.code().unwrap_or(1))
}

#[cfg(any(unix, windows))]
mod engine {
    use std::collections::{HashMap, HashSet};
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::ExitStatus;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use anyhow::{bail, Context, Result};

    use super::{delegate, AddArgs, CloneProbe, DEFAULT_MIN_SIZE};
    use crate::attrs;
    use crate::git::{self, Entry, Tree, MODE_EXEC};
    use crate::index;
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
        tree: Arc<Tree>,
        dirty: HashSet<Vec<u8>>,
        score: usize,
    }

    struct Analysis {
        sha: String,
        target: Arc<Tree>,
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

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Lifecycle {
        Planned,
        Registered,
        Populated,
        Completed,
        RolledBack,
    }

    impl Lifecycle {
        fn advance(self, next: Self) -> Result<Self> {
            let allowed = matches!(
                (self, next),
                (Self::Planned, Self::Registered)
                    | (Self::Registered, Self::Populated)
                    | (Self::Populated, Self::Completed)
                    | (Self::Registered, Self::RolledBack)
                    | (Self::Populated, Self::RolledBack)
            );
            if allowed {
                Ok(next)
            } else {
                bail!("invalid worktree lifecycle transition: {self:?} -> {next:?}")
            }
        }
    }

    pub(super) fn ensure_seed(
        cwd: &Path,
        repo_root: &Path,
        a: &AddArgs,
        trace: &Trace,
    ) -> Result<PathBuf> {
        let seed = repo_root.join(volume::SEED_DIR);
        let lock = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(seed.with_extension("lock"))
            .with_context(|| format!("opening the volume seed lock in {}", repo_root.display()))?;
        lock.lock()
            .with_context(|| format!("locking the volume seed in {}", repo_root.display()))?;
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
        let probe = CloneProbe::at(&seed, true, trace);
        let code = probe.create(&sa, cwd, &seed, None, trace)?;
        trace.phase("seed", t);
        if code != 0 {
            bail!("creating the seed checkout {} failed", seed.display());
        }
        Ok(seed)
    }

    pub(super) fn create(
        a: &AddArgs,
        cwd: &Path,
        dest: &Path,
        anchor: &Path,
        preferred_donor: Option<&Path>,
        trace: &Trace,
    ) -> Result<i32> {
        let dest_arg = a.path.as_ref().expect("checked by caller");
        let dev =
            util::device_id(anchor).context("cannot identify the destination's filesystem")?;

        let wts = git::list_worktrees(cwd)?;
        let cands = match &a.from {
            Some(from) => {
                let from = cwd.join(from);
                let top =
                    git::toplevel(&from).with_context(|| format!("--from {}", from.display()))?;
                let head = git::rev_parse_commit(&top, OsStr::new("HEAD"))?;
                if util::device_id(&top) != Some(dev) {
                    return delegate(
                        a,
                        cwd,
                        "the --from worktree is on a different filesystem than the destination",
                    );
                }
                vec![Cand { path: top, head }]
            }
            None => {
                let mut c = candidates(&wts, cwd, dest, dev);
                if let Some(seed) = preferred_donor {
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
            return delegate(a, cwd, "no donor worktree on the same filesystem");
        }
        trace.note(format!(
            "candidates: {}",
            cands
                .iter()
                .map(|c| c.path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));

        let guess_rev = a
            .commitish
            .clone()
            .unwrap_or_else(|| OsString::from("HEAD"));
        let guess = git::rev_parse_commit(cwd, &guess_rev).ok();
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
        let lock = match register_lock(cwd) {
            Ok(lock) => lock,
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::Unsupported) =>
            {
                return delegate(
                    a,
                    cwd,
                    &format!("filesystem locking is unavailable for worktree registration: {e:#}"),
                );
            }
            Err(e) => return Err(e.context("cannot lock worktree registration")),
        };
        let explicit = a.from.is_some();
        let t = Instant::now();
        let (status, early) = std::thread::scope(|s| {
            let h = s.spawn(move || {
                let _lock = lock;
                git::run_inherit(cwd, &add_args)
            });
            let early = guess
                .as_deref()
                .map(|sha| analyze(cwd, sha, &cands, explicit, trace));
            (h.join().expect("git worktree add thread"), early)
        });
        let status = status?;
        if !status.success() {
            return Ok(status.code().unwrap_or(1));
        }
        let mut lifecycle = Lifecycle::Planned.advance(Lifecycle::Registered)?;
        trace.phase("register+scan", t);

        let prepared = (|| -> Result<_> {
            let target = git::rev_parse_commit(dest, OsStr::new("HEAD"))?;
            let analysis = match early {
                Some(Ok(an)) if an.sha == target => Ok(an),
                _ => analyze(cwd, &target, &cands, explicit, trace),
            };
            let mut report = analysis
                .and_then(|an| seed(dest, &an, a, trace))
                .unwrap_or_else(|e| {
                    eprintln!("sharetree: warning: seeding skipped: {e:#}");
                    SeedReport::default()
                });
            let t = Instant::now();
            let status = checkout(dest, a, &mut report)?;
            trace.phase("checkout", t);
            Ok((status, target, report))
        })();
        let (target, report) = match prepared {
            Ok((status, target, report)) if status.success() => {
                lifecycle = lifecycle.advance(Lifecycle::Populated)?;
                (target, report)
            }
            Ok((status, _, _)) => {
                let _ = lifecycle.advance(Lifecycle::RolledBack);
                rollback_registration(cwd, dest);
                return Ok(status.code().unwrap_or(1));
            }
            Err(e) => {
                let _ = lifecycle.advance(Lifecycle::RolledBack);
                rollback_registration(cwd, dest);
                return Err(e);
            }
        };

        let zero = "0".repeat(target.len());
        let hook_code = if a.internal {
            0
        } else {
            let h = git::run_inherit(
                dest,
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
            summary(dest, &report);
        }
        trace.total();

        if a.warm {
            match &report.donor {
                Some(donor) if report.verified > 0 || report.planned == 0 => {
                    #[cfg(unix)]
                    crate::warm::run(donor, dest, &wts, a.quiet, trace)?;
                    #[cfg(not(unix))]
                    {
                        let _ = (donor, &wts);
                        eprintln!("sharetree: --warm is not available on this platform");
                    }
                }
                _ => eprintln!("sharetree: --warm skipped: no donor worktree we could reflink from (caches are never byte-copied)"),
            }
        }
        let _ = lifecycle.advance(Lifecycle::Completed)?;
        Ok(hook_code)
    }

    fn rollback_registration(cwd: &Path, dest: &Path) {
        let _lock = register_lock(cwd)
            .inspect_err(|e| {
                eprintln!(
                    "sharetree: warning: cannot lock worktree registration for cleanup: {e:#}"
                );
            })
            .ok();
        let removed = git::output(
            cwd,
            &[
                OsStr::new("worktree"),
                OsStr::new("remove"),
                OsStr::new("--force"),
                OsStr::new("--force"),
                OsStr::new("--"),
                dest.as_os_str(),
            ],
        );
        if let Err(e) = removed {
            eprintln!("sharetree: warning: could not remove the unfinished worktree: {e:#}");
        }
    }

    fn register_lock(cwd: &Path) -> Result<fs::File> {
        let path = git::common_dir(cwd)?.join("sharetree-register.lock");
        let f = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        f.lock()
            .with_context(|| format!("locking {}", path.display()))?;
        Ok(f)
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
        let mut trees: HashMap<String, Arc<Tree>> = std::thread::scope(|s| {
            let hs: Vec<_> = heads
                .iter()
                .map(|h| s.spawn(move || (h.to_string(), git::ls_tree(repo, h))))
                .collect();
            hs.into_iter()
                .filter_map(|h| h.join().ok())
                .filter_map(|(h, t)| t.ok().map(|t| (h, Arc::new(t))))
                .collect()
        });
        let target = match trees.remove(sha) {
            Some(t) => t,
            None => Arc::new(
                git::ls_tree(repo, sha)
                    .with_context(|| format!("cannot list the target tree {sha}"))?,
            ),
        };
        let mut wanted: HashMap<&str, usize> = HashMap::new();
        let mut regular_files = 0;
        for entry in target.entries.values().filter(|e| e.is_regular()) {
            *wanted.entry(&entry.oid).or_default() += 1;
            regular_files += 1;
        }
        let score_of = |tree: &Tree, dirty: Option<&HashSet<Vec<u8>>>| {
            let mut seen = HashSet::new();
            tree.entries
                .iter()
                .filter(|(p, e)| e.is_regular() && dirty.is_none_or(|paths| !paths.contains(*p)))
                .filter_map(|(_, e)| wanted.get_key_value(e.oid.as_str()))
                .filter(|(oid, _)| seen.insert(**oid))
                .map(|(_, &count)| count)
                .sum::<usize>()
        };
        let scores: HashMap<&str, usize> = trees
            .iter()
            .map(|(head, tree)| (head.as_str(), score_of(tree, None)))
            .collect();
        let mut ranked: Vec<(usize, usize, &Cand)> = cands
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let score = if c.head == sha {
                    regular_files
                } else {
                    scores.get(c.head.as_str()).copied().unwrap_or(0)
                };
                (score, i, c)
            })
            .collect();
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        trace.phase("score donors", t);

        let mut donor = None;
        let mut best_score = 0;
        for (score, _, c) in ranked.into_iter().take(3) {
            if score <= best_score {
                break;
            }
            let t = Instant::now();
            let tree = if c.head == sha {
                &target
            } else {
                &trees[&c.head]
            };
            let dirty: HashSet<Vec<u8>> =
                match git::output(&c.path, &["diff-index", "-z", "--name-only", "HEAD", "--"]) {
                    Ok(out) => git::split_z(&out).into_iter().collect(),
                    Err(e) => {
                        trace.note(format!("skipping donor {}: {e:#}", c.path.display()));
                        continue;
                    }
                };
            trace.phase("donor status", t);
            let dirty_regular = dirty
                .iter()
                .filter(|path| tree.entries.get(*path).is_some_and(Entry::is_regular))
                .count();
            let total = if c.head == sha {
                regular_files
            } else {
                tree.regular_count()
            };
            if !explicit && dirty_regular > total / 2 {
                trace.note(format!(
                    "skipping donor {}: {} of {} tracked files differ from its HEAD",
                    c.path.display(),
                    dirty_regular,
                    total
                ));
                continue;
            }
            let score = if dirty_regular == 0 {
                score
            } else {
                score_of(tree, Some(&dirty))
            };
            if score <= best_score {
                continue;
            }
            best_score = score;
            donor = Some(Donor {
                path: c.path.clone(),
                tree: Arc::clone(tree),
                dirty,
                score,
            });
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

    fn reusable_tree_path(path: &[u8]) -> bool {
        let name = std::str::from_utf8(path);
        if name.as_ref().is_ok_and(|name| {
            name.chars().any(|c| {
                matches!(c, '\u{200c}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{206a}'..='\u{206f}' | '\u{feff}')
            })
        }) {
            return false;
        }
        if path
            .split(|b| matches!(b, b'/' | b'\\' | b':'))
            .any(|part| {
                let mut end = part.len();
                while end > 0 && matches!(part[end - 1], b'.' | b' ') {
                    end -= 1;
                }
                let part = &part[..end];
                part.eq_ignore_ascii_case(b".git") || part.eq_ignore_ascii_case(b"git~1")
            })
        {
            return false;
        }
        if cfg!(windows) && name.is_err() {
            return false;
        }
        path.split(|&b| b == b'/').all(|part| {
            !part.is_empty()
                && !matches!(part, b"." | b"..")
                && !part.contains(&0)
                && (!cfg!(windows) || reusable_windows_component(part))
        })
    }

    fn reusable_windows_component(part: &[u8]) -> bool {
        if part
            .iter()
            .any(|b| b.is_ascii_control() || b"<>:\"\\|?*~".contains(b))
            || part.last().is_some_and(|b| matches!(b, b'.' | b' '))
        {
            return false;
        }
        let stem = part
            .split(|&b| b == b'.')
            .next()
            .unwrap_or_default()
            .trim_ascii_end();
        if [
            b"CON".as_slice(),
            b"PRN",
            b"AUX",
            b"NUL",
            b"CONIN$",
            b"CONOUT$",
        ]
        .iter()
        .any(|reserved| stem.eq_ignore_ascii_case(reserved))
        {
            return false;
        }
        let numbered_device = stem.get(..3).is_some_and(|prefix| {
            prefix.eq_ignore_ascii_case(b"COM") || prefix.eq_ignore_ascii_case(b"LPT")
        }) && matches!(
            stem.get(3..),
            Some([b'0'..=b'9'] | b"\xc2\xb9" | b"\xc2\xb2" | b"\xc2\xb3")
        );
        !numbered_device
    }

    fn seed(dest: &Path, an: &Analysis, a: &AddArgs, trace: &Trace) -> Result<SeedReport> {
        let mut report = SeedReport::default();
        let Some(d) = &an.donor else {
            trace.note("no donor with matching files; plain checkout");
            return Ok(report);
        };
        report.donor = Some(d.path.clone());
        let t = Instant::now();

        let mut by_oid: HashMap<&str, &[u8]> = HashMap::new();
        for (p, e) in &d.tree.entries {
            if e.is_regular() && !d.dirty.contains(p) && reusable_tree_path(p) {
                let donor_path = by_oid.entry(e.oid.as_str()).or_insert(p.as_slice());
                *donor_path = (*donor_path).min(p);
            }
        }
        let mut plan: Vec<Planned> = Vec::new();
        for (p, e) in &an.target.entries {
            if !e.is_regular() || e.size < a.min_size || !reusable_tree_path(p) {
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

        if !plan.is_empty() {
            let paths: Vec<&[u8]> = plan.iter().map(|p| p.dst).collect();
            let reusable = attrs::reusable_paths(dest, &an.sha, &paths)?;
            plan.retain(|p| reusable.contains(p.dst));
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
        if cloned.is_empty() {
            return Ok(report);
        }

        let t = Instant::now();
        let fmt = git::object_format(dest)?;
        let next = AtomicUsize::new(0);
        let verified = Mutex::new(Vec::with_capacity(cloned.len()));
        std::thread::scope(|s| {
            for _ in 0..hash_workers(cloned.len()) {
                s.spawn(|| {
                    let mut mine = Vec::new();
                    loop {
                        let k = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&i) = cloned.get(k) else { break };
                        let p = &plan[i];
                        if let Some(clone) = index::verify_clone(dest, p.dst, p.entry, fmt) {
                            mine.push(clone);
                        }
                    }
                    verified.lock().expect("lock").extend(mine);
                });
            }
        });
        let verified = verified.into_inner().expect("lock");
        trace.phase("verify", t);

        let t = Instant::now();
        let seeded = index::seed_worktree(dest, verified, fmt)?;
        report.verified = seeded.entries;
        report.bytes = seeded.bytes;
        report.index_written = seeded.entries != 0;
        trace.phase("write index", t);
        Ok(report)
    }

    fn hash_workers(n_items: usize) -> usize {
        let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
        cpus.clamp(4, 16).min(n_items.max(1))
    }

    fn checkout(dest: &Path, a: &AddArgs, report: &mut SeedReport) -> Result<ExitStatus> {
        let mut git = git::cmd(dest);
        git.args(["-c", "checkout.workers=0"]);
        if a.internal {
            git.args(["-c", "core.hooksPath=/dev/null"]);
        }
        git.args(["reset", "--hard", "--no-recurse-submodules"]);
        if a.quiet {
            git.arg("--quiet");
        }
        git.stdin(std::process::Stdio::null());

        let mut status = git.status().context("failed to run git reset")?;
        if !status.success()
            && report.index_written
            && !git::succeeds(dest, &["ls-files", "--cached", "--"])
        {
            eprintln!(
                "sharetree: warning: git rejected the seeded index; retrying a plain checkout"
            );
            fs::remove_file(git::git_path(dest, "index")?).context("removing the seeded index")?;
            report.index_written = false;
            report.cloned = 0;
            report.verified = 0;
            report.bytes = 0;
            status = git.status().context("failed to run git reset")?;
        }

        if status.success()
            && report.index_written
            && !git::succeeds(dest, &["diff-index", "--cached", "--quiet", "HEAD", "--"])
        {
            eprintln!("sharetree: warning: index differs from HEAD after checkout; resetting");
            status = git.status().context("failed to run git reset")?;
        }
        Ok(status)
    }

    #[cfg(test)]
    mod tests {
        use super::{reusable_tree_path, reusable_windows_component, Lifecycle};

        #[test]
        fn lifecycle_accepts_creation_and_rollback_paths() {
            let phase = Lifecycle::Planned
                .advance(Lifecycle::Registered)
                .unwrap()
                .advance(Lifecycle::Populated)
                .unwrap()
                .advance(Lifecycle::Completed)
                .unwrap();
            assert_eq!(phase, Lifecycle::Completed);

            let phase = Lifecycle::Planned
                .advance(Lifecycle::Registered)
                .unwrap()
                .advance(Lifecycle::RolledBack)
                .unwrap();
            assert_eq!(phase, Lifecycle::RolledBack);
        }

        #[test]
        fn lifecycle_rejects_skipped_or_reopened_phases() {
            assert!(Lifecycle::Planned.advance(Lifecycle::Populated).is_err());
            assert!(Lifecycle::Completed.advance(Lifecycle::RolledBack).is_err());
            assert!(Lifecycle::RolledBack
                .advance(Lifecycle::Registered)
                .is_err());
        }

        #[test]
        fn clone_paths_require_normal_relative_components() {
            assert!(reusable_tree_path(b"src/sub directory/file.rs"));
            assert!(reusable_tree_path(b"src/REPO.GIT/file"));
            for path in [
                b"".as_slice(),
                b"src//file",
                b"/absolute",
                b"src/./file",
                b"src/../file",
                b"src/",
                b"src/repo/.git/config",
            ] {
                assert!(!reusable_tree_path(path), "{path:?}");
            }
        }

        #[test]
        fn windows_path_components_exclude_reserved_names() {
            assert!(reusable_windows_component(b"source.rs"));
            assert!(reusable_windows_component(b"component.rs"));
            for name in [
                b"bad:name".as_slice(),
                b"bad\\name",
                b"name.",
                b"NUL.txt",
                b"COM1",
            ] {
                assert!(!reusable_windows_component(name), "{name:?}");
            }
        }
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
        let abbreviated = parse(&os(&["--check", "--no-check", "wt"])).unwrap();
        assert!(abbreviated.no_checkout);
        assert!(abbreviated.forward.is_empty());
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
