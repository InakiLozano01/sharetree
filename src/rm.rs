use std::ffi::OsString;
use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

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
    let dot_git = wt.join(".git");
    let bytes =
        read_git_file(&dot_git).with_context(|| format!("reading {}", dot_git.display()))?;
    let path = git::trim_nl(&bytes)
        .strip_prefix(b"gitdir: ")
        .ok_or_else(|| anyhow!("{}/.git is not a gitdir file", wt.display()))?;
    let path = decode_git_path(path, &dot_git)?;
    if wt != original {
        let mut parts = path.components();
        if parts.find(|part| *part != Component::CurDir) == Some(Component::ParentDir) {
            return Ok(original
                .parent()
                .context("missing original worktree parent")?
                .join(parts.as_path()));
        }
    }
    Ok(original.join(path))
}

fn read_git_file(path: &Path) -> std::io::Result<Vec<u8>> {
    if !path.symlink_metadata()?.is_file() {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!("expected a regular Git metadata file at {}", path.display()),
        ));
    }
    fs::read(path)
}

fn decode_git_path(bytes: &[u8], source: &Path) -> Result<PathBuf> {
    if bytes.is_empty() || bytes.contains(&0) {
        bail!("invalid Git path in {}", source.display());
    }
    #[cfg(windows)]
    std::str::from_utf8(bytes)
        .with_context(|| format!("invalid Git path in {}", source.display()))?;
    Ok(util::bytes_to_path(bytes))
}

pub(crate) fn registered_admin(common: &Path, path: &Path) -> Result<Option<PathBuf>> {
    let root = common.join("worktrees");
    match root.symlink_metadata() {
        Ok(info) if info.is_dir() => {}
        Ok(_) => bail!("unexpected Git worktree directory {}", root.display()),
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("checking {}", root.display())),
    };
    let path = checkout_path(path)?;
    let mut registered = None;
    for ent in fs::read_dir(&root).with_context(|| format!("reading {}", root.display()))? {
        let ent = ent?;
        let kind = ent.file_type()?;
        let admin = ent.path();
        if kind.is_symlink() {
            bail!("unexpected Git worktree directory {}", admin.display());
        }
        if !kind.is_dir() {
            continue;
        }
        let gitdir = admin.join("gitdir");
        let link = match read_git_file(&gitdir) {
            Ok(link) => link,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(e).with_context(|| format!("checking {}/gitdir", admin.display()))
            }
        };
        let target = admin.join(decode_git_path(git::trim_nl(&link), &gitdir)?);
        if target.file_name().is_some_and(|name| name == ".git")
            && checkout_path(target.parent().context("missing checkout path")?)? == path
        {
            validate_admin(common, &admin)?;
            if registered.replace(admin).is_some() {
                bail!(
                    "more than one Git registration belongs to {}",
                    path.display()
                );
            }
        }
    }
    Ok(registered)
}

fn checkout_path(path: &Path) -> Result<PathBuf> {
    match path.canonicalize() {
        Ok(path) => Ok(path),
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
            let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
                return Err(e).with_context(|| format!("resolving {}", path.display()));
            };
            match fs::read_link(path) {
                Ok(target) => checkout_path(&parent.join(target)),
                Err(err)
                    if matches!(err.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) =>
                {
                    Ok(checkout_path(parent)?.join(name))
                }
                Err(err) => Err(err).with_context(|| format!("resolving {}", path.display())),
            }
        }
        Err(e) => Err(e).with_context(|| format!("resolving {}", path.display())),
    }
}

fn validate_admin(common: &Path, admin: &Path) -> Result<()> {
    let root = common.join("worktrees");
    if !root.symlink_metadata()?.is_dir()
        || !admin.symlink_metadata()?.is_dir()
        || admin
            .parent()
            .context("missing Git admin parent")?
            .canonicalize()?
            != root.canonicalize()?
    {
        bail!("unexpected Git worktree directory {}", admin.display());
    }
    let commondir = admin.join("commondir");
    let link =
        read_git_file(&commondir).with_context(|| format!("reading {}", commondir.display()))?;
    let target = admin.join(decode_git_path(git::trim_nl(&link), &commondir)?);
    if target.canonicalize()? != common.canonicalize()? {
        bail!("{} belongs to a different Git repository", admin.display());
    }
    Ok(())
}

pub(crate) fn checkout_admin(common: &Path, path: &Path) -> Result<PathBuf> {
    if !path.symlink_metadata()?.is_dir() {
        bail!("expected a worktree directory at {}", path.display());
    }
    let admin = admin_dir(path)?;
    validate_admin(common, &admin)?;
    if let Some(registered) = registered_admin(common, path)? {
        if admin.canonicalize()? == registered.canonicalize()? {
            reject_mounts(&registered)?;
            reject_submodule_worktrees(&registered)?;
            return Ok(registered);
        }
    }
    bail!("Git registration does not point back to {}", path.display())
}

fn reject_submodule_worktrees(admin: &Path) -> Result<()> {
    let mut dirs = vec![admin.join("modules")];
    while let Some(dir) = dirs.pop() {
        match dir.symlink_metadata() {
            Ok(info) if info.is_dir() => {}
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Ok(_) => bail!("cannot verify Git submodule metadata at {}", dir.display()),
            Err(e) => return Err(e).with_context(|| format!("checking {}", dir.display())),
        }
        if dir
            .join("HEAD")
            .symlink_metadata()
            .is_ok_and(|info| info.is_file())
        {
            let worktrees = dir.join("worktrees");
            match worktrees.symlink_metadata() {
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Ok(info) if info.is_dir() && fs::read_dir(&worktrees)?.next().is_none() => {}
                Ok(_) => bail!("submodule at {} has its own Git worktrees", dir.display()),
                Err(e) => {
                    return Err(e).with_context(|| format!("checking {}", worktrees.display()))
                }
            }
            dirs.push(dir.join("modules"));
        } else {
            for entry in fs::read_dir(&dir)? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    bail!(
                        "cannot verify Git submodule metadata at {}",
                        entry.path().display()
                    );
                }
                dirs.push(entry.path());
            }
        }
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

struct Alias {
    path: PathBuf,
    target: PathBuf,
}

fn remove_link(link: Option<&Alias>) -> Result<()> {
    if let Some(link) = link {
        match fs::read_link(&link.path) {
            Ok(target) if target == link.target => {}
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
            _ => bail!(
                "worktree alias {} changed during removal",
                link.path.display()
            ),
        }
        match fs::remove_file(&link.path) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", link.path.display())),
        }
    }
    Ok(())
}

fn remove_with_git(cwd: &Path, path: &Path, force: u8, link: Option<&Alias>) -> Result<i32> {
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
        let mut parked = Self {
            checkout,
            admin: admin_trash.join(format!("{id}-admin")),
        };
        if let Err(error) = fs::rename(path, &parked.checkout) {
            if error.kind() != ErrorKind::CrossesDevices || trash != admin_trash {
                return Err(error.into());
            }
            let local = local_trash_dir(path);
            fs::create_dir_all(&local)?;
            parked.checkout = local.join(&id);
            fs::rename(path, &parked.checkout)?;
        }
        Ok(parked)
    }

    fn recover(path: &Path, admin: &Path, common: &Path) -> Result<Option<Self>> {
        let admin_trash = common.join(TRASH_DIR);
        let local_trash = local_trash_dir(path);
        let canonical_admin = admin.canonicalize()?;
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
                        .and_then(|candidate| candidate.canonicalize().map_err(Into::into))
                        .is_ok_and(|candidate| candidate == canonical_admin)
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
        reject_mounts(&self.checkout)?;
        reject_mounts(admin)?;
        reject_submodule_worktrees(admin)?;
        fs::rename(admin, &self.admin).with_context(|| format!("unregistering {}", admin.display()))
    }

    fn finish(self, original: &Path, link: Option<&Alias>, sync: bool) -> Result<i32> {
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
    reject_submodule_worktrees(admin)?;
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
    let requested = cwd.join(&a.path).components().collect::<PathBuf>();
    let alias = match requested.symlink_metadata() {
        Ok(info) => info.is_symlink(),
        Err(e) if e.kind() == ErrorKind::NotFound => false,
        Err(e) => return Err(e).with_context(|| format!("checking {}", requested.display())),
    };
    let (target, link) = if alias {
        let target = fs::read_link(&requested)?;
        let parent = requested.parent().unwrap_or(&cwd).canonicalize()?;
        (
            checkout_path(&parent.join(&target))?,
            Some(Alias {
                path: parent.join(requested.file_name().context("missing alias name")?),
                target,
            }),
        )
    } else {
        (checkout_path(&requested)?, None)
    };
    let link = link.as_ref();
    let common = git::common_dir(&cwd)?;
    let wts = git::list_worktrees(&cwd)?;
    let mut matching = Vec::new();
    for w in &wts {
        if checkout_path(&w.path)? == target {
            matching.push(w);
        }
    }
    let mut matches = matching.into_iter();
    let wt = matches.next();
    if matches.next().is_some() {
        bail!(
            "more than one Git registration belongs to {}",
            target.display()
        );
    }
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

    if let Err(e) = wt.path.symlink_metadata() {
        if e.kind() != ErrorKind::NotFound {
            return Err(e).with_context(|| format!("checking {}", wt.path.display()));
        }
        let admin = registered_admin(&common, &wt.path)?.with_context(|| {
            format!("cannot confirm Git registration for {}", wt.path.display())
        })?;
        reject_mounts(&admin)?;
        reject_submodule_worktrees(&admin)?;
        if let Some(parked) = ParkedWorktree::recover(&wt.path, &admin, &common)? {
            parked.unregister(&admin)?;
            return parked.finish(&wt.path, link, a.sync);
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

    reject_mounts(&wt.path)?;
    let admin = checkout_admin(&common, &wt.path)?;
    if a.force == 0 {
        check_clean(&wt.path, &admin)?;
    }

    let parked = match ParkedWorktree::begin(&wt.path, &common) {
        Ok(parked) => parked,
        Err(e) => {
            checkout_admin(&common, &wt.path)?;
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
    local_trash_dir(wt)
}

fn local_trash_dir(wt: &Path) -> PathBuf {
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

fn reject_mounts(path: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let path = match path.canonicalize() {
            Ok(path) => path,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err).with_context(|| format!("resolving {}", path.display())),
        };
        if let Some(mount) = crate::volume::mounts()?
            .into_iter()
            .find(|mount| mount.mountpoint.starts_with(&path))
        {
            bail!(
                "refusing to remove {} while {} is mounted",
                path.display(),
                mount.mountpoint.display()
            );
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = path;
    Ok(())
}

fn sweep(path: &Path) -> Result<()> {
    reject_mounts(path)?;
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
