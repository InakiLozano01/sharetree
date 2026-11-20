//! Dependency advisor (research note §6): on any filesystem the larger win
//! is usually a shared package store or compiler cache, not the checkout.
//! sharetree detects lockfiles in the worktree and says what each tool will do
//! on this filesystem (clone, hardlink or copy) and what to change.
//!
//! Detection is pure (a list of tracked paths in, findings out) so it is
//! unit-tested; the environment is gathered once into `EnvView`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

use crate::backend::Platform;
use crate::git;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Eco {
    Pnpm,
    Npm,
    Yarn,
    Bun,
    Uv,
    Pip,
    Cargo,
    Go,
}

impl Eco {
    pub fn name(self) -> &'static str {
        match self {
            Eco::Pnpm => "pnpm",
            Eco::Npm => "npm",
            Eco::Yarn => "yarn",
            Eco::Bun => "bun",
            Eco::Uv => "uv",
            Eco::Pip => "pip",
            Eco::Cargo => "cargo",
            Eco::Go => "go",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Detected {
    pub eco: Eco,
    /// Marker files that triggered it (worktree-relative, `/`-separated).
    pub files: Vec<String>,
}

fn eco_of(name: &str) -> Option<Eco> {
    Some(match name {
        "pnpm-lock.yaml" => Eco::Pnpm,
        "package-lock.json" | "npm-shrinkwrap.json" => Eco::Npm,
        "yarn.lock" | ".yarnrc.yml" => Eco::Yarn,
        "bun.lock" | "bun.lockb" => Eco::Bun,
        "uv.lock" => Eco::Uv,
        "poetry.lock" | "Pipfile.lock" => Eco::Pip,
        n if n.starts_with("requirements") && n.ends_with(".txt") => Eco::Pip,
        "Cargo.lock" | "Cargo.toml" => Eco::Cargo,
        "go.mod" => Eco::Go,
        _ => return None,
    })
}

/// Ecosystems used in a tree, from its (tracked) paths.
pub fn detect<'a>(paths: impl IntoIterator<Item = &'a str>) -> Vec<Detected> {
    let mut found: Vec<Detected> = Vec::new();
    for p in paths {
        let comps: Vec<&str> = p.split('/').collect();
        if comps
            .iter()
            .any(|c| *c == "node_modules" || *c == ".git" || *c == "testdata" || *c == "fixtures")
        {
            continue;
        }
        let Some(eco) = comps.last().and_then(|n| eco_of(n)) else {
            continue;
        };
        match found.iter_mut().find(|d| d.eco == eco) {
            Some(d) => d.files.push(p.to_string()),
            None => found.push(Detected {
                eco,
                files: vec![p.to_string()],
            }),
        }
    }
    // pip markers next to a uv project are uv's business.
    if found.iter().any(|d| d.eco == Eco::Uv) {
        found.retain(|d| d.eco != Eco::Pip);
    }
    for d in &mut found {
        d.files.sort();
    }
    found.sort_by_key(|d| d.eco);
    found
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsClass {
    /// The reflink probe succeeded here.
    Reflink,
    /// It failed: tools fall back to hardlinks (same filesystem) or copies.
    NoReflink,
}

/// Environment facts the advice depends on. `*_same_fs`: whether the tool's
/// store/cache is on the worktree's filesystem (hardlinks need that).
#[derive(Clone, Debug, Default)]
pub struct EnvView {
    pub cargo_target_dir: Option<String>,
    pub rustc_wrapper: Option<String>,
    pub sccache_basedirs: Option<String>,
    pub sccache_installed: bool,
    pub uv_link_mode: Option<String>,
    pub uv_cache: Option<PathBuf>,
    pub uv_cache_same_fs: Option<bool>,
    pub bun_cache: Option<PathBuf>,
    pub bun_cache_same_fs: Option<bool>,
    pub pnpm_store: Option<PathBuf>,
    pub pnpm_store_same_fs: Option<bool>,
    pub goflags: Option<String>,
    /// The repo's `.cargo/config.toml` sets `target-dir` / `build-dir`.
    pub cargo_config_shared_dir: bool,
    /// Directory holding the worktrees (for SCCACHE_BASEDIRS).
    pub worktrees_parent: Option<PathBuf>,
}

pub struct Ctx {
    pub fs: String,
    pub class: FsClass,
    pub platform: Platform,
    pub env: EnvView,
}

#[derive(Clone, Debug)]
pub struct Advice {
    /// One line for the hint after `sharetree add`; None when nothing to do.
    pub short: Option<String>,
    /// Everything, for `sharetree doctor`.
    pub details: Vec<String>,
}

fn yes_no(b: Option<bool>) -> &'static str {
    match b {
        Some(true) => "yes",
        Some(false) => "NO",
        None => "unknown",
    }
}

/// The link mode package stores end up with here.
pub fn link_mode_summary(c: &Ctx) -> String {
    match (c.class, c.platform) {
        (FsClass::Reflink, _) => format!(
            "{} can reflink: package stores can clone (copy-on-write), so editing an installed file never reaches the store",
            c.fs
        ),
        (FsClass::NoReflink, Platform::Windows) => format!(
            "{} cannot clone: package stores hardlink (store on the same volume) or copy",
            c.fs
        ),
        (FsClass::NoReflink, _) => format!(
            "{} has no reflinks: package stores hardlink (store on the same filesystem) or copy",
            c.fs
        ),
    }
}

pub fn advise(d: &Detected, c: &Ctx) -> Advice {
    let e = &c.env;
    let reflink = c.class == FsClass::Reflink;
    let mut details = Vec::new();
    let short;
    match d.eco {
        Eco::Pnpm => {
            if let Some(s) = &e.pnpm_store {
                details.push(format!(
                    "store: {} (same filesystem: {})",
                    s.display(),
                    yes_no(e.pnpm_store_same_fs)
                ));
            }
            if reflink {
                short = Some(
                    "set `package-import-method=clone` in .npmrc: node_modules becomes a copy-on-write clone of the store".to_string(),
                );
                details.push("pnpm installs from one content-addressed store; with `package-import-method=clone` each worktree's node_modules shares the store's blocks and edits stay private.".into());
            } else {
                short = Some(match e.pnpm_store_same_fs {
                    Some(false) => "the store is on another filesystem, so it copies; set `store-dir` to a path on this filesystem".to_string(),
                    _ => "hardlinks node_modules from its store (one store per filesystem); never edit files inside node_modules".to_string(),
                });
                details.push("Without reflinks pnpm hardlinks packages from the store when it is on the same filesystem (it keeps one store per disk), otherwise it copies.".into());
                details.push("A hardlinked file in node_modules *is* the store's file: patch packages with `pnpm patch`, not in place.".into());
            }
            details.push("Optional: `enable-global-virtual-store=true` shares the .pnpm layout itself across projects through symlinks.".into());
        }
        Eco::Npm => {
            short = Some(if reflink {
                "copies every package per worktree; `sharetree add --warm` reflinks node_modules, or switch to pnpm".to_string()
            } else {
                "copies every package into each worktree; pnpm (shared store, hardlinks) avoids that".to_string()
            });
            details.push(
                "npm keeps a download cache but writes a full node_modules per worktree.".into(),
            );
        }
        Eco::Yarn => {
            let berry = d.files.iter().any(|f| f.ends_with(".yarnrc.yml"));
            short = Some(
                "Yarn Berry shares its cache already; `nodeLinker: pnp` (or `nodeLinker: pnpm`) avoids a full node_modules per worktree".to_string(),
            );
            details.push(if berry {
                "Yarn Berry: `enableGlobalCache` is on by default, so the zip cache is shared.".into()
            } else {
                "Yarn: with Yarn Berry the global cache is shared; Yarn 1 copies into each node_modules.".into()
            });
            details.push("`nmMode: hardlinks-global` hardlinks node_modules from the global store (same edit caveat as pnpm).".into());
        }
        Eco::Bun => {
            if let Some(s) = &e.bun_cache {
                details.push(format!(
                    "cache: {} (same filesystem: {})",
                    s.display(),
                    yes_no(e.bun_cache_same_fs)
                ));
            }
            short = Some(match (c.platform, e.bun_cache_same_fs) {
                (Platform::MacOs, _) => "clones (clonefile) from its cache".to_string(),
                (_, Some(false)) => "the cache is on another filesystem, so bun copies; set BUN_INSTALL_CACHE_DIR to this filesystem".to_string(),
                _ => "hardlinks from ~/.bun/install/cache (default backend on Linux and Windows); don't edit node_modules in place".to_string(),
            });
        }
        Eco::Uv => {
            if let Some(s) = &e.uv_cache {
                details.push(format!(
                    "cache: {} (same filesystem: {})",
                    s.display(),
                    yes_no(e.uv_cache_same_fs)
                ));
            }
            if let Some(m) = &e.uv_link_mode {
                details.push(format!("UV_LINK_MODE={m}"));
            }
            short = Some(match (reflink, c.platform, e.uv_cache_same_fs) {
                (_, _, Some(false)) => "the cache is on another filesystem, so every .venv is a full copy; set UV_CACHE_DIR to a path on this filesystem".to_string(),
                (true, Platform::Windows, _) => "hardlinks from its cache (Windows default); `UV_LINK_MODE=clone` clones on ReFS".to_string(),
                (true, _, _) => "clones each .venv from its cache (link-mode clone, the default): copy-on-write".to_string(),
                (false, _, _) => "cannot clone here, so it hardlinks from its cache; set UV_LINK_MODE=hardlink to make that explicit".to_string(),
            });
            details.push("A .venv holds absolute paths: create it per worktree with `uv sync` (sharetree --warm never clones it).".into());
            details.push(
                "Never use link-mode symlink: clearing the cache breaks every environment.".into(),
            );
        }
        Eco::Pip => {
            short = Some(
                "pip/poetry copy every package into each venv; uv (`uv venv && uv pip sync`) shares one cache".to_string(),
            );
            details.push("Each venv must be created per worktree (absolute paths).".into());
        }
        Eco::Cargo => {
            let parent = e
                .worktrees_parent
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<worktrees parent>".into());
            let sccache_on = e
                .rustc_wrapper
                .as_deref()
                .is_some_and(|w| w.contains("sccache"));
            let shared_target = e.cargo_target_dir.is_some() || e.cargo_config_shared_dir;
            short = Some(if shared_target {
                format!(
                    "a shared cargo target-dir{} serializes builds across worktrees (exclusive build lock); use sccache instead",
                    e.cargo_target_dir
                        .as_deref()
                        .map(|d| format!(" (CARGO_TARGET_DIR={d})"))
                        .unwrap_or_default()
                )
            } else if !sccache_on {
                format!(
                    "share compiled crates with sccache: RUSTC_WRAPPER=sccache SCCACHE_BASEDIRS={parent} (not a shared target-dir)"
                )
            } else if e.sccache_basedirs.is_none() {
                format!("sccache is on; set SCCACHE_BASEDIRS={parent} so worktree paths don't defeat its cache")
            } else {
                "sccache is set up; each worktree keeps its own target/".to_string()
            });
            details.push("The registry and git sources in CARGO_HOME are shared already.".into());
            details.push("Never point worktrees at one target-dir: cargo holds an exclusive lock on the build dir for the whole build, so parallel builds wait.".into());
            if !e.sccache_installed {
                details.push("sccache is not on PATH (`cargo install sccache`).".into());
            }
            details.push(
                "Binaries, proc-macros and incremental crates are not cached by sccache.".into(),
            );
        }
        Eco::Go => {
            let rw = e
                .goflags
                .as_deref()
                .is_some_and(|f| f.contains("-modcacherw"));
            short = rw.then(|| {
                "GOFLAGS has -modcacherw: editors can then modify the shared module cache"
                    .to_string()
            });
            details.push(
                "The module cache is shared by every worktree and read-only; nothing to do.".into(),
            );
        }
    }
    Advice { short, details }
}

// ---------------------------------------------------------------------------
// Gathering facts

fn env_s(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

fn home() -> Option<PathBuf> {
    env_s("HOME")
        .or_else(|| env_s("USERPROFILE"))
        .map(PathBuf::from)
}

/// Same filesystem as `wt`? Judged at the nearest existing ancestor, since a
/// store that does not exist yet will be created there.
pub fn same_fs(a: &Path, wt: &Path) -> Option<bool> {
    let a = crate::util::nearest_existing(a);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let x = std::fs::metadata(&a).ok()?.dev();
        let y = std::fs::metadata(wt).ok()?.dev();
        Some(x == y)
    }
    #[cfg(not(unix))]
    {
        let root = |p: &Path| {
            p.components()
                .next()
                .map(|c| c.as_os_str().to_ascii_lowercase())
        };
        Some(root(&a) == root(wt))
    }
}

fn uv_cache_dir() -> Option<PathBuf> {
    if let Some(d) = env_s("UV_CACHE_DIR") {
        return Some(PathBuf::from(d));
    }
    if cfg!(windows) {
        return env_s("LOCALAPPDATA").map(|d| PathBuf::from(d).join("uv").join("cache"));
    }
    env_s("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".cache")))
        .map(|d| d.join("uv"))
}

fn bun_cache_dir() -> Option<PathBuf> {
    if let Some(d) = env_s("BUN_INSTALL_CACHE_DIR") {
        return Some(PathBuf::from(d));
    }
    env_s("BUN_INSTALL")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".bun")))
        .map(|d| d.join("install").join("cache"))
}

fn on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    let exts: &[&str] = if cfg!(windows) {
        &[".exe", ".cmd", ""]
    } else {
        &[""]
    };
    std::env::split_paths(&path)
        .any(|d| exts.iter().any(|x| d.join(format!("{name}{x}")).is_file()))
}

fn cargo_config_shares_dir(wt: &Path) -> bool {
    [".cargo/config.toml", ".cargo/config"].iter().any(|f| {
        std::fs::read_to_string(wt.join(f)).is_ok_and(|t| {
            t.lines().any(|l| {
                let l = l.trim_start();
                l.starts_with("target-dir") || l.starts_with("build-dir")
            })
        })
    })
}

/// `exact`: ask pnpm itself for its store (slow: starts node), for doctor.
pub fn gather_env(wt: &Path, detected: &[Detected], exact: bool) -> EnvView {
    let has = |e: Eco| detected.iter().any(|d| d.eco == e);
    let mut v = EnvView {
        cargo_target_dir: env_s("CARGO_TARGET_DIR").or_else(|| env_s("CARGO_BUILD_TARGET_DIR")),
        rustc_wrapper: env_s("RUSTC_WRAPPER").or_else(|| env_s("CARGO_BUILD_RUSTC_WRAPPER")),
        sccache_basedirs: env_s("SCCACHE_BASEDIRS"),
        uv_link_mode: env_s("UV_LINK_MODE"),
        goflags: env_s("GOFLAGS"),
        worktrees_parent: wt.parent().map(Path::to_path_buf),
        ..Default::default()
    };
    if has(Eco::Cargo) {
        v.sccache_installed = on_path("sccache");
        v.cargo_config_shared_dir = cargo_config_shares_dir(wt);
    }
    if has(Eco::Uv) {
        v.uv_cache = uv_cache_dir();
        v.uv_cache_same_fs = v.uv_cache.as_deref().and_then(|c| same_fs(c, wt));
    }
    if has(Eco::Bun) {
        v.bun_cache = bun_cache_dir();
        v.bun_cache_same_fs = v.bun_cache.as_deref().and_then(|c| same_fs(c, wt));
    }
    if has(Eco::Pnpm) {
        let asked = if exact && on_path("pnpm") {
            std::process::Command::new("pnpm")
                .args(["store", "path"])
                .current_dir(wt)
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
                .filter(|p| p.is_absolute())
        } else {
            None
        };
        // Without asking pnpm: it keeps one store per disk (creating
        // `<fs root>/.pnpm-store` when the default store is elsewhere), so
        // hardlinking is the expected outcome; leave the store unknown.
        if let Some(p) = asked {
            v.pnpm_store_same_fs = same_fs(&p, wt);
            v.pnpm_store = Some(p);
        }
    }
    v
}

/// Tracked paths of a worktree.
pub fn tracked_paths(wt: &Path) -> Vec<String> {
    git::output(wt, &["ls-files", "-z"])
        .map(|o| {
            git::split_z(&o)
                .into_iter()
                .map(|p| String::from_utf8_lossy(&p).into_owned())
                .collect()
        })
        .unwrap_or_default()
}

pub fn fs_name(p: &Path) -> String {
    #[cfg(target_os = "linux")]
    {
        crate::linux::fs_name(p).to_string()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = p;
        if cfg!(windows) {
            "this volume".into()
        } else {
            "this filesystem".into()
        }
    }
}

pub fn fs_class(p: &Path) -> FsClass {
    #[cfg(unix)]
    {
        if crate::reflink::probe(p).is_ok() {
            FsClass::Reflink
        } else {
            FsClass::NoReflink
        }
    }
    #[cfg(not(unix))]
    {
        let _ = p;
        FsClass::NoReflink
    }
}

/// The short hint printed after `sharetree add`. Silent when no ecosystem
/// needs a change.
pub fn print_add_hint(wt: &Path) {
    if std::env::var_os("SHARETREE_NO_ADVICE").is_some_and(|v| !v.is_empty() && v != "0") {
        return;
    }
    let paths = tracked_paths(wt);
    let detected = detect(paths.iter().map(String::as_str));
    if detected.is_empty() {
        return;
    }
    let ctx = Ctx {
        fs: fs_name(wt),
        class: fs_class(wt),
        platform: Platform::current(),
        env: gather_env(wt, &detected, false),
    };
    let lines: Vec<(Eco, String)> = detected
        .iter()
        .filter_map(|d| advise(d, &ctx).short.map(|s| (d.eco, s)))
        .collect();
    if lines.is_empty() {
        return;
    }
    eprintln!("sharetree: dependencies: {}", link_mode_summary(&ctx));
    for (eco, l) in lines.iter().take(4) {
        eprintln!("sharetree:   {:<5} {l}", eco.name());
    }
    eprintln!("sharetree:   (details: `sharetree doctor`; silence: SHARETREE_NO_ADVICE=1)");
}

/// `sharetree doctor [<path>]`.
pub fn run_doctor(args: &[OsString]) -> Result<i32> {
    let mut path: Option<PathBuf> = None;
    for a in args {
        match a.to_str() {
            Some(s) if s.starts_with('-') => bail!("doctor: unknown option {s}"),
            _ if path.is_none() => path = Some(PathBuf::from(a)),
            _ => bail!("usage: sharetree doctor [<path>]"),
        }
    }
    let cwd = std::env::current_dir()?;
    let start = crate::util::absolutize(path.as_deref().unwrap_or(Path::new(".")), &cwd);
    let wt = git::toplevel(&start).unwrap_or_else(|_| start.clone());
    let fs = fs_name(&wt);
    let class = fs_class(&wt);
    println!("worktree    {}", wt.display());
    println!(
        "filesystem  {fs}: {}",
        match class {
            FsClass::Reflink => "reflinks work (sharetree add shares blocks)",
            FsClass::NoReflink => "no reflinks (sharetree add alone writes full checkouts)",
        }
    );

    // Backend `sharetree add` would pick for a sibling of this worktree.
    if let Ok(common) = git::common_dir(&wt) {
        let cfg = crate::config::Config::load()?;
        let vol = cfg.volume_for(&common);
        let decision = crate::backend::decide(
            &crate::backend::Inputs {
                probe_ok: class == FsClass::Reflink && cfg!(unix),
                volume: vol.map(|v| v.name.clone()),
                auto_volume: cfg.auto_volume_for(&common),
                ..Default::default()
            },
            Platform::current(),
        );
        let what = match &decision {
            crate::backend::Decision::Reflink { .. } => {
                "reflink beside the destination".to_string()
            }
            crate::backend::Decision::IntoVolume(v) => {
                format!("place the worktree in volume '{v}' (auto_volume)")
            }
            crate::backend::Decision::Plain(h) => format!(
                "plain `git worktree add`{}",
                crate::backend::hint_text(h, &fs)
                    .map(|t| format!("\n            {t}"))
                    .unwrap_or_default()
            ),
            crate::backend::Decision::Ask(v) => format!(
                "ask whether to place the worktree in volume '{v}' (auto_volume is not set; \
                 without a terminal: plain `git worktree add`)"
            ),
            crate::backend::Decision::Fail(m) => m.clone(),
        };
        println!("sharetree add {what}");
        match vol {
            Some(v) => println!(
                "volume      '{}' ({}, {}); auto_volume = {}",
                v.name,
                v.kind.as_str(),
                crate::setup::volume_state_line(v),
                crate::setup::auto_desc(cfg.auto_volume_for(&common))
            ),
            None => println!("volume      none configured"),
        }
    }

    let paths = tracked_paths(&wt);
    let detected = detect(paths.iter().map(String::as_str));
    let ctx = Ctx {
        fs,
        class,
        platform: Platform::current(),
        env: gather_env(&wt, &detected, true),
    };
    println!();
    if detected.is_empty() {
        println!("dependencies: no lockfiles found (pnpm, npm, yarn, bun, uv, pip, cargo, go)");
        return Ok(0);
    }
    println!("dependencies: {}", link_mode_summary(&ctx));
    for d in &detected {
        let a = advise(d, &ctx);
        let shown: Vec<&str> = d.files.iter().take(3).map(String::as_str).collect();
        let more = d.files.len().saturating_sub(3);
        println!(
            "\n{} ({}{})",
            d.eco.name(),
            shown.join(", "),
            if more > 0 {
                format!(", +{more}")
            } else {
                String::new()
            }
        );
        if let Some(s) = &a.short {
            println!("  -> {s}");
        }
        for l in &a.details {
            println!("     {l}");
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(class: FsClass, platform: Platform) -> Ctx {
        Ctx {
            fs: "ext4".into(),
            class,
            platform,
            env: EnvView {
                worktrees_parent: Some(PathBuf::from("/src")),
                ..Default::default()
            },
        }
    }

    fn one(eco: Eco) -> Detected {
        Detected {
            eco,
            files: vec!["x".into()],
        }
    }

    #[test]
    fn detects_lockfiles_at_any_depth_but_not_in_node_modules() {
        let d = detect([
            "pnpm-lock.yaml",
            "apps/web/package.json",
            "node_modules/foo/package-lock.json",
            "services/api/uv.lock",
            "services/api/requirements-dev.txt",
            "crates/core/Cargo.toml",
            "Cargo.lock",
            "tools/go.mod",
            "README.md",
            "testdata/go.mod",
        ]);
        let ecos: Vec<Eco> = d.iter().map(|x| x.eco).collect();
        assert_eq!(ecos, vec![Eco::Pnpm, Eco::Uv, Eco::Cargo, Eco::Go]);
        assert_eq!(d[2].files, vec!["Cargo.lock", "crates/core/Cargo.toml"]);
        assert!(detect(["src/main.c"]).is_empty());
    }

    #[test]
    fn pip_without_uv_and_bun_variants() {
        let d = detect([
            "requirements.txt",
            "bun.lockb",
            "yarn.lock",
            "package-lock.json",
        ]);
        let ecos: Vec<Eco> = d.iter().map(|x| x.eco).collect();
        assert_eq!(ecos, vec![Eco::Npm, Eco::Yarn, Eco::Bun, Eco::Pip]);
    }

    #[test]
    fn link_mode_is_stated_per_filesystem() {
        assert!(link_mode_summary(&ctx(FsClass::NoReflink, Platform::Linux))
            .contains("ext4 has no reflinks: package stores hardlink"));
        let mut c = ctx(FsClass::Reflink, Platform::Linux);
        c.fs = "btrfs".into();
        assert!(link_mode_summary(&c).contains("btrfs can reflink"));
        let mut c = ctx(FsClass::NoReflink, Platform::Windows);
        c.fs = "NTFS".into();
        assert!(link_mode_summary(&c).contains("NTFS cannot clone"));
    }

    #[test]
    fn pnpm_advice_depends_on_reflinks_and_store_location() {
        let s = advise(&one(Eco::Pnpm), &ctx(FsClass::Reflink, Platform::Linux))
            .short
            .unwrap();
        assert!(s.contains("package-import-method=clone"), "{s}");
        let mut c = ctx(FsClass::NoReflink, Platform::Linux);
        let s = advise(&one(Eco::Pnpm), &c).short.unwrap();
        assert!(s.contains("hardlinks"), "{s}");
        c.env.pnpm_store_same_fs = Some(false);
        let s = advise(&one(Eco::Pnpm), &c).short.unwrap();
        assert!(s.contains("store-dir"), "{s}");
    }

    #[test]
    fn uv_advice_per_filesystem() {
        let s = advise(&one(Eco::Uv), &ctx(FsClass::Reflink, Platform::Linux))
            .short
            .unwrap();
        assert!(s.contains("link-mode clone"), "{s}");
        let s = advise(&one(Eco::Uv), &ctx(FsClass::NoReflink, Platform::Linux))
            .short
            .unwrap();
        assert!(s.contains("UV_LINK_MODE=hardlink"), "{s}");
        let mut c = ctx(FsClass::NoReflink, Platform::Linux);
        c.env.uv_cache_same_fs = Some(false);
        let s = advise(&one(Eco::Uv), &c).short.unwrap();
        assert!(s.contains("UV_CACHE_DIR"), "{s}");
        let a = advise(&one(Eco::Uv), &ctx(FsClass::NoReflink, Platform::Linux));
        assert!(a.details.iter().any(|l| l.contains("symlink")));
    }

    #[test]
    fn cargo_never_recommends_a_shared_target_dir() {
        let a = advise(&one(Eco::Cargo), &ctx(FsClass::NoReflink, Platform::Linux));
        let s = a.short.unwrap();
        assert!(
            s.contains("RUSTC_WRAPPER=sccache SCCACHE_BASEDIRS=/src"),
            "{s}"
        );
        assert!(a.details.iter().any(|l| l.contains("exclusive lock")));

        let mut c = ctx(FsClass::Reflink, Platform::Linux);
        c.env.cargo_target_dir = Some("/tmp/shared".into());
        let s = advise(&one(Eco::Cargo), &c).short.unwrap();
        assert!(
            s.contains("CARGO_TARGET_DIR=/tmp/shared") && s.contains("serializes"),
            "{s}"
        );

        let mut c = ctx(FsClass::Reflink, Platform::Linux);
        c.env.rustc_wrapper = Some("/usr/bin/sccache".into());
        let s = advise(&one(Eco::Cargo), &c).short.unwrap();
        assert!(s.contains("SCCACHE_BASEDIRS=/src"), "{s}");
        c.env.sccache_basedirs = Some("/src".into());
        let s = advise(&one(Eco::Cargo), &c).short.unwrap();
        assert!(s.contains("set up"), "{s}");
    }

    #[test]
    fn go_is_quiet_unless_modcacherw() {
        assert!(
            advise(&one(Eco::Go), &ctx(FsClass::NoReflink, Platform::Linux))
                .short
                .is_none()
        );
        let mut c = ctx(FsClass::NoReflink, Platform::Linux);
        c.env.goflags = Some("-mod=mod -modcacherw".into());
        assert!(advise(&one(Eco::Go), &c)
            .short
            .unwrap()
            .contains("-modcacherw"));
    }

    #[test]
    fn npm_bun_yarn_pip_have_advice() {
        for eco in [Eco::Npm, Eco::Bun, Eco::Yarn, Eco::Pip] {
            assert!(advise(&one(eco), &ctx(FsClass::NoReflink, Platform::Linux))
                .short
                .is_some());
        }
        let s = advise(&one(Eco::Npm), &ctx(FsClass::Reflink, Platform::Linux))
            .short
            .unwrap();
        assert!(s.contains("--warm"), "{s}");
    }

    #[test]
    fn same_fs_uses_nearest_existing_ancestor() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(
            same_fs(&d.path().join("not/yet/created"), d.path()),
            Some(true)
        );
    }
}
