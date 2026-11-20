//! `sharetree setup ...`: one-time creation of a copy-on-write volume for
//! filesystems that cannot reflink (research note §7.3, §7.4).
//!
//! ```text
//! sharetree setup volume   [--fs btrfs|xfs] [--size N] [--image P] [--name N]
//!                          [--label L] [--auto] [--no-unit] [--print]
//! sharetree setup devdrive [--size N] [--letter X] [--vhdx P] [--label L] [--print]
//! sharetree setup status
//! sharetree setup mount    [--volume N]
//! sharetree setup teardown [--volume N] [--delete-image] [--force] [--print]
//! sharetree setup auto     on|off|ask [--repo]
//! ```

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use crate::config::{self, Config, MountMethod, Volume, VolumeKind};
use crate::util::human;
use crate::volume::{self, State};
use crate::{devdrive, git};

pub const USAGE: &str = "\
usage:
  sharetree setup volume   [--fs btrfs|xfs] [--size <N>] [--image <path>] [--name <name>]
                           [--label <label>] [--auto] [--no-unit] [--print]
  sharetree setup devdrive [--size <N>] [--letter <X>] [--vhdx <path>] [--label <label>] [--print]
  sharetree setup status
  sharetree setup mount    [--volume <name>]
  sharetree setup teardown [--volume <name>] [--delete-image] [--force] [--print]
  sharetree setup auto     on|off|ask [--repo]
";

/// Tiny option scanner: `--key value`, `--key=value` and flags.
struct Opts {
    args: Vec<String>,
}

impl Opts {
    fn new(args: &[OsString]) -> Result<Opts> {
        let args = args
            .iter()
            .map(|a| {
                a.to_str()
                    .map(str::to_string)
                    .ok_or_else(|| anyhow!("setup: arguments must be valid UTF-8"))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Opts { args })
    }

    fn flag(&mut self, name: &str) -> bool {
        let before = self.args.len();
        self.args.retain(|a| a != name);
        self.args.len() != before
    }

    fn value(&mut self, name: &str) -> Result<Option<String>> {
        let prefix = format!("{name}=");
        if let Some(i) = self.args.iter().position(|a| a.starts_with(&prefix)) {
            let v = self.args.remove(i)[prefix.len()..].to_string();
            return Ok(Some(v));
        }
        if let Some(i) = self.args.iter().position(|a| a == name) {
            if i + 1 >= self.args.len() {
                bail!("{name} requires a value");
            }
            let v = self.args.remove(i + 1);
            self.args.remove(i);
            return Ok(Some(v));
        }
        Ok(None)
    }

    fn finish(self) -> Result<Vec<String>> {
        if let Some(a) = self.args.iter().find(|a| a.starts_with('-')) {
            bail!("setup: unknown option {a}\n{USAGE}");
        }
        Ok(self.args)
    }
}

pub fn run(args: &[OsString]) -> Result<i32> {
    let Some(sub) = args.first().and_then(|s| s.to_str()) else {
        eprint!("{USAGE}");
        return Ok(2);
    };
    let rest = &args[1..];
    match sub {
        "volume" => setup_volume(rest),
        "devdrive" => setup_devdrive(rest),
        "status" => status(rest),
        "mount" => mount(rest),
        "teardown" => teardown(rest),
        "auto" => auto(rest),
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            Ok(0)
        }
        other => {
            eprintln!("sharetree setup: unknown subcommand '{other}'\n{USAGE}");
            Ok(2)
        }
    }
}

fn pick_volume<'c>(cfg: &'c Config, name: Option<&str>) -> Result<&'c Volume> {
    match name {
        Some(n) => cfg.volume(n).ok_or_else(|| {
            anyhow!("no sharetree volume named '{n}' (see `sharetree setup status`)")
        }),
        None => match cfg.volumes.as_slice() {
            [] => bail!("no sharetree volume is configured (`sharetree setup volume`)"),
            [v] => Ok(v),
            _ => cfg
                .volume("default")
                .ok_or_else(|| anyhow!("several volumes are configured; pass --volume <name>")),
        },
    }
}

/// `auto_volume` as shown to the user.
pub fn auto_desc(v: Option<bool>) -> &'static str {
    match v {
        Some(true) => "true",
        Some(false) => "false",
        None => "unset (`sharetree add` asks on a terminal)",
    }
}

/// One line: where the volume is and whether it is mounted.
pub fn volume_state_line(v: &Volume) -> String {
    match volume::current_state(v) {
        State::Mounted { mountpoint, device } => format!(
            "mounted at {}{}",
            mountpoint.display(),
            device.map(|d| format!(" from {d}")).unwrap_or_default()
        ),
        State::Attached { device } => format!("attached as {device}, not mounted"),
        State::Detached => "not mounted".into(),
        State::ImageMissing => format!("image missing: {}", v.image.display()),
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn user_name() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "user".into())
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// setup volume (Linux)

fn setup_volume(args: &[OsString]) -> Result<i32> {
    let mut o = Opts::new(args)?;
    let print = o.flag("--print");
    let auto = o.flag("--auto");
    let no_unit = o.flag("--no-unit");
    let name = o.value("--name")?.unwrap_or_else(|| "default".into());
    let fs = o.value("--fs")?;
    let size = o
        .value("--size")?
        .map(|s| config::parse_size(&s))
        .transpose()?;
    let image = o.value("--image")?.map(PathBuf::from);
    let label = o.value("--label")?;
    let rest = o.finish()?;
    if !rest.is_empty() {
        bail!("setup volume: unexpected argument {}", rest[0]);
    }
    let kind = match fs.as_deref() {
        None | Some("btrfs") => VolumeKind::Btrfs,
        Some("xfs") => VolumeKind::Xfs,
        Some(other) => bail!("--fs {other}: use btrfs or xfs"),
    };
    if !cfg!(target_os = "linux") {
        bail!(if cfg!(windows) {
            "`sharetree setup volume` is for Linux; on Windows use `sharetree setup devdrive`"
        } else {
            "`sharetree setup volume` is for Linux; APFS already supports clones"
        });
    }
    #[cfg(target_os = "linux")]
    {
        linux_setup_volume(VolumeArgs {
            print,
            auto,
            no_unit,
            name,
            kind,
            size,
            image,
            label,
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (print, auto, no_unit, name, kind, size, image, label);
        unreachable!()
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct VolumeArgs {
    print: bool,
    auto: bool,
    no_unit: bool,
    name: String,
    kind: VolumeKind,
    size: Option<u64>,
    image: Option<PathBuf>,
    label: Option<String>,
}

#[cfg(target_os = "linux")]
fn linux_setup_volume(a: VolumeArgs) -> Result<i32> {
    let mut cfg = Config::load()?;
    if let Some(v) = cfg.volume(&a.name).cloned() {
        // Idempotent: make sure it is mounted, report, done.
        if a.print {
            println!(
                "volume '{}' already exists: {}",
                v.name,
                volume_state_line(&v)
            );
            return Ok(0);
        }
        let mp = volume::mount_any(&v, false)?;
        if let Some(vm) = cfg.volume_mut(&a.name) {
            vm.mountpoint = Some(mp.clone());
        }
        if a.auto {
            cfg.auto_volume = Some(true);
        }
        cfg.save()?;
        println!("sharetree volume '{}' is ready at {}", v.name, mp.display());
        return Ok(0);
    }

    let data = config::data_dir().context("cannot locate a data directory (set HOME)")?;
    let image = a.image.clone().unwrap_or_else(|| {
        data.join(if a.name == "default" {
            "volume.img".to_string()
        } else {
            format!("volume-{}.img", a.name)
        })
    });
    let image = crate::util::absolutize(&image, &std::env::current_dir()?);
    let adopt = image.symlink_metadata().is_ok();
    let kind = if adopt {
        volume::image_kind(&image).ok_or_else(|| {
            anyhow!(
                "{} exists and is not a Btrfs or XFS image; choose another --image",
                image.display()
            )
        })?
    } else {
        a.kind
    };
    let mkfs = format!("mkfs.{}", kind.as_str());
    if !adopt && volume::find_in_path(&mkfs).is_none() {
        bail!(
            "{mkfs} is not installed ({}){}",
            if kind == VolumeKind::Xfs {
                "xfsprogs"
            } else {
                "btrfs-progs"
            },
            if kind == VolumeKind::Xfs {
                "; or use --fs btrfs"
            } else {
                ""
            }
        );
    }
    let size = match (adopt, a.size) {
        (true, _) => std::fs::metadata(&image)?.len(),
        (false, Some(s)) => s,
        (false, None) => {
            let dir = crate::util::nearest_existing(image.parent().unwrap_or(&image));
            volume::default_size(volume::free_bytes(&dir).unwrap_or(0), kind)
        }
    };
    if kind == VolumeKind::Xfs && size < 300 << 20 {
        bail!("an XFS volume needs at least 300 MiB");
    }
    if kind == VolumeKind::Btrfs && size < 128 << 20 {
        bail!("a Btrfs volume needs at least 128 MiB");
    }
    let mut label = a.label.clone().unwrap_or_else(|| "sharetree".into());
    if kind == VolumeKind::Xfs {
        label.truncate(12);
    }
    let udisks = volume::udisks_usable();
    let fstab_mp = PathBuf::from(format!(
        "/mnt/sharetree-{}{}",
        user_name(),
        if a.name == "default" {
            String::new()
        } else {
            format!("-{}", a.name)
        }
    ));
    let mut v = Volume {
        name: a.name.clone(),
        kind,
        image: image.clone(),
        label,
        size,
        mount: if udisks.is_ok() {
            MountMethod::Udisks
        } else {
            MountMethod::Fstab
        },
        mountpoint: if udisks.is_ok() {
            None
        } else {
            Some(fstab_mp.clone())
        },
        unit: None,
    };
    let exe = std::env::current_exe().context("locating the sharetree binary")?;

    if a.print {
        println!("# sharetree setup volume '{}' would:", v.name);
        if adopt {
            println!(
                "# adopt the existing {} image {}",
                kind.as_str(),
                image.display()
            );
        } else {
            println!(
                "# create a sparse {} image (grows as used; NOCOW on a Btrfs host):",
                human(size)
            );
            println!(
                "truncate -s {size} {}",
                volume::shell_quote(&image.display().to_string())
            );
            match kind {
                VolumeKind::Btrfs => println!(
                    "mkfs.btrfs -q -L {} --rootdir <empty dir owned by you> {}",
                    volume::shell_quote(&v.label),
                    volume::shell_quote(&image.display().to_string())
                ),
                _ => println!(
                    "mkfs.xfs -q -L {} -m reflink=1 -p <protofile> {}\n# protofile:\n{}",
                    volume::shell_quote(&v.label),
                    volume::shell_quote(&image.display().to_string()),
                    volume::xfs_protofile(unsafe { libc::getuid() }, unsafe { libc::getgid() })
                        .lines()
                        .map(|l| format!("#   {l}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
            }
        }
        match &udisks {
            Ok(()) => {
                println!("# attach and mount without root (polkit allows it in this session):");
                println!(
                    "udisksctl loop-setup --no-user-interaction --no-partition-scan -f {}",
                    volume::shell_quote(&image.display().to_string())
                );
                println!("udisksctl mount --no-user-interaction -b /dev/loopN");
                if !a.no_unit {
                    println!(
                        "# systemd user unit ~/.config/systemd/user/{} (enabled; re-mounts at login):",
                        volume::unit_name(&v.name)
                    );
                    for l in
                        volume::systemd_unit(&exe, &v, &config::config_path().unwrap_or_default())
                            .lines()
                    {
                        println!("#   {l}");
                    }
                }
            }
            Err(why) => {
                println!("# udisks cannot be used here: {why}");
                println!("# run once, as root, so the volume is mounted at boot:");
                print!("{}", volume::root_once_commands(&v, &fstab_mp));
            }
        }
        println!(
            "# record it in {}",
            config::config_path().unwrap_or_default().display()
        );
        return Ok(0);
    }

    if !adopt {
        volume::create_image(&image, size)?;
        if let Err(e) = volume::mkfs(&v) {
            let _ = std::fs::remove_file(&image);
            return Err(e);
        }
        eprintln!(
            "sharetree: created a {} {} image at {} (sparse: {} allocated now)",
            human(size),
            kind.as_str(),
            image.display(),
            human(allocated(&image))
        );
    } else {
        eprintln!(
            "sharetree: adopting the existing {} image {}",
            kind.as_str(),
            image.display()
        );
    }
    cfg.volumes.push(v.clone());
    if a.auto {
        cfg.auto_volume = Some(true);
    }
    let cfg_path = cfg.save()?;

    match udisks {
        Ok(()) => {
            let mp = volume::mount_any(&v, false)?;
            v.mountpoint = Some(mp.clone());
            if !a.no_unit {
                match install_unit(&exe, &v) {
                    Ok(unit) => {
                        eprintln!("sharetree: installed and enabled the systemd user unit {unit} (re-mounts at login)");
                        v.unit = Some(unit);
                    }
                    Err(e) => eprintln!(
                        "sharetree: warning: could not install the login unit ({e:#}); run `sharetree setup mount` after each login"
                    ),
                }
            }
            if let Some(vm) = cfg.volume_mut(&v.name) {
                *vm = v.clone();
            }
            cfg.save()?;
            println!(
                "sharetree volume '{}' is mounted at {}",
                v.name,
                mp.display()
            );
            println!("config: {}", cfg_path.display());
            println!(
                "next: `sharetree add --into-volume <path>`{}",
                match cfg.auto_volume {
                    Some(true) => " (or plain `sharetree add`: auto_volume is on)",
                    Some(false) => "",
                    None => {
                        "; where reflinks don't work, plain `sharetree add` asks whether to use \
                         the volume (`sharetree setup auto on|off` decides for good)"
                    }
                }
            );
        }
        Err(why) => {
            println!("udisks cannot mount for you here: {why}");
            println!("Run these once as root; the volume is then mounted at every boot:\n");
            print!("{}", volume::root_once_commands(&v, &fstab_mp));
            println!(
                "\nThen check with `sharetree setup status`. Config: {}",
                cfg_path.display()
            );
        }
    }
    Ok(0)
}

#[cfg(unix)]
fn allocated(p: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).map(|m| m.blocks() * 512).unwrap_or(0)
}

#[cfg(not(unix))]
fn allocated(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

#[cfg(target_os = "linux")]
fn install_unit(exe: &Path, v: &Volume) -> Result<String> {
    let dir = config::systemd_user_dir().context("no config directory")?;
    std::fs::create_dir_all(&dir)?;
    let unit = volume::unit_name(&v.name);
    let path = dir.join(&unit);
    let cfg = config::config_path().context("no config path")?;
    std::fs::write(&path, volume::systemd_unit(exe, v, &cfg))
        .with_context(|| format!("writing {}", path.display()))?;
    volume::systemctl_user(&["daemon-reload"])?;
    volume::systemctl_user(&["enable", &unit])?;
    Ok(unit)
}

#[cfg(target_os = "linux")]
fn remove_unit(unit: &str) {
    let _ = volume::systemctl_user(&["disable", unit]);
    if let Some(dir) = config::systemd_user_dir() {
        let _ = std::fs::remove_file(dir.join(unit));
    }
    let _ = volume::systemctl_user(&["daemon-reload"]);
}

// ---------------------------------------------------------------------------
// setup mount / status / auto

fn mount(args: &[OsString]) -> Result<i32> {
    let mut o = Opts::new(args)?;
    let name = o.value("--volume")?;
    o.finish()?;
    let mut cfg = Config::load()?;
    let v = pick_volume(&cfg, name.as_deref())?.clone();
    let mp = volume::mount_any(&v, false)?;
    if v.mountpoint.as_deref() != Some(mp.as_path()) {
        if let Some(vm) = cfg.volume_mut(&v.name) {
            vm.mountpoint = Some(mp.clone());
        }
        cfg.save()?;
    }
    println!("{}", mp.display());
    Ok(0)
}

fn auto(args: &[OsString]) -> Result<i32> {
    let mut o = Opts::new(args)?;
    let repo = o.flag("--repo");
    let rest = o.finish()?;
    let on = match rest.as_slice() {
        [s] if s == "on" => true,
        [s] if s == "off" => false,
        [s] if s == "ask" || s == "unset" => {
            let mut cfg = Config::load()?;
            if repo {
                let common = git::common_dir(&std::env::current_dir()?)?;
                cfg.repo_mut(&common).auto_volume = None;
                cfg.repos
                    .retain(|r| r.volume.is_some() || r.auto_volume.is_some());
            } else {
                cfg.auto_volume = None;
            }
            cfg.save()?;
            println!("auto_volume unset: `sharetree add` asks again");
            return Ok(0);
        }
        _ => bail!("usage: sharetree setup auto on|off|ask [--repo]"),
    };
    let mut cfg = Config::load()?;
    if repo {
        let common = git::common_dir(&std::env::current_dir()?)?;
        cfg.repo_mut(&common).auto_volume = Some(on);
        println!(
            "auto_volume = {on} for {} (overrides the global setting)",
            common.display()
        );
    } else {
        cfg.auto_volume = Some(on);
        println!("auto_volume = {on}");
    }
    cfg.save()?;
    if on && cfg.volumes.is_empty() {
        eprintln!("sharetree: note: no volume is configured yet (`sharetree setup volume`)");
    }
    Ok(0)
}

fn status(args: &[OsString]) -> Result<i32> {
    Opts::new(args)?.finish()?;
    let cfg = Config::load()?;
    println!(
        "config       {}",
        config::config_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(none)".into())
    );
    println!("auto_volume  {}", auto_desc(cfg.auto_volume));
    for r in cfg.repos.iter().filter(|r| r.auto_volume.is_some()) {
        println!(
            "             {} for {}",
            auto_desc(r.auto_volume),
            r.common_dir.display()
        );
    }
    #[cfg(target_os = "linux")]
    println!(
        "udisks       {}",
        match volume::udisks_usable() {
            Ok(()) => "usable without a password in this session".to_string(),
            Err(e) => e,
        }
    );
    if cfg.volumes.is_empty() {
        println!("volumes      none (`sharetree setup volume` on Linux, `sharetree setup devdrive` on Windows)");
    }
    for v in &cfg.volumes {
        println!();
        println!("volume '{}'", v.name);
        println!("  kind       {}", v.kind.as_str());
        println!("  image      {}", v.image.display());
        if let Ok(m) = std::fs::metadata(&v.image) {
            println!(
                "  size       {} cap, {} allocated on the host",
                human(m.len()),
                human(allocated(&v.image))
            );
        }
        println!("  mount      {}", volume::mount_method_desc(v.mount));
        let st = volume::current_state(v);
        println!("  state      {}", volume_state_line(v));
        if let State::Mounted { mountpoint, device } = &st {
            #[cfg(target_os = "linux")]
            {
                if let Some((total, avail)) = volume::statvfs(mountpoint) {
                    println!(
                        "  usage      {} used of {} ({} free)",
                        human(total.saturating_sub(avail)),
                        human(total),
                        human(avail)
                    );
                }
                if let Some(d) = device.as_deref().and_then(volume::btrfs_discardable) {
                    println!(
                        "  discard    {} waiting to be returned to the host (async)",
                        human(d)
                    );
                }
            }
            #[cfg(not(target_os = "linux"))]
            let _ = device;
            let root = volume::worktrees_root(v, mountpoint);
            for e in std::fs::read_dir(&root).into_iter().flatten().flatten() {
                let seed = e.path().join(volume::SEED_DIR);
                if seed.join(".git").exists() {
                    let n = std::fs::read_dir(e.path())
                        .map(|r| {
                            r.flatten()
                                .filter(|x| x.path().join(".git").is_file())
                                .count()
                        })
                        .unwrap_or(0);
                    println!(
                        "  repo       {}: seed + {} worktree(s)",
                        e.file_name().to_string_lossy(),
                        n.saturating_sub(1)
                    );
                }
            }
        }
        match (&v.unit, v.mount) {
            (Some(u), _) => println!("  login unit {u}"),
            (None, MountMethod::Udisks) => {
                println!("  login unit none: run `sharetree setup mount` after logging in")
            }
            _ => {}
        }
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// teardown

/// Worktrees inside a mounted volume: (path, git common dir, is_seed).
fn worktrees_in(root: &Path) -> Vec<(PathBuf, PathBuf, bool)> {
    let mut v = Vec::new();
    for repo in std::fs::read_dir(root).into_iter().flatten().flatten() {
        for wt in std::fs::read_dir(repo.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            let p = wt.path();
            if !p.join(".git").is_file() {
                continue;
            }
            let Ok(admin) = crate::rm::admin_dir(&p) else {
                continue;
            };
            let Some(common) = admin.parent().and_then(Path::parent) else {
                continue;
            };
            let seed = p.file_name().is_some_and(|n| n == volume::SEED_DIR);
            v.push((p, common.to_path_buf(), seed));
        }
    }
    v.sort();
    v
}

fn teardown(args: &[OsString]) -> Result<i32> {
    let mut o = Opts::new(args)?;
    let name = o.value("--volume")?;
    let delete = o.flag("--delete-image");
    let force = o.flag("--force");
    let print = o.flag("--print");
    o.finish()?;
    let mut cfg = Config::load()?;
    let v = pick_volume(&cfg, name.as_deref())?.clone();

    if v.kind == VolumeKind::Devdrive {
        return teardown_devdrive(&mut cfg, &v, delete, print);
    }
    if print {
        println!("# sharetree setup teardown --volume {} would:", v.name);
        if delete {
            println!("# remove the seed worktrees in the volume (refuses other worktrees without --force)");
        }
        println!(
            "# unmount and detach: udisksctl unmount -b <dev>; udisksctl loop-delete -b <dev>"
        );
        if let Some(u) = &v.unit {
            println!("systemctl --user disable {u}; rm ~/.config/systemd/user/{u}");
        }
        if delete {
            println!("rm {}", volume::shell_quote(&v.image.display().to_string()));
        }
        println!("# drop volume '{}' from the config", v.name);
        return Ok(0);
    }

    // Worktrees inside: seeds are sharetree's; other ones are the user's.
    let mounted = match volume::current_state(&v) {
        State::Mounted { mountpoint, .. } => Some(mountpoint),
        _ if delete => volume::mount_any(&v, true).ok(),
        _ => None,
    };
    if let Some(mp) = &mounted {
        let wts = worktrees_in(&volume::worktrees_root(&v, mp));
        let users: Vec<&PathBuf> = wts.iter().filter(|w| !w.2).map(|w| &w.0).collect();
        if delete && !users.is_empty() && !force {
            bail!(
                "the volume holds {} worktree(s); remove them with `sharetree rm`, or pass --force to delete them with the image:\n  {}",
                users.len(),
                users.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n  ")
            );
        }
        if delete {
            for (p, common, _) in &wts {
                let st = std::process::Command::new("git")
                    .arg("--git-dir")
                    .arg(common)
                    .args(["worktree", "remove", "--force", "--force"])
                    .arg(p)
                    .status()
                    .context("running git worktree remove")?;
                if !st.success() {
                    bail!("git worktree remove failed for {}", p.display());
                }
            }
        } else if !wts.is_empty() {
            eprintln!(
                "sharetree: keeping {} worktree(s) inside the image; they stay registered (locked) and come back when the image is mounted again",
                wts.len()
            );
        }
    } else if delete && !force {
        bail!("cannot mount the volume to check for worktrees inside it; pass --force to delete the image anyway");
    }

    #[cfg(target_os = "linux")]
    {
        volume::unmount(&v)?;
        if let Some(u) = &v.unit {
            remove_unit(u);
        }
    }
    if delete {
        match std::fs::remove_file(&v.image) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("deleting {}", v.image.display())),
        }
    }
    cfg.volumes.retain(|x| x.name != v.name);
    for r in &mut cfg.repos {
        if r.volume.as_deref() == Some(v.name.as_str()) {
            r.volume = None;
        }
    }
    cfg.repos
        .retain(|r| r.volume.is_some() || r.auto_volume.is_some());
    cfg.save()?;
    println!(
        "sharetree volume '{}' removed{}",
        v.name,
        if delete {
            " and its image deleted".to_string()
        } else {
            format!(
                "; the image is kept at {} (`sharetree setup volume --image <it>` adopts it again)",
                v.image.display()
            )
        }
    );
    Ok(0)
}

// ---------------------------------------------------------------------------
// Windows Dev Drive

fn devdrive_opts(vhdx: &str, size: u64, letter: char, label: &str) -> Result<devdrive::Opts> {
    let data = config::data_dir()
        .map(|d| d.display().to_string())
        .unwrap_or_else(|| r"%LOCALAPPDATA%\sharetree".into());
    let user = match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
        (Ok(d), Ok(u)) => format!("{d}\\{u}"),
        _ => r"<DOMAIN>\<user>".into(),
    };
    let o = devdrive::Opts {
        vhdx: vhdx.to_string(),
        size_bytes: size,
        letter,
        label: label.to_string(),
        user,
        result_file: format!(r"{data}\devdrive-result.txt"),
    };
    o.validate()?;
    Ok(o)
}

fn setup_devdrive(args: &[OsString]) -> Result<i32> {
    let mut o = Opts::new(args)?;
    let print = o.flag("--print");
    let size = o
        .value("--size")?
        .map(|s| config::parse_size(&s))
        .transpose()?
        .unwrap_or(100 << 30);
    let letter = match o.value("--letter")? {
        Some(l) => {
            let l = l.trim_end_matches(':');
            let mut cs = l.chars();
            match (cs.next(), cs.next()) {
                (Some(c), None) => c.to_ascii_uppercase(),
                _ => bail!("--letter takes one drive letter, e.g. Q"),
            }
        }
        None => 'Q',
    };
    let label = o.value("--label")?.unwrap_or_else(|| "sharetree".into());
    let vhdx = match o.value("--vhdx")? {
        Some(p) => p,
        None => config::data_dir()
            .map(|d| d.join("devdrive.vhdx").display().to_string())
            .unwrap_or_else(|| r"%LOCALAPPDATA%\sharetree\devdrive.vhdx".into()),
    };
    o.finish()?;
    let opts = devdrive_opts(&vhdx, size, letter, &label)?;
    let script = devdrive::setup_script(&opts);
    if print {
        print!("{script}");
        return Ok(0);
    }
    if !cfg!(windows) {
        bail!(
            "`sharetree setup devdrive` runs on Windows; `--print` shows the PowerShell it would run"
        );
    }
    let mut cfg = Config::load()?;
    if cfg.volumes.iter().any(|v| v.kind == VolumeKind::Devdrive) {
        bail!("a Dev Drive is already configured (`sharetree setup status`)");
    }
    run_elevated(&script, "setup-devdrive.ps1", &opts.result_file)
        .and_then(|r| devdrive::parse_result(&r))?;
    cfg.volumes.push(Volume {
        name: "devdrive".into(),
        kind: VolumeKind::Devdrive,
        image: PathBuf::from(&opts.vhdx),
        label,
        size,
        mount: MountMethod::Devdrive,
        mountpoint: Some(PathBuf::from(format!("{letter}:\\"))),
        unit: Some(devdrive::TASK_NAME.into()),
    });
    let p = cfg.save()?;
    println!("Dev Drive {letter}: is ready; config: {}", p.display());
    println!("next: `sharetree add --into-volume <path>` (sharetree does not block-clone on Windows yet: worktrees there are full checkouts, with Dev Drive's faster I/O)");
    Ok(0)
}

fn teardown_devdrive(cfg: &mut Config, v: &Volume, delete: bool, print: bool) -> Result<i32> {
    let letter = v
        .mountpoint
        .as_ref()
        .and_then(|m| m.to_str())
        .and_then(|s| s.chars().next())
        .unwrap_or('Q');
    let mut opts = devdrive_opts(
        &v.image.display().to_string(),
        devdrive::MIN_SIZE_BYTES,
        letter,
        &v.label,
    )?;
    opts.size_bytes = v.size.max(devdrive::MIN_SIZE_BYTES);
    let script = devdrive::teardown_script(&opts, delete);
    if print {
        print!("{script}");
        return Ok(0);
    }
    if !cfg!(windows) {
        bail!("a Dev Drive is removed on Windows; `--print` shows the script");
    }
    let r = run_elevated(&script, "teardown-devdrive.ps1", &opts.result_file)?;
    devdrive::parse_result(&r)?;
    cfg.volumes.retain(|x| x.name != v.name);
    cfg.save()?;
    println!(
        "Dev Drive removed{}",
        if delete { " and its VHDX deleted" } else { "" }
    );
    Ok(0)
}

/// Write `script` next to the config data and run it elevated (one UAC
/// prompt), returning what it wrote to its result file.
fn run_elevated(script: &str, file: &str, result_file: &str) -> Result<String> {
    let dir = config::data_dir().context("LOCALAPPDATA is not set")?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(file);
    // A UTF-8 BOM makes Windows PowerShell 5.1 read the file as UTF-8.
    std::fs::write(&path, format!("\u{feff}{script}"))?;
    let _ = std::fs::remove_file(result_file);
    eprintln!(
        "sharetree: running {} elevated (review it with --print)",
        path.display()
    );
    let st = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command"])
        .arg(devdrive::elevate_command(&path.display().to_string()))
        .status()
        .context("running powershell.exe")?;
    if !st.success() {
        bail!("powershell exited with {st} (UAC declined?)");
    }
    Ok(std::fs::read_to_string(result_file).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn option_scanner() {
        let mut o = Opts::new(&os(&["--size", "2G", "--print", "--fs=xfs", "pos"])).unwrap();
        assert!(o.flag("--print"));
        assert!(!o.flag("--auto"));
        assert_eq!(o.value("--size").unwrap().as_deref(), Some("2G"));
        assert_eq!(o.value("--fs").unwrap().as_deref(), Some("xfs"));
        assert_eq!(o.value("--image").unwrap(), None);
        assert_eq!(o.finish().unwrap(), vec!["pos".to_string()]);
        let o = Opts::new(&os(&["--bogus"])).unwrap();
        assert!(o.finish().is_err());
        let mut o = Opts::new(&os(&["--size"])).unwrap();
        assert!(o.value("--size").is_err());
    }

    #[test]
    fn devdrive_print_works_on_any_platform() {
        let o = devdrive_opts(r"D:\vm\cow.vhdx", 64 << 30, 'Q', "sharetree").unwrap();
        assert!(devdrive::setup_script(&o).contains(r"$Vhdx = 'D:\vm\cow.vhdx'"));
        assert!(devdrive_opts(r"D:\vm\cow.vhdx", 1 << 30, 'Q', "sharetree").is_err());
    }
}
