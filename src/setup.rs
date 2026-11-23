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

    fn finish_empty(self) -> Result<()> {
        if let Some(arg) = self.finish()?.first() {
            bail!("setup: unexpected argument {arg}\n{USAGE}");
        }
        Ok(())
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

pub fn auto_desc(v: Option<bool>) -> &'static str {
    match v {
        Some(true) => "true",
        Some(false) => "false",
        None => "unset (`sharetree add` asks on a terminal)",
    }
}

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
    o.finish_empty()?;
    if name.is_empty()
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
    {
        bail!("--name must contain only letters, digits, hyphens, underscores or dots");
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
    let _lock = (!a.print).then(config::lock_volume_setup).transpose()?;
    let cfg = Config::load()?;
    if let Some(v) = cfg.volume(&a.name).cloned() {
        if a.print {
            println!(
                "volume '{}' already exists: {}",
                v.name,
                volume_state_line(&v)
            );
            return Ok(0);
        }
        let mp = volume::mount_any(&v, false)?;
        Config::update(|cfg| {
            if let Some(vm) = cfg.volume_mut(&a.name) {
                vm.mountpoint = Some(mp.clone());
            }
            if a.auto {
                cfg.auto_volume = Some(true);
            }
            Ok(())
        })?;
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
    let adopt = match image.symlink_metadata() {
        Ok(metadata) if metadata.is_file() => true,
        Ok(_) => bail!("{} is not a regular image file", image.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(e).with_context(|| format!("checking {}", image.display())),
    };
    if let Some(existing) = cfg
        .volumes
        .iter()
        .find(|v| crate::util::canon(&v.image) == crate::util::canon(&image))
    {
        bail!(
            "{} is already configured as volume '{}'",
            image.display(),
            existing.name
        );
    }
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
        let mut end = label.len().min(12);
        while !label.is_char_boundary(end) {
            end -= 1;
        }
        label.truncate(end);
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
    Config::update(|cfg| {
        cfg.volumes.push(v.clone());
        if a.auto {
            cfg.auto_volume = Some(true);
        }
        Ok(())
    })?;
    let cfg_path = config::config_path().unwrap_or_default();

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
            let auto_volume = Config::update(|cfg| {
                if let Some(vm) = cfg.volume_mut(&v.name) {
                    *vm = v.clone();
                }
                Ok(cfg.auto_volume)
            })?;
            println!(
                "sharetree volume '{}' is mounted at {}",
                v.name,
                mp.display()
            );
            println!("config: {}", cfg_path.display());
            println!(
                "next: `sharetree add --into-volume <path>`{}",
                match auto_volume {
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
    if !unit.starts_with("sharetree-volume-")
        || !unit.ends_with(".service")
        || Path::new(unit).file_name().and_then(|name| name.to_str()) != Some(unit)
    {
        eprintln!("sharetree: warning: skipping unexpected login unit {unit:?}");
        return;
    }
    let _ = volume::systemctl_user(&["disable", unit]);
    if let Some(dir) = config::systemd_user_dir() {
        let _ = std::fs::remove_file(dir.join(unit));
    }
    let _ = volume::systemctl_user(&["daemon-reload"]);
}

fn mount(args: &[OsString]) -> Result<i32> {
    let mut o = Opts::new(args)?;
    let name = o.value("--volume")?;
    o.finish_empty()?;
    let _lock = config::lock_volume_use()?;
    let cfg = Config::load()?;
    let v = pick_volume(&cfg, name.as_deref())?.clone();
    let mp = volume::mount_any(&v, false)?;
    if v.mountpoint.as_deref() != Some(mp.as_path()) {
        Config::update(|cfg| {
            if let Some(vm) = cfg.volume_mut(&v.name) {
                vm.mountpoint = Some(mp.clone());
            }
            Ok(())
        })?;
    }
    println!("{}", mp.display());
    Ok(0)
}

fn auto(args: &[OsString]) -> Result<i32> {
    let mut o = Opts::new(args)?;
    let repo = o.flag("--repo");
    let rest = o.finish()?;
    let on = match rest.as_slice() {
        [s] if s == "on" => Some(true),
        [s] if s == "off" => Some(false),
        [s] if s == "ask" || s == "unset" => None,
        _ => bail!("usage: sharetree setup auto on|off|ask [--repo]"),
    };
    let common = repo
        .then(|| git::common_dir(&std::env::current_dir()?))
        .transpose()?;
    let missing_volume = Config::update(|cfg| {
        if let Some(common) = &common {
            cfg.repo_mut(common).auto_volume = on;
            cfg.repos
                .retain(|r| r.volume.is_some() || r.auto_volume.is_some());
        } else {
            cfg.auto_volume = on;
        }
        Ok(cfg.volumes.is_empty())
    })?;
    match (on, common) {
        (None, _) => println!("auto_volume unset: `sharetree add` asks again"),
        (Some(on), Some(common)) => println!(
            "auto_volume = {on} for {} (overrides the global setting)",
            common.display()
        ),
        (Some(on), None) => println!("auto_volume = {on}"),
    }
    if on == Some(true) && missing_volume {
        eprintln!("sharetree: note: no volume is configured yet (`sharetree setup volume`)");
    }
    Ok(0)
}

fn status(args: &[OsString]) -> Result<i32> {
    Opts::new(args)?.finish_empty()?;
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

#[derive(Debug)]
struct VolumeWorktree {
    path: PathBuf,
    common: PathBuf,
    seed: bool,
}

fn is_owned_submodule(path: &Path, admin: &Path, parent_admin: &Path) -> Result<bool> {
    let modules = parent_admin.join("modules");
    if admin == modules || !admin.starts_with(&modules) {
        return Ok(false);
    }
    let superproject = git::output(path, &["rev-parse", "--show-superproject-working-tree"])?;
    let superproject =
        crate::util::bytes_to_path(superproject.strip_suffix(b"\n").unwrap_or(&superproject));
    if !modules.symlink_metadata()?.is_dir()
        || !superproject.is_absolute()
        || crate::rm::admin_dir(&superproject)?.canonicalize()? != parent_admin
        || git::common_dir(path)?.canonicalize()? != admin
        || git::toplevel(path)?.canonicalize()? != path
    {
        bail!("cannot verify submodule ownership at {}", path.display());
    }
    Ok(true)
}

fn seed_is_clean(path: &Path) -> bool {
    git::output(path, &["ls-files", "-v", "-z"]).is_ok_and(|index| {
        index
            .split(|&byte| byte == 0)
            .all(|entry| entry.is_empty() || entry.starts_with(b"H "))
    }) && git::output(
        path,
        &[
            "-c",
            "core.fsmonitor=false",
            "status",
            "--porcelain",
            "-z",
            "--untracked-files=normal",
            "--ignored=matching",
            "--ignore-submodules=none",
        ],
    )
    .is_ok_and(|status| status.is_empty())
}

fn worktrees_in(root: &Path) -> Result<Vec<VolumeWorktree>> {
    use std::fs;
    use std::io::ErrorKind;
    use std::rc::Rc;

    let root = root.canonicalize()?;
    let mut worktrees = Vec::new();
    let seed_lock = Path::new(volume::SEED_DIR).with_extension("lock");
    let mut dirs = vec![(root.clone(), None::<Rc<PathBuf>>)];
    while let Some((path, mut owner_admin)) = dirs.pop() {
        let dot_git = path.join(".git");
        match dot_git.symlink_metadata() {
            Ok(info) if info.is_file() && path != root => {
                let mut admin = crate::rm::admin_dir(&path)?.canonicalize()?;
                let submodule = owner_admin
                    .as_ref()
                    .map(|parent| is_owned_submodule(&path, &admin, parent))
                    .transpose()?
                    .unwrap_or(false);
                if !submodule {
                    let common = git::common_dir(&path)?;
                    admin = crate::rm::checkout_admin(&common, &path)?.canonicalize()?;
                    let wts = git::list_worktrees(&path)?;
                    let mut matches = wts.iter().filter(|w| crate::util::canon(&w.path) == path);
                    let wt = matches
                        .next()
                        .filter(|w| !w.is_main && matches.next().is_none())
                        .with_context(|| {
                            format!("cannot verify worktree registration at {}", path.display())
                        })?;
                    let seed = path.file_name().is_some_and(|n| n == volume::SEED_DIR)
                        && wt.detached
                        && volume::is_seed_lock(wt.lock_reason.as_deref())
                        && matches!(admin.join("modules").symlink_metadata(), Err(e) if e.kind() == ErrorKind::NotFound)
                        && seed_is_clean(&path);
                    worktrees.push(VolumeWorktree {
                        path: path.clone(),
                        common,
                        seed,
                    });
                }
                owner_admin = Some(Rc::new(admin));
            }
            Ok(_) => bail!(
                "cannot verify worktree registration at {}",
                dot_git.display()
            ),
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("checking {}", dot_git.display())),
        }
        for entry in fs::read_dir(&path).with_context(|| format!("reading {}", path.display()))? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                dirs.push((entry.path(), owner_admin.clone()));
            } else if owner_admin.is_none()
                && !(kind.is_file()
                    && entry.file_name() == seed_lock.as_os_str()
                    && entry.metadata()?.len() == 0)
            {
                bail!(
                    "cannot account for {} in the volume",
                    entry.path().display()
                );
            }
        }
    }
    worktrees.sort_by(|a, b| b.path.cmp(&a.path));
    Ok(worktrees)
}

fn forget_volume(name: &str) -> Result<()> {
    Config::update(|cfg| {
        cfg.volumes.retain(|v| v.name != name);
        for r in &mut cfg.repos {
            if r.volume.as_deref() == Some(name) {
                r.volume = None;
            }
        }
        cfg.repos
            .retain(|r| r.volume.is_some() || r.auto_volume.is_some());
        Ok(())
    })
}

fn teardown(args: &[OsString]) -> Result<i32> {
    let mut o = Opts::new(args)?;
    let name = o.value("--volume")?;
    let delete = o.flag("--delete-image");
    let force = o.flag("--force");
    let print = o.flag("--print");
    o.finish_empty()?;
    let _lock = (!print).then(config::lock_volume_setup).transpose()?;
    let cfg = Config::load()?;
    let v = pick_volume(&cfg, name.as_deref())?.clone();

    if v.kind == VolumeKind::Devdrive {
        return teardown_devdrive(&v, delete, force, print);
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
    if delete {
        if let Some(var) = git::context_override() {
            bail!("unset {var} before deleting a volume that may contain worktrees");
        }
    }

    let mounted = match volume::try_current_state(&v)? {
        State::Mounted { ref mountpoint, .. } if v.mount == MountMethod::Fstab => {
            bail!(
                "volume '{}' is managed through /etc/fstab; teardown cannot unmount {}",
                v.name,
                mountpoint.display()
            );
        }
        State::Mounted { mountpoint, .. } => Some(mountpoint),
        State::ImageMissing => None,
        _ if delete && v.mount == MountMethod::Fstab => bail!(
            "cannot inspect the volume for worktrees: '{}' is managed through /etc/fstab",
            v.name
        ),
        _ if delete => {
            Some(volume::mount_any(&v, true).context("cannot inspect the volume for worktrees")?)
        }
        _ => None,
    };
    if let Some(mp) = &mounted {
        let root = volume::worktrees_root(&v, mp);
        #[cfg(target_os = "linux")]
        if delete {
            let root = root.canonicalize()?;
            let State::Mounted {
                mountpoint,
                device: Some(device),
            } = volume::try_current_state(&v)?
            else {
                bail!("cannot confirm the device mounted at {}", root.display());
            };
            let mounts = volume::mounts()?;
            let mut at_root = mounts.iter().filter(|m| m.mountpoint == root);
            if mountpoint != root
                || !at_root
                    .next()
                    .is_some_and(|m| m.source == device && m.root == "/")
                || at_root.next().is_some()
            {
                bail!("cannot inspect the entire volume at {}", root.display());
            }
            if let Some(mount) = mounts
                .iter()
                .find(|m| m.mountpoint != root && m.mountpoint.starts_with(&root))
            {
                bail!(
                    "cannot inspect the volume while {} is mounted inside it",
                    mount.mountpoint.display()
                );
            }
        }
        let wts = match worktrees_in(&root) {
            Ok(wts) => wts,
            Err(e) if delete => return Err(e).context("cannot inspect the volume for worktrees"),
            Err(e) => {
                eprintln!("sharetree: warning: cannot inspect the volume for registered worktrees ({e:#})");
                Vec::new()
            }
        };
        let users: Vec<&PathBuf> = wts.iter().filter(|w| !w.seed).map(|w| &w.path).collect();
        if delete && !users.is_empty() && !force {
            bail!(
                "the volume holds {} worktree(s); remove them with `sharetree rm`, or pass --force to delete them with the image:\n  {}",
                users.len(),
                users.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n  ")
            );
        }
        if delete {
            for wt in &wts {
                crate::rm::checkout_admin(&wt.common, &wt.path)?;
                if wt.seed && !force && !seed_is_clean(&wt.path) {
                    bail!("{} changed while inspecting the volume", wt.path.display());
                }
                let st = std::process::Command::new("git")
                    .arg("--git-dir")
                    .arg(&wt.common)
                    .args(["worktree", "remove", "--force", "--force", "--"])
                    .arg(&wt.path)
                    .status()
                    .context("running git worktree remove")?;
                if !st.success() {
                    bail!("git worktree remove failed for {}", wt.path.display());
                }
            }
        } else if !wts.is_empty() {
            eprintln!(
                "sharetree: keeping {} worktree(s) inside the image; they stay registered (locked) and come back when the image is mounted again",
                wts.len()
            );
        }
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
    forget_volume(&v.name)?;
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
    o.finish_empty()?;
    let mut opts = devdrive_opts(&vhdx, size, letter, &label)?;
    if print {
        print!("{}", devdrive::setup_script(&opts));
        return Ok(0);
    }
    if !cfg!(windows) {
        bail!(
            "`sharetree setup devdrive` runs on Windows; `--print` shows the PowerShell it would run"
        );
    }
    let _lock = config::lock_volume_setup()?;
    let cfg = Config::load()?;
    if cfg.volumes.iter().any(|v| v.kind == VolumeKind::Devdrive) {
        bail!("a Dev Drive is already configured (`sharetree setup status`)");
    }
    let confirmed = run_elevated(&mut opts, "setup-devdrive.ps1", devdrive::setup_script)
        .and_then(|r| devdrive::parse_result(&r))?;
    if confirmed != Some(letter) {
        bail!("the Dev Drive script did not confirm drive {letter}:");
    }
    let volume = Volume {
        name: "devdrive".into(),
        kind: VolumeKind::Devdrive,
        image: PathBuf::from(&opts.vhdx),
        label,
        size,
        mount: MountMethod::Devdrive,
        mountpoint: Some(PathBuf::from(format!("{letter}:\\"))),
        unit: Some(devdrive::TASK_NAME.into()),
    };
    Config::update(|cfg| {
        cfg.volumes.push(volume);
        Ok(())
    })?;
    let p = config::config_path().unwrap_or_default();
    println!("Dev Drive {letter}: is ready; config: {}", p.display());
    println!("next: `sharetree add --into-volume <path>` (add `--windows-clone` to try experimental block cloning on Windows 11 24H2 or later)");
    Ok(0)
}

fn teardown_devdrive(v: &Volume, delete: bool, force: bool, print: bool) -> Result<i32> {
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
    if print {
        print!("{}", devdrive::teardown_script(&opts, delete, force));
        return Ok(0);
    }
    if !cfg!(windows) {
        bail!("a Dev Drive is removed on Windows; `--print` shows the script");
    }
    let r = run_elevated(&mut opts, "teardown-devdrive.ps1", |opts| {
        devdrive::teardown_script(opts, delete, force)
    })?;
    devdrive::parse_result(&r)?;
    forget_volume(&v.name)?;
    println!(
        "Dev Drive removed{}",
        if delete { " and its VHDX deleted" } else { "" }
    );
    Ok(0)
}

fn run_elevated(
    opts: &mut devdrive::Opts,
    file: &str,
    script: impl FnOnce(&devdrive::Opts) -> String,
) -> Result<String> {
    let dir = config::data_dir().context("LOCALAPPDATA is not set")?;
    std::fs::create_dir_all(&dir)?;
    let run = tempfile::tempdir_in(dir)?;
    let path = run.path().join(file);
    let result = run.path().join("result.txt");
    opts.result_file = result
        .to_str()
        .context("result path must be Unicode")?
        .into();
    std::fs::write(&path, format!("\u{feff}{}", script(opts)))?;
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
    std::fs::read_to_string(&result)
        .context("the elevated Dev Drive script did not write its result")
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
        assert!(Opts::new(&os(&["unexpected"]))
            .unwrap()
            .finish_empty()
            .is_err());
    }

    #[test]
    fn teardown_checks_malformed_worktree_registrations() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().join("project/topic");
        std::fs::create_dir_all(&wt).unwrap();
        assert!(worktrees_in(tmp.path()).unwrap().is_empty());
        std::fs::write(wt.join(".git"), "invalid registration").unwrap();
        assert!(worktrees_in(tmp.path()).is_err());
    }

    fn fixture_repo(path: &Path) {
        std::fs::create_dir(path).unwrap();
        git::output(path, &["init", "-q"]).unwrap();
        std::fs::write(path.join("tracked"), b"fixture").unwrap();
        git::output(path, &["add", "tracked"]).unwrap();
        git::output(
            path,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=",
                "commit",
                "-qm",
                "fixture",
            ],
        )
        .unwrap();
    }

    fn fixture_worktree(repo: &Path, checkout: &Path, reason: &str) {
        git::output(
            repo,
            &[
                "worktree",
                "add",
                "--detach",
                "--lock",
                "--reason",
                reason,
                checkout.to_str().unwrap(),
            ],
        )
        .unwrap();
    }

    #[test]
    fn teardown_inventories_root_deep_and_nested_worktrees() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let root = tmp.path().join("volume");
        std::fs::create_dir(&root).unwrap();
        fixture_repo(&repo);

        let seed = root.join("repo/.sharetree-seed");
        let direct = root.join("direct");
        let nested = direct.join("area/nested");
        let named_seed = root.join("custom/deep/.sharetree-seed");
        fixture_worktree(&repo, &seed, volume::SEED_LOCK_REASON);
        fixture_worktree(&repo, &direct, "custom");
        fixture_worktree(&repo, &nested, "custom");
        fixture_worktree(&repo, &named_seed, "custom");
        std::fs::write(seed.with_extension("lock"), []).unwrap();

        let wts = worktrees_in(&root).unwrap();
        assert_eq!(wts.len(), 4);
        assert_eq!(wts.iter().filter(|wt| wt.seed).count(), 1);
        assert!(wts
            .iter()
            .any(|wt| wt.path.ends_with("repo/.sharetree-seed") && wt.seed));
        let index = |path: &Path| wts.iter().position(|wt| wt.path.ends_with(path)).unwrap();
        assert!(index(Path::new("area/nested")) < index(Path::new("direct")));
        assert!(wts
            .iter()
            .any(|wt| wt.path.ends_with("custom/deep/.sharetree-seed") && !wt.seed));

        git::output(&seed, &["update-index", "--assume-unchanged", "tracked"]).unwrap();
        std::fs::write(seed.join("tracked"), b"hidden edits").unwrap();
        assert!(git::output(&seed, &["status", "--porcelain"])
            .unwrap()
            .is_empty());
        assert!(worktrees_in(&root).unwrap().iter().all(|wt| !wt.seed));
        git::output(&seed, &["update-index", "--no-assume-unchanged", "tracked"]).unwrap();
        git::output(&seed, &["checkout", "--", "tracked"]).unwrap();

        std::fs::write(seed.join("local-only"), b"keep").unwrap();
        assert!(worktrees_in(&root).unwrap().iter().all(|wt| !wt.seed));
    }

    #[test]
    fn teardown_accounts_for_owned_submodules_and_rejects_their_linked_worktrees() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let source = tmp.path().join("source");
        let root = tmp.path().join("volume");
        std::fs::create_dir(&root).unwrap();
        fixture_repo(&repo);
        fixture_repo(&source);
        git::output(
            &repo,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                source.to_str().unwrap(),
                "path/to/module",
            ],
        )
        .unwrap();
        git::output(
            &repo,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=",
                "commit",
                "-qm",
                "track submodule",
            ],
        )
        .unwrap();
        let checkout = root.join(volume::SEED_DIR);
        fixture_worktree(&repo, &checkout, volume::SEED_LOCK_REASON);
        git::output(
            &checkout,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
            ],
        )
        .unwrap();

        let worktrees = worktrees_in(&root).unwrap();
        assert_eq!(worktrees.len(), 1);
        assert_eq!(worktrees[0].path, checkout.canonicalize().unwrap());
        assert!(!worktrees[0].seed);

        let module = checkout.join("path/to/module");
        let external = tmp.path().join("external-module-worktree");
        fixture_worktree(&module, &external, "external module worktree");
        let err = worktrees_in(&root).unwrap_err();
        assert!(err.to_string().contains("has its own Git worktrees"));
        git::output(
            &checkout,
            &["submodule", "deinit", "-f", "--", "path/to/module"],
        )
        .unwrap();
        assert!(!module.join(".git").exists());
        let err = worktrees_in(&root).unwrap_err();
        assert!(err.to_string().contains("has its own Git worktrees"));
        assert!(external.join("tracked").is_file());
    }

    #[test]
    fn teardown_refuses_to_inventory_unowned_volume_data() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("unknown");
        std::fs::write(&p, "keep").unwrap();
        let err = worktrees_in(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("cannot account for"));
        assert_eq!(std::fs::read_to_string(p).unwrap(), "keep");
    }

    #[test]
    fn devdrive_print_works_on_any_platform() {
        let o = devdrive_opts(r"D:\vm\cow.vhdx", 64 << 30, 'Q', "sharetree").unwrap();
        assert!(devdrive::setup_script(&o).contains(r"$Vhdx = 'D:\vm\cow.vhdx'"));
        assert!(devdrive_opts(r"D:\vm\cow.vhdx", 1 << 30, 'Q', "sharetree").is_err());
    }
}
