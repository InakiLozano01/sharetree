//! `sharetree rm`: remove a worktree with `git worktree remove`'s safety checks,
//! then rename it into a trash directory and delete it in the background
//! (lane's park-then-sweep design, design §8.5).

use std::ffi::OsString;
use std::fs;
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
    let mut force = 0;
    let mut sync = false;
    let mut path = None;
    for a in args {
        match a.to_str() {
            Some("-f" | "--force") => force += 1,
            Some("-ff") => force += 2,
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

/// `<worktree>/.git` is a file "gitdir: <admin dir>".
pub(crate) fn admin_dir(wt: &Path) -> Result<PathBuf> {
    let text = fs::read_to_string(wt.join(".git"))
        .with_context(|| format!("{} is not a linked worktree", wt.display()))?;
    let p = text
        .trim_end()
        .strip_prefix("gitdir: ")
        .ok_or_else(|| anyhow!("{}/.git is not a gitdir file", wt.display()))?;
    let p = PathBuf::from(p);
    Ok(if p.is_absolute() {
        p
    } else {
        util::absolutize(&p, wt)
    })
}

pub fn run(args: &[OsString]) -> Result<i32> {
    let a = parse(args)?;
    let cwd = std::env::current_dir()?;
    let requested = util::absolutize(&a.path, &cwd);
    let target = util::canon(&requested);
    // `sharetree add --into-volume` leaves a symlink where the user asked for
    // the worktree; remove it with the worktree.
    let link = requested
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
        .then(|| requested.clone());
    let wts = git::list_worktrees(&cwd)?;
    let Some(wt) = wts.iter().find(|w| util::canon(&w.path) == target) else {
        bail!("{} is not a worktree of this repository", a.path.display());
    };
    if wt.is_main {
        bail!("refusing to remove the main worktree");
    }
    if util::is_within(&cwd, &wt.path) {
        bail!(
            "refusing to remove {}: the current directory is inside it",
            wt.path.display()
        );
    }
    if crate::overlay::is_base_lock(wt.lock_reason.as_deref()) && a.force < 2 {
        bail!(
            "{} is an overlay base (the lower layer of overlay worktrees); `sharetree rm` of the \
             last overlay using it removes it, or force with `sharetree rm -f -f`",
            wt.path.display()
        );
    }
    if crate::overlay::is_overlay_lock(wt.lock_reason.as_deref()) {
        #[cfg(target_os = "linux")]
        return crate::overlay::remove(&cwd, &wt.path, a.force);
        #[cfg(not(target_os = "linux"))]
        bail!("{} is an overlay worktree (Linux only)", wt.path.display());
    }
    let ours = volume::is_sharetree_lock(wt.lock_reason.as_deref());
    if volume::is_seed_lock(wt.lock_reason.as_deref()) && a.force < 2 {
        bail!(
            "{} is the sharetree volume's seed (the donor for worktrees in the volume); \
             `sharetree setup teardown` removes it, or force with `sharetree rm -f -f`",
            wt.path.display()
        );
    }
    if wt.locked && a.force < 2 && !ours {
        bail!(
            "{} is locked; use `sharetree rm -f -f` (or `git worktree unlock`)",
            wt.path.display()
        );
    }
    let common = git::common_dir(&cwd)?;
    if !wt.path.exists() && ours && a.force < 2 {
        bail!(
            "{} is in a sharetree volume that is not mounted; mount it (`sharetree setup mount`) \
             first, or drop the registration and leave its files with `sharetree rm -f -f`",
            wt.path.display()
        );
    }
    if !wt.path.exists() {
        // Nothing to trash; let git drop the registration.
        let mut args: Vec<OsString> = vec!["worktree".into(), "remove".into()];
        args.extend((0..a.force).map(|_| OsString::from("--force")));
        args.push(wt.path.clone().into_os_string());
        return Ok(git::run_inherit(&cwd, &args)?.code().unwrap_or(1));
    }
    let admin = admin_dir(&wt.path)?;
    if util::canon(admin.parent().unwrap_or(&admin)) != util::canon(&common.join("worktrees")) {
        bail!(
            "unexpected git dir {} for {}",
            admin.display(),
            wt.path.display()
        );
    }
    if a.force == 0 {
        let st = git::output(
            &wt.path,
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
                wt.path.display()
            );
        }
        if admin.join("modules").exists() {
            bail!(
                "{} contains initialized submodules, use --force to delete it",
                wt.path.display()
            );
        }
    }

    // Park the tree on the same filesystem (a rename is instant), then drop
    // git's registration exactly as `git worktree remove` does.
    let trash = trash_dir_for(&wt.path, &common);
    fs::create_dir_all(&trash)?;
    let name = format!(
        "{}-{}-{}",
        wt.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    );
    let parked = trash.join(name);
    if let Err(e) = fs::rename(&wt.path, &parked) {
        // Cross-device or similar: let git delete it synchronously.
        eprintln!(
            "sharetree: cannot park {} ({e}); using `git worktree remove`",
            wt.path.display()
        );
        let mut args: Vec<OsString> = vec!["worktree".into(), "remove".into()];
        args.extend((0..a.force).map(|_| OsString::from("--force")));
        args.push(wt.path.clone().into_os_string());
        return Ok(git::run_inherit(&cwd, &args)?.code().unwrap_or(1));
    }
    fs::remove_dir_all(&admin).with_context(|| format!("removing {}", admin.display()))?;
    eprintln!("sharetree: removed worktree {}", wt.path.display());
    if let Some(l) = &link {
        let _ = fs::remove_file(l);
    }

    if a.sync || cfg!(not(unix)) {
        fs::remove_dir_all(&parked)?;
    } else {
        spawn_sweeper(&parked)?;
    }
    Ok(0)
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
fn spawn_sweeper(parked: &Path) -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let exe = std::env::current_exe()?;
    Command::new(exe)
        .arg("__sweep")
        .arg(parked)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0) // survive the terminal's Ctrl-C
        .spawn()
        .context("spawning background delete")?;
    Ok(())
}

#[cfg(not(unix))]
fn spawn_sweeper(parked: &Path) -> Result<()> {
    fs::remove_dir_all(parked)?;
    Ok(())
}

/// `sharetree __sweep <parked dir>`: background deletion. Only ever deletes a
/// path inside a sharetree trash directory.
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
        fs::remove_dir_all(&p).ok();
        if let Some(parent) = p.parent() {
            let _ = fs::remove_dir(parent); // only if empty
        }
    }
    Ok(0)
}
