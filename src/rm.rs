use std::ffi::OsString;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use crate::git;
use crate::util;
use crate::volume;

pub const TRASH_DIR: &str = "sharetree-trash";

struct RmArgs {
    force: u8,
    sync: bool,
    path: PathBuf,
}

fn parse(args: &[OsString]) -> Result<RmArgs> {
    let mut force = 0u8;
    let mut sync = false;
    let mut path = None;
    for a in args {
        match a.to_str() {
            Some("-f" | "--force") => force = force.saturating_add(1).min(2),
            Some("-ff") => force = 2,
            Some("--sync") => sync = true,
            Some(s) if s.starts_with('-') && s.len() > 1 => bail!("rm: unknown option {s}"),
            _ if path.is_none() => path = Some(PathBuf::from(a)),
            _ => bail!("rm: only one worktree at a time"),
        }
    }
    Ok(RmArgs {
        force,
        sync,
        path: path.ok_or_else(|| anyhow!("usage: sharetree rm [-f [-f]] [--sync] <worktree>"))?,
    })
}

pub(crate) fn admin_dir(wt: &Path) -> Result<PathBuf> {
    read_admin_dir(wt, wt)
}

fn read_admin_dir(wt: &Path, original: &Path) -> Result<PathBuf> {
    let bytes = fs::read(wt.join(".git"))
        .with_context(|| format!("{} is not a linked worktree", wt.display()))?;
    let path = git_path(&bytes)
        .strip_prefix(b"gitdir: ")
        .ok_or_else(|| anyhow!("{}/.git is not a gitdir file", wt.display()))?;
    Ok(util::absolutize(&util::bytes_to_path(path), original))
}

fn git_path(bytes: &[u8]) -> &[u8] {
    let path = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    if cfg!(windows) {
        path.strip_suffix(b"\r").unwrap_or(path)
    } else {
        path
    }
}

pub(crate) fn registered_admin(common: &Path, path: &Path) -> Result<Option<PathBuf>> {
    let root = common.join("worktrees");
    let dirs = match fs::read_dir(&root) {
        Ok(dirs) => dirs,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", root.display())),
    };
    let dotgit = path.join(".git");
    for ent in dirs {
        let ent = ent?;
        if !ent.file_type()?.is_dir() {
            continue;
        }
        let admin = ent.path();
        let link = match fs::read(admin.join("gitdir")) {
            Ok(link) => link,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("reading {}/gitdir", admin.display())),
        };
        let target = util::absolutize(&util::bytes_to_path(git_path(&link)), &admin);
        if util::canon(&target) == util::canon(&dotgit) {
            return Ok(Some(admin));
        }
    }
    Ok(None)
}

fn validate_admin(common: &Path, admin: &Path) -> Result<()> {
    if admin.parent().map(util::canon) != Some(util::canon(&common.join("worktrees")))
        || admin
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_symlink())
    {
        bail!("unexpected Git worktree directory {}", admin.display());
    }
    Ok(())
}

pub(crate) fn check_clean(path: &Path, admin: &Path) -> Result<()> {
    let st = git::output(
        path,
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
            path.display()
        );
    }
    if admin.join("modules").try_exists()? {
        bail!(
            "{} contains initialized submodules, use --force to delete it",
            path.display()
        );
    }
    Ok(())
}

fn remove_link(link: Option<&Path>) -> Result<()> {
    if let Some(link) = link {
        match fs::remove_file(link) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", link.display())),
        }
    }
    Ok(())
}

fn remove_with_git(cwd: &Path, path: &Path, force: u8, link: Option<&Path>) -> Result<i32> {
    let mut args: Vec<OsString> = vec!["worktree".into(), "remove".into()];
    args.extend((0..force).map(|_| OsString::from("--force")));
    args.extend(["--".into(), path.as_os_str().to_owned()]);
    let status = git::run_inherit(cwd, &args)?.code().unwrap_or(1);
    if status == 0 {
        remove_link(link)?;
    }
    Ok(status)
}

struct ParkedWorktree {
    checkout: PathBuf,
    admin: PathBuf,
}

impl ParkedWorktree {
    fn begin(path: &Path, common: &Path) -> Result<Self> {
        let id = format!(
            "{}-{}-{}",
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        );
        let trash = trash_dir_for(path, common);
        let admin_trash = common.join(TRASH_DIR);
        fs::create_dir_all(&trash)?;
        fs::create_dir_all(&admin_trash)?;
        let checkout = trash.join(&id);
        let parked = Self {
            checkout,
            admin: admin_trash.join(format!("{id}-admin")),
        };
        fs::rename(path, &parked.checkout)?;
        Ok(parked)
    }

    fn recover(path: &Path, admin: &Path, common: &Path) -> Result<Option<Self>> {
        let admin_trash = common.join(TRASH_DIR);
        let local_trash = path.parent().unwrap_or(path).join(format!(".{TRASH_DIR}"));
        let mut parked = None;
        for trash in [&admin_trash, &local_trash] {
            let dirs = match fs::read_dir(trash) {
                Ok(dirs) => dirs,
                Err(e) if e.kind() == ErrorKind::NotFound => continue,
                Err(e) => return Err(e).with_context(|| format!("reading {}", trash.display())),
            };
            for entry in dirs {
                let entry = entry?;
                let checkout = entry.path();
                if !entry.file_type()?.is_dir()
                    || !read_admin_dir(&checkout, path)
                        .is_ok_and(|a| util::canon(&a) == util::canon(admin))
                {
                    continue;
                }
                if parked.is_some() {
                    bail!(
                        "more than one parked checkout belongs to {}",
                        path.display()
                    );
                }
                let mut admin_name = entry.file_name();
                admin_name.push("-admin");
                parked = Some(Self {
                    checkout,
                    admin: admin_trash.join(admin_name),
                });
            }
        }
        Ok(parked)
    }

    fn unregister(&self, admin: &Path) -> Result<()> {
        match fs::rename(admin, &self.admin) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound && !admin.try_exists()? => Ok(()),
            Err(e) => Err(e).with_context(|| format!("unregistering {}", admin.display())),
        }
    }

    fn finish(self, original: &Path, link: Option<&Path>, sync: bool) -> Result<i32> {
        let alias_cleanup = remove_link(link);
        let paths = [&*self.checkout, &*self.admin];
        if sync || !cfg!(unix) || spawn_sweeper(&paths).is_err() {
            for path in paths {
                sweep(path)?;
            }
        }
        alias_cleanup?;
        eprintln!("sharetree: removed worktree {}", original.display());
        Ok(0)
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn unregister(common: &Path, admin: &Path) -> Result<()> {
    validate_admin(common, admin)?;
    let trash = common.join(TRASH_DIR);
    fs::create_dir_all(&trash)?;
    let parked = trash.join(format!("overlay-admin-{}", std::process::id()));
    fs::rename(admin, &parked).with_context(|| format!("unregistering {}", admin.display()))?;
    sweep(&parked)
}

pub fn run(args: &[OsString]) -> Result<i32> {
    let a = parse(args)?;
    if let Some(var) = git::context_override() {
        bail!("sharetree rm requires Git to discover each worktree from its path; unset {var}");
    }
    let cwd = std::env::current_dir()?;
    let requested = util::absolutize(&a.path, &cwd);
    let alias = requested
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink());
    let (target, link) = if alias {
        let target = fs::read_link(&requested)?;
        let parent = requested.parent().unwrap_or(&cwd);
        (
            util::canon(&util::absolutize(&target, parent)),
            Some(requested.as_path()),
        )
    } else {
        (util::canon(&requested), None)
    };
    let common = git::common_dir(&cwd)?;
    let wts = git::list_worktrees(&cwd)?;
    let wt = wts.iter().find(|w| util::canon(&w.path) == target);
    if wt.is_some_and(|w| w.is_main) {
        bail!("refusing to remove the main worktree");
    }
    if util::is_within(&cwd, &target) {
        bail!(
            "refusing to remove {}: the current directory is inside it",
            target.display()
        );
    }
    let marked = wt.is_some_and(|w| crate::overlay::is_overlay_lock(w.lock_reason.as_deref()));
    #[cfg(target_os = "linux")]
    if let Some(code) = crate::overlay::remove(&cwd, &common, &target, marked, a.force)? {
        remove_link(link)?;
        return Ok(code);
    }
    #[cfg(not(target_os = "linux"))]
    if marked {
        bail!("{} is an overlay worktree (Linux only)", target.display());
    }
    let Some(wt) = wt else {
        bail!("{} is not a worktree of this repository", a.path.display());
    };

    let base = crate::overlay::is_base_lock(wt.lock_reason.as_deref());
    if base && a.force < 2 {
        bail!(
            "{} is an overlay base (the lower layer of overlay worktrees); `sharetree rm` of the \
             last overlay using it removes it, or force with `sharetree rm -f -f`",
            wt.path.display()
        );
    }
    #[cfg(target_os = "linux")]
    let _base_lock = if base {
        Some(crate::overlay::lock_unused_base(&wt.path)?)
    } else {
        None
    };
    let ours = volume::is_sharetree_lock(wt.lock_reason.as_deref());
    let seed = volume::is_seed_lock(wt.lock_reason.as_deref());
    if seed && a.force < 2 {
        bail!(
            "{} is the sharetree volume's seed (the donor for worktrees in the volume); \
             `sharetree setup teardown` removes it, or force with `sharetree rm -f -f`",
            wt.path.display()
        );
    }
    let _volume_lock = if ours || seed {
        Some(crate::config::lock_volume_use()?)
    } else {
        None
    };
    if wt.locked && a.force < 2 && !ours {
        bail!(
            "{} is locked; use `sharetree rm -f -f` (or `git worktree unlock`)",
            wt.path.display()
        );
    }

    if !wt.path.try_exists()? {
        if let Some(admin) = registered_admin(&common, &wt.path)? {
            if let Some(parked) = ParkedWorktree::recover(&wt.path, &admin, &common)? {
                parked.unregister(&admin)?;
                return parked.finish(&wt.path, link, a.sync);
            }
        }
        if ours && a.force < 2 {
            bail!(
                "{} is in a sharetree volume that is not mounted; mount it (`sharetree setup mount`) \
                 first, or drop the registration and leave its files with `sharetree rm -f -f`",
                wt.path.display()
            );
        }
        return remove_with_git(&cwd, &wt.path, a.force, link);
    }

    let admin = admin_dir(&wt.path)?;
    validate_admin(&common, &admin)?;
    if a.force == 0 {
        check_clean(&wt.path, &admin)?;
    }

    let parked = match ParkedWorktree::begin(&wt.path, &common) {
        Ok(parked) => parked,
        Err(e) => {
            eprintln!(
                "sharetree: cannot park {} ({e}); using `git worktree remove`",
                wt.path.display()
            );
            let git_force = if ours && wt.locked { 2 } else { a.force };
            return remove_with_git(&cwd, &wt.path, git_force, link);
        }
    };
    if let Err(err) = parked.unregister(&admin) {
        fs::rename(&parked.checkout, &wt.path).with_context(|| {
            format!(
                "{err:#}; could not restore the checkout from {} to {}",
                parked.checkout.display(),
                wt.path.display()
            )
        })?;
        return Err(err);
    }
    parked.finish(&wt.path, link, a.sync)
}

fn trash_dir_for(wt: &Path, common: &Path) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let same = match (fs::metadata(wt), fs::metadata(common)) {
            (Ok(a), Ok(b)) => a.dev() == b.dev(),
            _ => false,
        };
        if same {
            return common.join(TRASH_DIR);
        }
    }
    #[cfg(not(unix))]
    let _ = common;
    wt.parent().unwrap_or(wt).join(format!(".{TRASH_DIR}"))
}

#[cfg(unix)]
fn spawn_sweeper(parked: &[&Path]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let exe = std::env::current_exe()?;
    Command::new(exe)
        .arg("__sweep")
        .args(parked)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .context("spawning background delete")?;
    Ok(())
}

#[cfg(not(unix))]
fn spawn_sweeper(parked: &[&Path]) -> Result<()> {
    for path in parked {
        sweep(path)?;
    }
    Ok(())
}

pub fn sweep_cmd(args: &[OsString]) -> Result<i32> {
    for a in args {
        let p = PathBuf::from(a);
        let in_trash = p
            .parent()
            .and_then(|d| d.file_name())
            .is_some_and(|n| n == TRASH_DIR || n == format!(".{TRASH_DIR}").as_str());
        if !in_trash {
            bail!(
                "refusing to sweep {}: not inside a sharetree trash dir",
                p.display()
            );
        }
        sweep(&p)?;
    }
    Ok(0)
}

fn sweep(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("sweeping {}", path.display())),
    }
    if let Some(parent) = path.parent() {
        let _ = fs::remove_dir(parent);
    }
    Ok(())
}
