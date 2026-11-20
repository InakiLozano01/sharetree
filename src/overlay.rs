//! Overlay worktrees (Linux, opt-in `sharetree add --overlay`): for
//! filesystems without reflinks (ext4, tmpfs), per the ext4/NTFS research
//! note §1.3–1.5 and §7.5.
//!
//! * **Lower layer**: an immutable *base* checkout per commit, a detached
//!   worktree at `$XDG_DATA_HOME/sharetree/overlay/<repo-id>/bases/<commit>`,
//!   locked so `git gc` never prunes it. The overlay docs forbid changing a
//!   lower layer while it is mounted, so the base is never the main worktree
//!   and nothing writes to it.
//! * **Upper/work dirs** per worktree under `.../wt/<name>/`, on the same
//!   filesystem as the base, so fuse-overlayfs reports the lower inode
//!   numbers and git's cached stat data stays valid.
//! * The worktree's `.git` file is moved into the upper dir, the
//!   fuse-overlayfs mount goes on the path the user asked for, and the
//!   worktree's index is **seeded from the base's settled index**: stat data
//!   match through the overlay (same inode, mtime, ctime, size), so nothing
//!   is copied up and the first `git status` needs no rehash.
//! * Each overlay worktree is **locked** (`sharetree overlay: ...`): while
//!   unmounted its path holds no `.git`, and `git worktree prune` would
//!   otherwise drop it.
//! * Mounts do not survive a reboot: `sharetree mount --all` remounts every
//!   overlay; `add` installs the systemd user unit `sharetree-overlays.service`
//!   that runs it at login. `sharetree rm` unmounts, deletes upper and work,
//!   and removes the base once no overlay uses it.
//!
//! Costs (research §1.3–1.4): the first write to a file copies the whole file
//! into the upper dir (even opening it read-write); renaming a directory
//! from the lower layer copies the tree (EXDEV); reads go through FUSE and
//! were 3–6x slower than plain ext4 for `git grep`, `find` and `cat`.

// Generators and parsers are portable so their tests run everywhere.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub const LOCK_PREFIX: &str = "sharetree overlay";
pub const BASE_LOCK_REASON: &str =
    "sharetree overlay base: read-only lower layer of overlay worktrees; do not edit";
pub const UNIT_NAME: &str = "sharetree-overlays.service";
/// fuse-overlayfs 1.18 fixed CVE-2026-77478 (hardlinked lower files
/// resolving to the wrong layer); older versions only get a warning.
pub const MIN_VERSION: (u32, u32) = (1, 18);

pub fn lock_reason(name: &str) -> String {
    format!(
        "{LOCK_PREFIX} '{name}': mounted with fuse-overlayfs; kept registered while unmounted; `sharetree rm` cleans up"
    )
}

pub fn is_overlay_lock(reason: Option<&str>) -> bool {
    reason.is_some_and(|r| r.starts_with(LOCK_PREFIX) && r != BASE_LOCK_REASON)
}

pub fn is_base_lock(reason: Option<&str>) -> bool {
    reason == Some(BASE_LOCK_REASON)
}

/// Where a repository's bases and overlay worktrees live.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub root: PathBuf,
}

impl Layout {
    pub fn new(data_dir: &Path, repo_id: &str) -> Layout {
        Layout {
            root: data_dir.join("overlay").join(repo_id),
        }
    }

    pub fn bases(&self) -> PathBuf {
        self.root.join("bases")
    }

    pub fn base(&self, commit: &str) -> PathBuf {
        self.bases().join(commit)
    }

    pub fn worktrees(&self) -> PathBuf {
        self.root.join("wt")
    }
}

/// Escape a path for a fuse-overlayfs `-o` option value: `,` separates
/// options and `:` separates lower dirs, both escaped with a backslash.
pub fn escape_opt(p: &Path) -> String {
    let s = p.to_string_lossy();
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, ',' | ':' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// fuse-overlayfs arguments for one worktree.
pub fn mount_args(lower: &Path, upper: &Path, work: &Path, mountpoint: &Path) -> Vec<OsString> {
    vec![
        "-o".into(),
        format!(
            "lowerdir={},upperdir={},workdir={}",
            escape_opt(lower),
            escape_opt(upper),
            escape_opt(work)
        )
        .into(),
        mountpoint.as_os_str().to_owned(),
    ]
}

/// Parse `fuse-overlayfs --version` ("fuse-overlayfs: version 1.18").
pub fn parse_version(out: &str) -> Option<(u32, u32)> {
    let v = out
        .split_whitespace()
        .skip_while(|w| *w != "version")
        .nth(1)?;
    let mut it = v.split('.').map(|x| {
        x.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse::<u32>()
    });
    Some((
        it.next()?.ok()?,
        it.next().and_then(Result::ok).unwrap_or(0),
    ))
}

/// systemd user unit that remounts every overlay worktree at login.
pub fn systemd_unit(exe: &Path, config: &Path) -> String {
    let mut env_q = String::from("\"");
    for c in format!("SHARETREE_CONFIG={}", config.display()).chars() {
        match c {
            '"' | '\\' => {
                env_q.push('\\');
                env_q.push(c);
            }
            '%' => env_q.push_str("%%"),
            _ => env_q.push(c),
        }
    }
    env_q.push('"');
    format!(
        "# Generated by sharetree {ver}. Removed by `sharetree rm` with the last overlay worktree.\n\
         [Unit]\n\
         Description=sharetree overlay worktrees (fuse-overlayfs)\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         Environment={env_q}\n\
         ExecStart={exe} mount --all\n\
         ExecStop={exe} mount --all --unmount\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        ver = env!("CARGO_PKG_VERSION"),
        exe = crate::volume::systemd_quote(&exe.display().to_string()),
    )
}

pub const COSTS: &str = "\
overlay worktrees (fuse-overlayfs) cost little space but behave differently:
  - the first write to a file copies the whole file into the upper dir (even
    opening it read-write without writing);
  - renaming a directory that comes from the base copies the whole tree;
  - reads go through FUSE: 3-6x slower than plain ext4 for git grep, find, cat;
  - the worktree exists only while mounted: `sharetree mount --all` (the
    systemd user unit sharetree-overlays.service runs it at login).";

#[cfg(target_os = "linux")]
pub use self::linux::*;

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant, SystemTime};

    use anyhow::{anyhow, bail, Context, Result};

    use super::*;
    use crate::add::AddArgs;
    use crate::config::{self, Config, OverlayEntry};
    use crate::util::{self, human};
    use crate::{git, volume};

    fn fuse_overlayfs() -> Result<PathBuf> {
        if let Some(p) = std::env::var_os("SHARETREE_FUSE_OVERLAYFS").filter(|v| !v.is_empty()) {
            return Ok(PathBuf::from(p));
        }
        volume::find_in_path("fuse-overlayfs").ok_or_else(|| {
            anyhow!(
                "--overlay needs fuse-overlayfs (package fuse-overlayfs), which is not installed"
            )
        })
    }

    /// fuse-overlayfs, fusermount3 and /dev/fuse, or why not.
    pub fn prerequisites() -> Result<PathBuf> {
        let bin = fuse_overlayfs()?;
        if volume::find_in_path("fusermount3").is_none() {
            bail!("--overlay needs fusermount3 (package fuse3)");
        }
        if !Path::new("/dev/fuse").exists() {
            bail!("--overlay needs /dev/fuse (is the fuse module loaded?)");
        }
        let out = Command::new(&bin)
            .arg("--version")
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("running {}", bin.display()))?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        match parse_version(&text) {
            Some(v) if v < MIN_VERSION => eprintln!(
                "sharetree: warning: fuse-overlayfs {}.{} is older than {}.{}, which fixed \
                 CVE-2026-77478 (hardlinked lower files resolving to the wrong layer)",
                v.0, v.1, MIN_VERSION.0, MIN_VERSION.1
            ),
            Some(_) => {}
            None => eprintln!(
                "sharetree: warning: cannot tell the fuse-overlayfs version from {:?}",
                text.trim()
            ),
        }
        Ok(bin)
    }

    pub fn is_mounted(path: &Path) -> bool {
        let want = util::canon(path);
        volume::mounts()
            .iter()
            .any(|m| m.mountpoint == want && m.fstype.contains("fuse-overlayfs"))
    }

    pub fn mount(e: &OverlayEntry) -> Result<()> {
        if is_mounted(&e.path) {
            return Ok(());
        }
        for d in [&e.lower, &e.upper] {
            if !d.is_dir() {
                bail!("overlay {}: {} is missing", e.path.display(), d.display());
            }
        }
        fs::create_dir_all(&e.work)?;
        fs::create_dir_all(&e.path)?;
        if fs::read_dir(&e.path)?.next().is_some() {
            bail!(
                "cannot mount the overlay on {}: the directory is not empty",
                e.path.display()
            );
        }
        let bin = fuse_overlayfs()?;
        let out = Command::new(&bin)
            .args(mount_args(&e.lower, &e.upper, &e.work, &e.path))
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("running {}", bin.display()))?;
        if !out.status.success() {
            bail!(
                "fuse-overlayfs on {}: {}",
                e.path.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let t = Instant::now();
        while !is_mounted(&e.path) {
            if t.elapsed() > Duration::from_secs(5) {
                bail!("{} did not appear as a mount", e.path.display());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    }

    pub fn unmount(path: &Path) -> Result<()> {
        if !is_mounted(path) {
            return Ok(());
        }
        let out = Command::new("fusermount3")
            .arg("-u")
            .arg(path)
            .stdin(Stdio::null())
            .output()
            .context("running fusermount3")?;
        if !out.status.success() {
            bail!(
                "fusermount3 -u {}: {}",
                path.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    /// Delete a tree that may contain the kernel's or fuse's `d---------`
    /// work directories (research §1.5): make everything writable first.
    fn remove_tree(p: &Path) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        fn walk(p: &Path) {
            let Ok(md) = p.symlink_metadata() else { return };
            if md.is_dir() {
                let _ = fs::set_permissions(p, fs::Permissions::from_mode(0o700));
                for e in fs::read_dir(p).into_iter().flatten().flatten() {
                    walk(&e.path());
                }
            }
        }
        walk(p);
        match fs::remove_dir_all(p) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("removing {}", p.display())),
        }
    }

    /// Wait until the wall clock is past every file just written, then let
    /// git refresh the base's index. Entries are then older than the index,
    /// so git never treats them as racily clean, and a copy of this index is
    /// stat-valid for every overlay on the base.
    fn settle_index(base: &Path) -> Result<()> {
        let newest = SystemTime::now();
        // git compares whole seconds unless built with USE_NSEC.
        let secs = newest
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        while SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
            <= secs
        {
            std::thread::sleep(Duration::from_millis(50));
        }
        git::output(base, &["update-index", "-q", "--really-refresh"])?;
        Ok(())
    }

    fn base_commit(dir: &Path) -> Option<String> {
        let name = dir.file_name()?.to_str()?;
        (name.len() >= 40 && name.bytes().all(|b| b.is_ascii_hexdigit())).then(|| name.into())
    }

    /// A base for `commit`: the exact one if it exists, else the existing
    /// base whose tree differs least when that is under 5 % of the files
    /// (git then writes the differences into the upper dir), else a new one.
    fn ensure_base(
        cwd: &Path,
        layout: &Layout,
        commit: &str,
        quiet: bool,
    ) -> Result<(PathBuf, String)> {
        let wts = git::list_worktrees(cwd)?;
        let registered = |p: &Path| {
            wts.iter()
                .any(|w| util::canon(&w.path) == util::canon(p) && p.join(".git").is_file())
        };
        let exact = layout.base(commit);
        if registered(&exact) {
            return Ok((exact, commit.to_string()));
        }
        let total = git::ls_tree(cwd, commit)?.entries.len().max(1);
        let mut best: Option<(usize, PathBuf, String)> = None;
        for e in fs::read_dir(layout.bases()).into_iter().flatten().flatten() {
            let p = e.path();
            let Some(c) = base_commit(&p) else { continue };
            if !registered(&p) {
                continue;
            }
            let Ok(out) = git::output(cwd, &["diff", "--name-only", "-z", &c, commit]) else {
                continue;
            };
            let n = git::split_z(&out).len();
            if best.as_ref().is_none_or(|b| n < b.0) {
                best = Some((n, p, c));
            }
        }
        if let Some((n, p, c)) = best {
            if n * 20 <= total {
                return Ok((p, c));
            }
        }
        if exact.symlink_metadata().is_ok() {
            remove_tree(&exact)?;
        }
        let _ = git::output(cwd, &["worktree", "prune"]);
        fs::create_dir_all(layout.bases())?;
        if !quiet {
            eprintln!(
                "sharetree: creating the overlay base for {} at {} (one full checkout per commit; overlays share it)",
                &commit[..12.min(commit.len())],
                exact.display()
            );
        }
        let st = git::cmd(cwd)
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "worktree",
                "add",
                "-q",
                "--detach",
                "--lock",
                "--reason",
                BASE_LOCK_REASON,
            ])
            .arg(&exact)
            .arg(commit)
            .stdin(Stdio::null())
            .status()
            .context("running git worktree add for the overlay base")?;
        if !st.success() {
            bail!("creating the overlay base {} failed", exact.display());
        }
        settle_index(&exact)?;
        Ok((exact, commit.to_string()))
    }

    fn dir_bytes(p: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt;
        let Ok(md) = p.symlink_metadata() else {
            return 0;
        };
        let own = md.blocks() * 512;
        if md.is_dir() {
            own + fs::read_dir(p)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| dir_bytes(&e.path()))
                .sum::<u64>()
        } else {
            own
        }
    }

    fn install_unit_once(quiet: bool) {
        if std::env::var_os("SHARETREE_NO_UNIT").is_some() {
            return;
        }
        let Some(dir) = config::systemd_user_dir() else {
            return;
        };
        let path = dir.join(UNIT_NAME);
        if path.exists() {
            return;
        }
        let res = (|| -> Result<()> {
            let exe = std::env::current_exe()?;
            let cfg = config::config_path().context("no config path")?;
            fs::create_dir_all(&dir)?;
            fs::write(&path, systemd_unit(&exe, &cfg))?;
            volume::systemctl_user(&["daemon-reload"])?;
            volume::systemctl_user(&["enable", UNIT_NAME])?;
            Ok(())
        })();
        match res {
            Ok(()) if !quiet => eprintln!(
                "sharetree: installed and enabled the systemd user unit {UNIT_NAME} (remounts overlay worktrees at login)"
            ),
            Ok(()) => {}
            Err(e) => eprintln!(
                "sharetree: warning: could not install {UNIT_NAME} ({e:#}); run `sharetree mount --all` after each login"
            ),
        }
    }

    fn remove_unit_if_unused(cfg: &Config) {
        if !cfg.overlays.is_empty() || std::env::var_os("SHARETREE_NO_UNIT").is_some() {
            return;
        }
        let Some(dir) = config::systemd_user_dir() else {
            return;
        };
        let path = dir.join(UNIT_NAME);
        if path.exists() {
            let _ = volume::systemctl_user(&["disable", UNIT_NAME]);
            let _ = fs::remove_file(&path);
            let _ = volume::systemctl_user(&["daemon-reload"]);
        }
    }

    /// `sharetree add --overlay ...`
    pub fn add(a: &AddArgs) -> Result<(i32, PathBuf)> {
        let cwd = std::env::current_dir()?;
        let dest = util::absolutize(Path::new(a.path.as_ref().expect("checked")), &cwd);
        prerequisites()?;
        if a.no_checkout || a.orphan {
            bail!("--overlay cannot be combined with --no-checkout or --orphan");
        }
        if git::config_bool(&cwd, "core.sparseCheckout")? == Some(true) {
            bail!("--overlay does not support sparse checkouts");
        }
        if git::config_bool(&cwd, "core.splitIndex")? == Some(true) {
            bail!("--overlay does not support core.splitIndex");
        }
        if dest.symlink_metadata().is_ok()
            && (!dest.is_dir() || fs::read_dir(&dest)?.next().is_some())
        {
            bail!("'{}' already exists", dest.display());
        }
        let common = git::common_dir(&cwd)?;
        let main = git::list_worktrees(&cwd)?
            .into_iter()
            .find(|w| w.is_main)
            .map(|w| w.path)
            .unwrap_or_else(|| common.clone());
        let data = config::data_dir().context("cannot locate a data directory (set HOME)")?;
        let layout = Layout::new(&data, &volume::repo_id(&common, &main));
        let rev = a
            .commitish
            .clone()
            .unwrap_or_else(|| OsString::from("HEAD"));
        let guess = git::rev_parse_commit(&cwd, &rev)
            .or_else(|_| git::rev_parse_commit(&cwd, OsStr::new("HEAD")))?;
        let (base, base_sha) = ensure_base(&cwd, &layout, &guess, a.quiet)?;

        let name = dest
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "worktree".into());
        fs::create_dir_all(layout.worktrees())?;
        let wdir = volume::unique_child(&layout.worktrees(), &name);
        let (upper, work) = (wdir.join("upper"), wdir.join("work"));
        fs::create_dir_all(&upper)?;
        fs::create_dir_all(&work)?;

        // Register (locked), without a checkout.
        let mut args: Vec<OsString> = vec!["worktree".into(), "add".into(), "--no-checkout".into()];
        args.extend(a.forward.iter().cloned());
        if !a.forward.iter().any(|f| f == "--lock") {
            args.extend([
                "--lock".into(),
                "--reason".into(),
                lock_reason(&name).into(),
            ]);
        }
        if a.separator {
            args.push("--".into());
        }
        args.push(dest.clone().into_os_string());
        if let Some(c) = &a.commitish {
            args.push(c.clone());
        }
        let st = git::run_inherit(&cwd, &args)?;
        if !st.success() {
            let _ = remove_tree(&wdir);
            return Ok((st.code().unwrap_or(1), dest));
        }
        let res = (|| -> Result<i32> {
            let target = git::rev_parse_commit(&dest, OsStr::new("HEAD"))?;
            let admin = crate::rm::admin_dir(&dest)?;
            fs::rename(dest.join(".git"), upper.join(".git"))
                .context("moving .git into the upper dir")?;
            let entry = OverlayEntry {
                path: dest.clone(),
                common_dir: common.clone(),
                lower: base.clone(),
                upper: upper.clone(),
                work: work.clone(),
            };
            let mut cfg = Config::load()?;
            cfg.overlays
                .retain(|o| util::canon(&o.path) != util::canon(&dest));
            cfg.overlays.push(entry.clone());
            cfg.save()?;
            mount(&entry)?;

            // Seed the index from the base's settled one.
            let base_index = crate::rm::admin_dir(&base)?.join("index");
            fs::copy(&base_index, admin.join("index")).context("seeding the index")?;
            let out = git::cmd(&dest)
                .args([
                    "-c",
                    "core.hooksPath=/dev/null",
                    "checkout",
                    "-q",
                    "-f",
                    "HEAD",
                ])
                .stdin(Stdio::null())
                .output()
                .context("running git checkout")?;
            if !out.status.success() {
                bail!(
                    "git checkout -f HEAD: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            let zero = "0".repeat(target.len());
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
            if !a.quiet {
                eprintln!(
                    "sharetree: overlay worktree at {} (lower: base {} at {}; upper: {} written so far)",
                    dest.display(),
                    &base_sha[..12.min(base_sha.len())],
                    base.display(),
                    human(dir_bytes(&upper).saturating_sub(dir_bytes(&upper.join(".git"))))
                );
                eprintln!("{COSTS}");
            }
            install_unit_once(a.quiet);
            Ok(if h.success() {
                0
            } else {
                h.code().unwrap_or(1)
            })
        })();
        match res {
            Ok(code) => Ok((code, dest)),
            Err(e) => {
                // Undo: unmount, drop registration and dirs, keep the base.
                let _ = unmount(&dest);
                let _ = git::output(
                    &cwd,
                    &[
                        OsStr::new("worktree"),
                        OsStr::new("remove"),
                        OsStr::new("-f"),
                        OsStr::new("-f"),
                        dest.as_os_str(),
                    ],
                );
                if let Ok(admin) = fs::read_to_string(upper.join(".git")) {
                    if let Some(p) = admin.trim_end().strip_prefix("gitdir: ") {
                        let _ = fs::remove_dir_all(p);
                    }
                }
                let _ = remove_tree(&wdir);
                let _ = fs::remove_dir(&dest);
                if let Ok(mut cfg) = Config::load() {
                    cfg.overlays
                        .retain(|o| util::canon(&o.path) != util::canon(&dest));
                    let _ = cfg.save();
                }
                Err(e)
            }
        }
    }

    /// `sharetree rm` of an overlay worktree: the same safety checks, then
    /// unmount, drop the registration, delete upper/work, and remove the
    /// base once no overlay uses it.
    pub fn remove(cwd: &Path, wt_path: &Path, force: u8) -> Result<i32> {
        let mut cfg = Config::load()?;
        let Some(e) = cfg
            .overlays
            .iter()
            .find(|o| util::canon(&o.path) == util::canon(wt_path))
            .cloned()
        else {
            bail!(
                "{} is locked as an overlay worktree but is not in the sharetree config; \
                 `git worktree unlock` it and remove it with git",
                wt_path.display()
            );
        };
        if !is_mounted(&e.path) {
            if let Err(err) = mount(&e) {
                if force < 2 {
                    bail!("cannot mount {} to check it ({err:#}); `sharetree rm -f -f` removes it unchecked", e.path.display());
                }
            }
        }
        if force == 0 && is_mounted(&e.path) {
            let st = git::output(
                &e.path,
                &[
                    "status",
                    "--porcelain",
                    "-z",
                    "--untracked-files=normal",
                    "--ignore-submodules=none",
                ],
            )?;
            if !st.is_empty() {
                bail!(
                    "{} contains modified or untracked files, use --force to delete it",
                    e.path.display()
                );
            }
        }
        let admin = fs::read_to_string(e.upper.join(".git"))
            .ok()
            .and_then(|t| t.trim_end().strip_prefix("gitdir: ").map(PathBuf::from));
        unmount(&e.path)?;
        match admin {
            Some(a) if a.starts_with(cfg_common(&e)) => {
                fs::remove_dir_all(&a).with_context(|| format!("removing {}", a.display()))?
            }
            _ => {
                let _ = git::output(cwd, &["worktree", "prune"]);
            }
        }
        if let Some(wdir) = e.upper.parent() {
            remove_tree(wdir)?;
        }
        let _ = fs::remove_dir(&e.path);
        cfg.overlays
            .retain(|o| util::canon(&o.path) != util::canon(&e.path));
        cfg.save()?;
        eprintln!("sharetree: removed overlay worktree {}", e.path.display());
        if !cfg.overlays.iter().any(|o| o.lower == e.lower) && e.lower.join(".git").is_file() {
            let ok = git::succeeds(
                cwd,
                &[
                    OsStr::new("worktree"),
                    OsStr::new("remove"),
                    OsStr::new("-f"),
                    OsStr::new("-f"),
                    e.lower.as_os_str(),
                ],
            );
            if ok {
                eprintln!(
                    "sharetree: removed the unused overlay base {}",
                    e.lower.display()
                );
            } else {
                eprintln!(
                    "sharetree: warning: could not remove the overlay base {}",
                    e.lower.display()
                );
            }
        }
        remove_unit_if_unused(&cfg);
        Ok(0)
    }

    fn cfg_common(e: &OverlayEntry) -> PathBuf {
        e.common_dir.join("worktrees")
    }

    /// `sharetree mount [--all] [--unmount] [<path>...]`
    pub fn run_mount(args: &[OsString]) -> Result<i32> {
        let mut all = false;
        let mut un = false;
        let mut paths = Vec::new();
        for a in args {
            match a.to_str() {
                Some("--all") => all = true,
                Some("--unmount" | "-u") => un = true,
                Some(s) if s.starts_with('-') => bail!("mount: unknown option {s}"),
                _ => paths.push(util::absolutize(Path::new(a), &std::env::current_dir()?)),
            }
        }
        if !all && paths.is_empty() {
            bail!("usage: sharetree mount [--unmount] (--all | <worktree>...)");
        }
        let cfg = Config::load()?;
        let chosen: Vec<&OverlayEntry> = cfg
            .overlays
            .iter()
            .filter(|o| all || paths.iter().any(|p| util::canon(p) == util::canon(&o.path)))
            .collect();
        if !all && chosen.len() != paths.len() {
            bail!("not every path given is an overlay worktree (see `sharetree ls`)");
        }
        let mut failed = 0;
        for o in chosen {
            let r = if un { unmount(&o.path) } else { mount(o) };
            match r {
                Ok(()) => println!(
                    "{} {}",
                    if un { "unmounted" } else { "mounted" },
                    o.path.display()
                ),
                Err(e) => {
                    failed += 1;
                    eprintln!("sharetree: {e:#}");
                }
            }
        }
        Ok(i32::from(failed > 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_arguments_escape_separators() {
        let a = mount_args(
            Path::new("/d/bases/abc"),
            Path::new("/d/wt/x,y/upper"),
            Path::new("/d/wt/x:y/work"),
            Path::new("/src/my wt"),
        );
        assert_eq!(a[0], "-o");
        assert_eq!(
            a[1],
            "lowerdir=/d/bases/abc,upperdir=/d/wt/x\\,y/upper,workdir=/d/wt/x\\:y/work"
        );
        assert_eq!(a[2], "/src/my wt");
    }

    #[test]
    fn versions_and_locks() {
        assert_eq!(
            parse_version("fuse-overlayfs: version 1.18\nFUSE library version 3.17.4\n"),
            Some((1, 18))
        );
        assert_eq!(
            parse_version("fuse-overlayfs: version 1.9-dev"),
            Some((1, 9))
        );
        assert_eq!(parse_version("garbage"), None);
        assert!((1, 9) < MIN_VERSION && (1, 18) >= MIN_VERSION);
        assert!(is_overlay_lock(Some(&lock_reason("x"))));
        assert!(!is_overlay_lock(Some(BASE_LOCK_REASON)));
        assert!(is_base_lock(Some(BASE_LOCK_REASON)));
        assert!(!is_overlay_lock(Some("sharetree volume 'v': ...")));
    }

    #[test]
    fn layout_and_unit() {
        let l = Layout::new(Path::new("/home/u/.local/share/sharetree"), "app-1a2b3c4d");
        assert_eq!(
            l.base("abc"),
            PathBuf::from("/home/u/.local/share/sharetree/overlay/app-1a2b3c4d/bases/abc")
        );
        assert!(l.worktrees().ends_with("app-1a2b3c4d/wt"));
        let u = systemd_unit(Path::new("/usr/bin/sharetree"), Path::new("/c/config.toml"));
        assert!(
            u.contains("ExecStart=\"/usr/bin/sharetree\" mount --all\n"),
            "{u}"
        );
        assert!(
            u.contains("Environment=\"SHARETREE_CONFIG=/c/config.toml\""),
            "{u}"
        );
        assert!(u.contains("WantedBy=default.target"));
    }
}
