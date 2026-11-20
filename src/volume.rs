//! sharetree volumes: a copy-on-write filesystem inside a file, for hosts whose
//! own filesystem cannot share blocks (research note §2, §7.3).
//!
//! * Linux: a sparse, user-owned Btrfs (or XFS `reflink=1`) image, attached
//!   with `udisksctl loop-setup` and mounted with `udisksctl mount`. Polkit
//!   allows both for the active desktop session, so no root is needed;
//!   otherwise the user adds a root-once fstab line.
//! * Windows: a ReFS Dev Drive VHDX (see `devdrive.rs`).
//!
//! Worktrees placed in a volume live under `<mount>/<repo-id>/`. A reflink
//! cannot cross filesystems, so each repository gets a **seed**: a detached
//! worktree at `<mount>/<repo-id>/.sharetree-seed`, created once with a plain
//! checkout, locked, and advanced to the target commit before each add. It
//! is the donor for every later worktree in the volume.

// The mountinfo parser and unit/fstab generators are Linux-only at run
// time but stay portable so their tests run everywhere.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::path::{Path, PathBuf};

use crate::config::{MountMethod, Volume, VolumeKind};

pub const SEED_DIR: &str = ".sharetree-seed";
/// Lock reasons sharetree writes. Worktrees in a volume are locked so that
/// `git gc` (worktree prune) keeps them while the volume is unmounted.
pub const VOLUME_LOCK_PREFIX: &str = "sharetree volume";
pub const SEED_LOCK_REASON: &str =
    "sharetree seed: donor checkout for worktrees in this volume; do not edit";

pub fn volume_lock_reason(name: &str) -> String {
    format!(
        "{VOLUME_LOCK_PREFIX} '{name}': kept registered while the volume is unmounted; `sharetree rm` unlocks"
    )
}

pub fn is_sharetree_lock(reason: Option<&str>) -> bool {
    reason.is_some_and(|r| r.starts_with(VOLUME_LOCK_PREFIX))
}

pub fn is_seed_lock(reason: Option<&str>) -> bool {
    reason.is_some_and(|r| r == SEED_LOCK_REASON)
}

/// Stable directory name for a repository inside a volume: the main
/// worktree's name plus a hash of its git common dir.
pub fn repo_id(common_dir: &Path, main_worktree: &Path) -> String {
    use sha2::{Digest, Sha256};
    let base: String = main_worktree
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into())
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let base = base.trim_start_matches('.');
    let h = Sha256::digest(crate::util::path_to_bytes(&crate::util::canon(common_dir)));
    let hex: String = h[..4].iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{hex}", if base.is_empty() { "repo" } else { base })
}

/// `dir/name`, or `dir/name-2`, `dir/name-3`... whichever does not exist.
pub fn unique_child(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if first.symlink_metadata().is_err() {
        return first;
    }
    (2..)
        .map(|i| dir.join(format!("{name}-{i}")))
        .find(|p| p.symlink_metadata().is_err())
        .expect("unbounded")
}

/// Where worktrees go inside a mounted volume.
pub fn worktrees_root(v: &Volume, mountpoint: &Path) -> PathBuf {
    match v.kind {
        VolumeKind::Devdrive => mountpoint.join("sharetree"),
        _ => mountpoint.to_path_buf(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    Mounted {
        mountpoint: PathBuf,
        device: Option<String>,
    },
    /// Loop device attached, filesystem not mounted.
    Attached {
        device: String,
    },
    Detached,
    ImageMissing,
}

impl State {
    pub fn mountpoint(&self) -> Option<&Path> {
        match self {
            State::Mounted { mountpoint, .. } => Some(mountpoint),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// /proc/self/mountinfo (portable parser, used on Linux)

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountEntry {
    pub root: String,
    pub mountpoint: PathBuf,
    pub fstype: String,
    pub source: String,
}

/// Undo mountinfo's octal escapes (`\040` for a space, etc.).
fn unescape(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            let v = (b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0');
            out.push(v);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

pub fn parse_mountinfo(text: &str) -> Vec<MountEntry> {
    let mut v = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split(' ').collect();
        let Some(sep) = f.iter().position(|x| *x == "-") else {
            continue;
        };
        if sep < 6 || f.len() < sep + 3 {
            continue;
        }
        v.push(MountEntry {
            root: String::from_utf8_lossy(&unescape(f[3])).into_owned(),
            mountpoint: crate::util::bytes_to_path(&unescape(f[4])),
            fstype: f[sep + 1].to_string(),
            source: String::from_utf8_lossy(&unescape(f[sep + 2])).into_owned(),
        });
    }
    v
}

// ---------------------------------------------------------------------------
// Generated text (portable, unit-tested)

/// Quote one ExecStart argument for systemd (double quotes; `%` and `$`
/// are specifier / variable characters and must be doubled).
pub fn systemd_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn unit_name(volume: &str) -> String {
    let safe: String = volume
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("sharetree-volume-{safe}.service")
}

/// systemd user unit that attaches and mounts the volume at login. polkit
/// grants udisks loop-setup/mount to a user service through the user's
/// active session (checked with pkcheck from a `systemd-run --user` unit);
/// the restart covers logins where the session is not active yet.
/// `config` is baked in as SHARETREE_CONFIG so the unit reads the same config
/// file as the `sharetree` that installed it.
pub fn systemd_unit(exe: &Path, v: &Volume, config: &Path) -> String {
    let env = format!("SHARETREE_CONFIG={}", config.display());
    let mut env_q = String::from("\"");
    for c in env.chars() {
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
        "# Generated by sharetree {ver}. Remove with `sharetree setup teardown --volume {name}`.\n\
         [Unit]\n\
         Description=sharetree copy-on-write volume '{name}'\n\
         ConditionPathExists={image}\n\
         StartLimitIntervalSec=600\n\
         StartLimitBurst=8\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         Environment={env_q}\n\
         ExecStart={exe} setup mount --volume {qname}\n\
         Restart=on-failure\n\
         RestartSec=15\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        ver = env!("CARGO_PKG_VERSION"),
        name = v.name,
        qname = systemd_quote(&v.name),
        image = v.image.display().to_string().replace('%', "%%"),
        exe = systemd_quote(&exe.display().to_string()),
    )
}

/// Escape a path for /etc/fstab (whitespace and backslash as octal).
pub fn fstab_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            ' ' => out.push_str("\\040"),
            '\t' => out.push_str("\\011"),
            '\n' => out.push_str("\\012"),
            '\\' => out.push_str("\\134"),
            _ => out.push(c),
        }
    }
    out
}

pub fn fstab_line(v: &Volume, mountpoint: &Path) -> String {
    let image = v.image.display().to_string();
    let image_dir = v
        .image
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "/".into());
    let discard = match v.kind {
        VolumeKind::Btrfs => "discard=async",
        _ => "discard",
    };
    format!(
        "{} {} {} loop,nofail,noatime,{discard},x-systemd.requires-mounts-for={} 0 0",
        fstab_escape(&image),
        fstab_escape(&mountpoint.display().to_string()),
        v.kind.as_str(),
        fstab_escape(&image_dir),
    )
}

/// What the user runs once, as root, when udisks is not allowed to mount
/// for them (SSH or headless sessions). sharetree never runs sudo itself.
pub fn root_once_commands(v: &Volume, mountpoint: &Path) -> String {
    let mp = shell_quote(&mountpoint.display().to_string());
    format!(
        "sudo mkdir -p {mp}\n\
         echo {line} | sudo tee -a /etc/fstab\n\
         sudo systemctl daemon-reload\n\
         sudo mount {mp}\n",
        line = shell_quote(&fstab_line(v, mountpoint)),
    )
}

pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=,:@%+".contains(&b))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// mkfs.xfs protofile: the root directory owned by the user (mkfs.xfs(8),
/// "-p protofile", line 3).
pub fn xfs_protofile(uid: u32, gid: u32) -> String {
    format!("sharetree\n0 0\nd--755 {uid} {gid}\n$\n")
}

/// Default image size: 30 % of the free space, capped at 200 GiB, at least
/// 1 GiB (2 GiB for XFS, whose minimum is 300 MiB plus log), rounded down to
/// whole GiB. The image is sparse, so the size is a cap, not a cost.
pub fn default_size(free: u64, kind: VolumeKind) -> u64 {
    const G: u64 = 1 << 30;
    let floor = if kind == VolumeKind::Xfs { 2 * G } else { G };
    let s = (free / 10 * 3).min(200 * G);
    (s / G * G).max(floor)
}

pub fn mount_method_desc(m: MountMethod) -> &'static str {
    match m {
        MountMethod::Udisks => "udisks (no root needed in a desktop session)",
        MountMethod::Fstab => "fstab (mounted by root at boot)",
        MountMethod::Devdrive => "VHDX attached by a logon scheduled task",
    }
}

/// Volume state on any platform. A Dev Drive counts as mounted when its
/// drive root exists.
pub fn current_state(v: &Volume) -> State {
    if v.kind == VolumeKind::Devdrive {
        return match &v.mountpoint {
            Some(m) if m.is_dir() => State::Mounted {
                mountpoint: m.clone(),
                device: None,
            },
            _ if v.image.symlink_metadata().is_err() => State::ImageMissing,
            _ => State::Detached,
        };
    }
    #[cfg(target_os = "linux")]
    {
        linux::state(v)
    }
    #[cfg(not(target_os = "linux"))]
    {
        State::Detached
    }
}

/// Mount the volume if needed (Linux: udisks), returning its mountpoint.
pub fn mount_any(v: &Volume, quiet: bool) -> anyhow::Result<PathBuf> {
    if let State::Mounted { mountpoint, .. } = current_state(v) {
        return Ok(mountpoint);
    }
    if v.kind == VolumeKind::Devdrive {
        anyhow::bail!(
            "the Dev Drive ({}) is not attached; the logon task re-attaches it, or run \
             `schtasks /run /tn {}` from an elevated prompt",
            v.image.display(),
            crate::devdrive::TASK_NAME
        );
    }
    #[cfg(target_os = "linux")]
    {
        linux::ensure_mounted(v, quiet)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = quiet;
        anyhow::bail!("sharetree volume '{}' can only be mounted on Linux", v.name)
    }
}

/// Recognize an existing image by its superblock magic, to re-adopt it.
pub fn image_kind(path: &Path) -> Option<VolumeKind> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let mut b = [0u8; 8];
    // XFS: "XFSB" at offset 0.
    f.read_exact(&mut b[..4]).ok()?;
    if &b[..4] == b"XFSB" {
        return Some(VolumeKind::Xfs);
    }
    // Btrfs: "_BHRfS_M" at 0x10040 (superblock at 64 KiB, magic at +0x40).
    f.seek(SeekFrom::Start(0x10040)).ok()?;
    f.read_exact(&mut b).ok()?;
    (&b == b"_BHRfS_M").then_some(VolumeKind::Btrfs)
}

#[cfg(target_os = "linux")]
pub use self::linux::*;

#[cfg(target_os = "linux")]
mod linux {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use anyhow::{bail, Context, Result};

    use super::{parse_mountinfo, MountEntry, State};
    use crate::config::{MountMethod, Volume, VolumeKind};
    use crate::util::canon;

    fn backing_file(loopdev: &str) -> Option<PathBuf> {
        let name = loopdev.strip_prefix("/dev/")?;
        let s = fs::read_to_string(format!("/sys/block/{name}/loop/backing_file")).ok()?;
        let s = s.trim_end_matches('\n');
        let s = s.strip_suffix(" (deleted)").unwrap_or(s);
        Some(PathBuf::from(s))
    }

    /// Loop devices whose backing file is `image`.
    pub fn loop_devices_for(image: &Path) -> Vec<String> {
        let want = canon(image);
        let mut v: Vec<String> = fs::read_dir("/sys/block")
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.starts_with("loop"))
            .map(|n| format!("/dev/{n}"))
            .filter(|d| backing_file(d).is_some_and(|b| b == want))
            .collect();
        v.sort();
        v
    }

    pub fn mounts() -> Vec<MountEntry> {
        fs::read_to_string("/proc/self/mountinfo")
            .map(|t| parse_mountinfo(&t))
            .unwrap_or_default()
    }

    pub fn state(v: &Volume) -> State {
        if v.mount == MountMethod::Devdrive {
            return State::Detached;
        }
        if v.image.symlink_metadata().is_err() {
            return State::ImageMissing;
        }
        let devs = loop_devices_for(&v.image);
        let all = mounts();
        // Prefer the filesystem root (a Btrfs subvolume mount has root != /).
        let mut hits: Vec<&MountEntry> = all.iter().filter(|m| devs.contains(&m.source)).collect();
        hits.sort_by_key(|m| m.root != "/");
        if let Some(m) = hits.first() {
            return State::Mounted {
                mountpoint: m.mountpoint.clone(),
                device: Some(m.source.clone()),
            };
        }
        match devs.into_iter().next() {
            Some(device) => State::Attached { device },
            None => State::Detached,
        }
    }

    pub fn find_in_path(name: &str) -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|d| d.join(name))
            .find(|p| p.is_file())
    }

    /// polkit's verdict for this process (None: pkcheck is not installed).
    pub fn polkit_allows(action: &str) -> Option<bool> {
        find_in_path("pkcheck")?;
        let st = Command::new("pkcheck")
            .args(["--action-id", action, "--process"])
            .arg(std::process::id().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok()?;
        Some(st.success())
    }

    /// Can this session attach and mount without a password?
    pub fn udisks_usable() -> Result<(), String> {
        if find_in_path("udisksctl").is_none() {
            return Err("udisksctl is not installed (package udisks2)".into());
        }
        for action in [
            "org.freedesktop.udisks2.loop-setup",
            "org.freedesktop.udisks2.filesystem-mount",
        ] {
            if polkit_allows(action) == Some(false) {
                return Err(format!(
                    "polkit does not allow {action} without a password in this session \
                     (SSH and headless sessions are not \"active\")"
                ));
            }
        }
        Ok(())
    }

    fn udisksctl(args: &[&str]) -> Result<String> {
        let out = Command::new("udisksctl")
            .args(args)
            .arg("--no-user-interaction")
            .stdin(Stdio::null())
            .output()
            .context("running udisksctl")?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        if !out.status.success() {
            bail!("udisksctl {}: {}", args.join(" "), text.trim());
        }
        Ok(text)
    }

    /// "Mapped file /x/volume.img as /dev/loop3."
    pub fn parse_loop_setup(out: &str) -> Option<String> {
        let i = out.find(" as /dev/")?;
        let dev: String = out[i + 4..]
            .chars()
            .take_while(|c| !c.is_whitespace())
            .collect();
        Some(dev.trim_end_matches('.').to_string())
    }

    fn wait_mounted(v: &Volume, for_: Duration) -> Option<PathBuf> {
        let t = Instant::now();
        loop {
            if let State::Mounted { mountpoint, .. } = state(v) {
                return Some(mountpoint);
            }
            if t.elapsed() >= for_ {
                return None;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Attach and mount the volume if needed; returns the mountpoint.
    pub fn ensure_mounted(v: &Volume, quiet: bool) -> Result<PathBuf> {
        let dev = match state(v) {
            State::Mounted { mountpoint, .. } => return Ok(mountpoint),
            State::ImageMissing => bail!(
                "the image of sharetree volume '{}' is missing: {}",
                v.name,
                v.image.display()
            ),
            _ if v.mount == MountMethod::Fstab => {
                let mp = v
                    .mountpoint
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                bail!(
                    "sharetree volume '{}' is not mounted; it is mounted by /etc/fstab at boot \
                     (now: `sudo mount {mp}`)",
                    v.name
                )
            }
            State::Attached { device } => device,
            State::Detached => {
                udisks_usable().map_err(|e| {
                    anyhow::anyhow!(
                        "cannot mount sharetree volume '{}': {e}. Run `sharetree setup volume` from a \
                         desktop session, or see `sharetree setup volume --print` for the root-once \
                         fstab alternative",
                        v.name
                    )
                })?;
                let image = v.image.to_string_lossy().into_owned();
                let out = udisksctl(&["loop-setup", "--no-partition-scan", "-f", &image])?;
                let dev = parse_loop_setup(&out)
                    .or_else(|| loop_devices_for(&v.image).into_iter().next())
                    .with_context(|| format!("udisksctl loop-setup said: {}", out.trim()))?;
                if !quiet {
                    eprintln!("sharetree: attached {} as {dev}", v.image.display());
                }
                dev
            }
        };
        // A desktop automounter (udiskie, GNOME, KDE) may mount it for us.
        if let Some(mp) = wait_mounted(v, Duration::from_millis(1500)) {
            return Ok(mp);
        }
        if let Err(e) = udisksctl(&["mount", "-b", &dev]) {
            // Lost a race with an automounter?
            if let Some(mp) = wait_mounted(v, Duration::from_secs(2)) {
                return Ok(mp);
            }
            return Err(e);
        }
        let mp = wait_mounted(v, Duration::from_secs(3))
            .with_context(|| format!("{dev} was mounted but does not appear in mountinfo"))?;
        if !quiet {
            eprintln!("sharetree: mounted volume '{}' at {}", v.name, mp.display());
        }
        Ok(mp)
    }

    /// Unmount and detach (udisks). Returns what happened.
    pub fn unmount(v: &Volume) -> Result<()> {
        loop {
            match state(v) {
                State::Mounted { device, mountpoint } => {
                    let Some(dev) = device else {
                        bail!("{} is mounted from an unknown device", mountpoint.display())
                    };
                    if v.mount == MountMethod::Fstab {
                        bail!(
                            "volume '{}' is mounted through /etc/fstab: run `sudo umount {}` and remove its fstab line",
                            v.name,
                            mountpoint.display()
                        );
                    }
                    udisksctl(&["unmount", "-b", &dev])?;
                }
                State::Attached { device } => {
                    udisksctl(&["loop-delete", "-b", &device])?;
                }
                State::Detached | State::ImageMissing => return Ok(()),
            }
        }
    }

    /// Create a sparse image file; NOCOW on a Btrfs host (VM-image advice:
    /// a CoW image file on CoW Btrfs fragments badly).
    pub fn create_image(path: &Path, size: u64) -> Result<()> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(d) = path.parent() {
            fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
        }
        let f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("creating {}", path.display()))?;
        if crate::linux::fs_magic(path) == Some(crate::linux::BTRFS_SUPER_MAGIC) {
            const FS_IOC_GETFLAGS: u64 = 0x8008_6601;
            const FS_IOC_SETFLAGS: u64 = 0x4008_6602;
            const FS_NOCOW_FL: libc::c_int = 0x0080_0000;
            let mut flags: libc::c_int = 0;
            // SAFETY: valid fd and out-pointer of the size the kernel writes.
            unsafe {
                if libc::ioctl(f.as_raw_fd(), FS_IOC_GETFLAGS as _, &mut flags) == 0 {
                    flags |= FS_NOCOW_FL;
                    libc::ioctl(f.as_raw_fd(), FS_IOC_SETFLAGS as _, &flags);
                }
            }
        }
        f.set_len(size)
            .with_context(|| format!("sizing {}", path.display()))?;
        Ok(())
    }

    /// Format the image with the root directory owned by the user.
    pub fn mkfs(v: &Volume) -> Result<()> {
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        let tmp = std::env::temp_dir().join(format!("sharetree-mkfs-{}", std::process::id()));
        fs::create_dir_all(&tmp)?;
        let res = (|| -> Result<()> {
            let mut c = match v.kind {
                VolumeKind::Btrfs => {
                    let root = tmp.join("root");
                    fs::create_dir_all(&root)?;
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&root, fs::Permissions::from_mode(0o755))?;
                    let mut c = Command::new("mkfs.btrfs");
                    c.args(["-q", "-L", &v.label, "--rootdir"])
                        .arg(&root)
                        .arg(&v.image);
                    c
                }
                VolumeKind::Xfs => {
                    let proto = tmp.join("proto");
                    fs::write(&proto, super::xfs_protofile(uid, gid))?;
                    let mut c = Command::new("mkfs.xfs");
                    c.args(["-q", "-L", &v.label, "-m", "reflink=1", "-p"])
                        .arg(&proto)
                        .arg(&v.image);
                    c
                }
                VolumeKind::Devdrive => bail!("a Dev Drive is created on Windows"),
            };
            let out = c
                .stdin(Stdio::null())
                .output()
                .with_context(|| format!("running mkfs.{}", v.kind.as_str()))?;
            if !out.status.success() {
                bail!(
                    "mkfs.{} failed: {}",
                    v.kind.as_str(),
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            Ok(())
        })();
        let _ = fs::remove_dir_all(&tmp);
        res
    }

    /// Free bytes for an unprivileged user on the filesystem holding `p`.
    pub fn free_bytes(p: &Path) -> Option<u64> {
        let (_, avail) = statvfs(p)?;
        Some(avail)
    }

    /// (total, available) bytes.
    pub fn statvfs(p: &Path) -> Option<(u64, u64)> {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(p.as_os_str().as_bytes()).ok()?;
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: valid C string and out-pointer.
        if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
            return None;
        }
        let f = st.f_frsize as u64;
        Some((st.f_blocks as u64 * f, st.f_bavail as u64 * f))
    }

    /// Btrfs bytes still waiting for async discard to punch holes in the
    /// image (`/sys/fs/btrfs/<fsid>/discard/discardable_bytes`).
    pub fn btrfs_discardable(device: &str) -> Option<u64> {
        let name = device.strip_prefix("/dev/")?;
        for e in fs::read_dir("/sys/fs/btrfs").ok()?.flatten() {
            if e.path().join("devices").join(name).exists() {
                let s = fs::read_to_string(e.path().join("discard/discardable_bytes")).ok()?;
                return s.trim().parse().ok();
            }
        }
        None
    }

    pub fn systemctl_user(args: &[&str]) -> Result<()> {
        let out = Command::new("systemctl")
            .arg("--user")
            .args(args)
            .stdin(Stdio::null())
            .output()
            .context("running systemctl --user")?;
        if !out.status.success() {
            bail!(
                "systemctl --user {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn loop_setup_output() {
            assert_eq!(
                parse_loop_setup("Mapped file /home/u/v.img as /dev/loop3.\n").as_deref(),
                Some("/dev/loop3")
            );
            assert_eq!(parse_loop_setup("Error"), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vol(kind: VolumeKind, image: &str) -> Volume {
        Volume {
            name: "default".into(),
            kind,
            image: PathBuf::from(image),
            label: "sharetree".into(),
            size: 1 << 30,
            mount: MountMethod::Udisks,
            mountpoint: None,
            unit: None,
        }
    }

    #[test]
    fn mountinfo_parsing_and_escapes() {
        let text = "\
36 35 98:0 / /run/media/u/cow\\040tree rw,nosuid,nodev,relatime shared:1 - btrfs /dev/loop3 rw,discard=async\n\
37 35 98:0 /sub /mnt/sub rw - btrfs /dev/loop3 rw\n\
garbage line\n\
40 1 0:5 / /proc rw,nosuid master:2 - proc proc rw\n";
        let m = parse_mountinfo(text);
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].mountpoint, PathBuf::from("/run/media/u/cow tree"));
        assert_eq!(m[0].fstype, "btrfs");
        assert_eq!(m[0].source, "/dev/loop3");
        assert_eq!(m[1].root, "/sub");
        assert_eq!(m[2].fstype, "proc");
    }

    #[test]
    fn repo_ids_are_stable_and_safe() {
        let a = repo_id(
            Path::new("/nonexistent/a/.git"),
            Path::new("/nonexistent/my app"),
        );
        let b = repo_id(
            Path::new("/nonexistent/b/.git"),
            Path::new("/nonexistent/my app"),
        );
        assert!(a.starts_with("my_app-"), "{a}");
        assert_eq!(a.len(), "my_app-".len() + 8);
        assert_ne!(a, b, "different repos with the same name get different ids");
        assert_eq!(
            a,
            repo_id(
                Path::new("/nonexistent/a/.git"),
                Path::new("/nonexistent/my app")
            )
        );
        assert!(repo_id(Path::new("/x/.git"), Path::new("/")).starts_with("repo-"));
    }

    #[test]
    fn unique_child_skips_existing() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(unique_child(d.path(), "wt"), d.path().join("wt"));
        std::fs::create_dir(d.path().join("wt")).unwrap();
        std::fs::create_dir(d.path().join("wt-2")).unwrap();
        assert_eq!(unique_child(d.path(), "wt"), d.path().join("wt-3"));
    }

    #[test]
    fn systemd_unit_quotes_paths() {
        let mut v = vol(VolumeKind::Btrfs, "/home/u/data 100%/volume.img");
        v.name = "my vol".into();
        let u = systemd_unit(
            Path::new("/opt/my tools/sharetree"),
            &v,
            Path::new("/home/u/.config/sharetree 5%/config.toml"),
        );
        assert!(
            u.contains(
                "Environment=\"SHARETREE_CONFIG=/home/u/.config/sharetree 5%%/config.toml\"\n"
            ),
            "{u}"
        );
        assert!(
            u.contains("ExecStart=\"/opt/my tools/sharetree\" setup mount --volume \"my vol\"\n"),
            "{u}"
        );
        assert!(
            u.contains("ConditionPathExists=/home/u/data 100%%/volume.img\n"),
            "{u}"
        );
        assert!(u.contains("Type=oneshot\nRemainAfterExit=yes\n"));
        assert!(u.contains("Restart=on-failure\n"));
        assert!(u.contains("WantedBy=default.target\n"));
        assert_eq!(systemd_quote("a$b%c\"d"), "\"a$$b%%c\\\"d\"");
        assert_eq!(unit_name("my vol"), "sharetree-volume-my_vol.service");
    }

    #[test]
    fn fstab_and_root_once_commands() {
        let v = vol(VolumeKind::Btrfs, "/home/u/my data/volume.img");
        let line = fstab_line(&v, Path::new("/mnt/sharetree-u"));
        assert_eq!(
            line,
            "/home/u/my\\040data/volume.img /mnt/sharetree-u btrfs \
             loop,nofail,noatime,discard=async,x-systemd.requires-mounts-for=/home/u/my\\040data 0 0"
        );
        let x = fstab_line(&vol(VolumeKind::Xfs, "/i.img"), Path::new("/mnt/c"));
        assert!(x.contains(" xfs loop,nofail,noatime,discard,"), "{x}");
        let cmds = root_once_commands(&v, Path::new("/mnt/sharetree-u"));
        assert!(
            cmds.starts_with("sudo mkdir -p /mnt/sharetree-u\n"),
            "{cmds}"
        );
        assert!(cmds.contains("| sudo tee -a /etc/fstab\n"), "{cmds}");
        assert!(cmds.contains("'/home/u/my\\040data/volume.img "), "{cmds}");
        assert!(cmds.ends_with("sudo mount /mnt/sharetree-u\n"));
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn xfs_protofile_sets_root_owner() {
        assert_eq!(
            xfs_protofile(1000, 100),
            "sharetree\n0 0\nd--755 1000 100\n$\n"
        );
    }

    #[test]
    fn default_sizes() {
        const G: u64 = 1 << 30;
        assert_eq!(default_size(100 * G, VolumeKind::Btrfs), 30 * G);
        assert_eq!(default_size(10_000 * G, VolumeKind::Btrfs), 200 * G);
        assert_eq!(default_size(G, VolumeKind::Btrfs), G);
        assert_eq!(default_size(G, VolumeKind::Xfs), 2 * G);
    }

    #[test]
    fn lock_reasons() {
        assert!(is_sharetree_lock(Some(&volume_lock_reason("v"))));
        assert!(!is_sharetree_lock(Some(SEED_LOCK_REASON)));
        assert!(is_seed_lock(Some(SEED_LOCK_REASON)));
        assert!(!is_sharetree_lock(None));
        assert!(!is_sharetree_lock(Some("on a usb disk")));
    }
}
