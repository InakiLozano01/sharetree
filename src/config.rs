//! User configuration: `$XDG_CONFIG_HOME/sharetree/config.toml` (Windows:
//! `%APPDATA%\sharetree\config.toml`; `SHARETREE_CONFIG` overrides the path).
//!
//! ```toml
//! auto_volume = false          # place worktrees in the volume when the
//!                              # destination cannot reflink; unset: ask
//!
//! [[volume]]
//! name = "default"
//! kind = "btrfs"               # btrfs | xfs | devdrive
//! image = "/home/me/.local/share/sharetree/volume.img"
//! label = "sharetree"
//! size = 107374182400
//! mount = "udisks"             # udisks | fstab | devdrive
//! mountpoint = "/run/media/me/sharetree"   # last seen (fstab: fixed)
//! unit = "sharetree-volume-default.service"
//!
//! windows_clone = false        # Windows: ReFS block cloning (experimental)
//!
//! [[repo]]
//! common_dir = "/home/me/src/app/.git"
//! volume = "default"           # optional; default: the only volume
//! auto_volume = true           # optional; overrides the global flag
//!
//! [[overlay]]                  # written by `sharetree add --overlay`
//! path = "/home/me/src/app-fix"
//! common_dir = "/home/me/src/app/.git"
//! lower = "/home/me/.local/share/sharetree/overlay/app-1a2b3c4d/bases/<commit>"
//! upper = "/home/me/.local/share/sharetree/overlay/app-1a2b3c4d/wt/app-fix/upper"
//! work = "/home/me/.local/share/sharetree/overlay/app-1a2b3c4d/wt/app-fix/work"
//! ```

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VolumeKind {
    Btrfs,
    Xfs,
    Devdrive,
}

impl VolumeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            VolumeKind::Btrfs => "btrfs",
            VolumeKind::Xfs => "xfs",
            VolumeKind::Devdrive => "devdrive",
        }
    }
}

/// How the volume gets attached and mounted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MountMethod {
    /// `udisksctl loop-setup` + `udisksctl mount`, no root (polkit).
    Udisks,
    /// A root-owned fstab line the user added once.
    Fstab,
    /// Windows: VHDX attached by a scheduled task.
    Devdrive,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Volume {
    pub name: String,
    pub kind: VolumeKind,
    pub image: PathBuf,
    pub label: String,
    #[serde(default)]
    pub size: u64,
    pub mount: MountMethod,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mountpoint: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RepoConfig {
    pub common_dir: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_volume: Option<bool>,
}

/// An overlay worktree (`sharetree add --overlay`), remounted by
/// `sharetree mount --all`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OverlayEntry {
    /// Where the user sees the worktree (the mountpoint).
    pub path: PathBuf,
    pub common_dir: PathBuf,
    /// The base checkout (lower layer).
    pub lower: PathBuf,
    pub upper: PathBuf,
    pub work: PathBuf,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// None: not decided yet (`add` asks on a terminal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_volume: Option<bool>,
    /// Windows: block-clone on ReFS / Dev Drive (experimental, untested).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub windows_clone: Option<bool>,
    #[serde(default, rename = "volume", skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<Volume>,
    #[serde(default, rename = "repo", skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<RepoConfig>,
    #[serde(default, rename = "overlay", skip_serializing_if = "Vec::is_empty")]
    pub overlays: Vec<OverlayEntry>,
}

fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

fn home() -> Option<PathBuf> {
    env_path("HOME").or_else(|| env_path("USERPROFILE"))
}

/// Directory holding `config.toml` (and, on Linux, nothing else).
pub fn config_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        return env_path("APPDATA").map(|p| p.join("sharetree"));
    }
    env_path("XDG_CONFIG_HOME")
        .or_else(|| home().map(|h| h.join(".config")))
        .map(|p| p.join("sharetree"))
}

pub fn config_path() -> Option<PathBuf> {
    env_path("SHARETREE_CONFIG").or_else(|| config_dir().map(|d| d.join("config.toml")))
}

/// Where sharetree keeps volume images and generated scripts.
pub fn data_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        return env_path("LOCALAPPDATA").map(|p| p.join("sharetree"));
    }
    env_path("XDG_DATA_HOME")
        .or_else(|| home().map(|h| h.join(".local").join("share")))
        .map(|p| p.join("sharetree"))
}

/// systemd user units (`$XDG_CONFIG_HOME/systemd/user`).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn systemd_user_dir() -> Option<PathBuf> {
    env_path("XDG_CONFIG_HOME")
        .or_else(|| home().map(|h| h.join(".config")))
        .map(|p| p.join("systemd").join("user"))
}

impl Config {
    pub fn parse(text: &str) -> Result<Config> {
        toml::from_str(text).context("invalid sharetree config")
    }

    pub fn to_toml(&self) -> Result<String> {
        let body = toml::to_string(self).context("serializing sharetree config")?;
        Ok(format!(
            "# sharetree configuration (see `sharetree setup status`).\n{body}"
        ))
    }

    /// The config file, or an empty config when there is none.
    pub fn load() -> Result<Config> {
        let Some(p) = config_path() else {
            return Ok(Config::default());
        };
        match std::fs::read_to_string(&p) {
            Ok(t) => Config::parse(&t).with_context(|| p.display().to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
        }
    }

    /// Write atomically (temp file + rename).
    pub fn save(&self) -> Result<PathBuf> {
        let Some(p) = config_path() else {
            bail!("cannot locate a config directory (set HOME or XDG_CONFIG_HOME)");
        };
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
        }
        let tmp = p.with_extension(format!("toml.tmp{}", std::process::id()));
        std::fs::write(&tmp, self.to_toml()?)
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &p).with_context(|| format!("writing {}", p.display()))?;
        Ok(p)
    }

    pub fn volume(&self, name: &str) -> Option<&Volume> {
        self.volumes.iter().find(|v| v.name == name)
    }

    pub fn volume_mut(&mut self, name: &str) -> Option<&mut Volume> {
        self.volumes.iter_mut().find(|v| v.name == name)
    }

    pub fn repo(&self, common_dir: &Path) -> Option<&RepoConfig> {
        self.repos
            .iter()
            .find(|r| same_path(&r.common_dir, common_dir))
    }

    pub fn repo_mut(&mut self, common_dir: &Path) -> &mut RepoConfig {
        if let Some(i) = self
            .repos
            .iter()
            .position(|r| same_path(&r.common_dir, common_dir))
        {
            return &mut self.repos[i];
        }
        self.repos.push(RepoConfig {
            common_dir: common_dir.to_path_buf(),
            ..Default::default()
        });
        self.repos.last_mut().expect("just pushed")
    }

    /// The volume a repository uses: the one its `[[repo]]` entry names, or
    /// else the only volume configured (volumes are per user, not per repo).
    pub fn volume_for(&self, common_dir: &Path) -> Option<&Volume> {
        match self.repo(common_dir).and_then(|r| r.volume.as_deref()) {
            Some(name) => self.volume(name),
            None if self.volumes.len() == 1 => self.volumes.first(),
            None => self.volume("default"),
        }
    }

    /// Whether worktrees of this repository go into its volume automatically
    /// when the destination cannot reflink: the repository's own setting,
    /// else the global one, else None (the user has not chosen yet).
    pub fn auto_volume_for(&self, common_dir: &Path) -> Option<bool> {
        self.repo(common_dir)
            .and_then(|r| r.auto_volume)
            .or(self.auto_volume)
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    a == b || crate::util::canon(a) == crate::util::canon(b)
}

/// Parse a size such as `100G`, `512M`, `1.5T` or plain bytes (binary units).
pub fn parse_size(s: &str) -> Result<u64> {
    let t = s.trim();
    let t = t
        .strip_suffix("iB")
        .or_else(|| t.strip_suffix('B'))
        .unwrap_or(t);
    let (num, mult) = match t.chars().last() {
        Some('K' | 'k') => (&t[..t.len() - 1], 1u64 << 10),
        Some('M' | 'm') => (&t[..t.len() - 1], 1 << 20),
        Some('G' | 'g') => (&t[..t.len() - 1], 1 << 30),
        Some('T' | 't') => (&t[..t.len() - 1], 1 << 40),
        _ => (t, 1),
    };
    let v: f64 = num
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("not a size: {s:?} (try 100G)"))?;
    if !(v.is_finite() && v > 0.0) {
        bail!("not a size: {s:?}");
    }
    Ok((v * mult as f64) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vol(name: &str) -> Volume {
        Volume {
            name: name.into(),
            kind: VolumeKind::Btrfs,
            image: PathBuf::from(format!("/data/{name}.img")),
            label: "sharetree".into(),
            size: 1 << 30,
            mount: MountMethod::Udisks,
            mountpoint: Some(PathBuf::from("/run/media/u/sharetree")),
            unit: None,
        }
    }

    #[test]
    fn round_trips_through_toml() {
        let mut c = Config {
            auto_volume: Some(true),
            windows_clone: Some(false),
            volumes: vec![vol("default")],
            repos: vec![],
            overlays: vec![OverlayEntry {
                path: "/src/app-fix".into(),
                common_dir: "/src/app/.git".into(),
                lower: "/d/bases/abc".into(),
                upper: "/d/wt/fix/upper".into(),
                work: "/d/wt/fix/work".into(),
            }],
        };
        c.repo_mut(Path::new("/src/app/.git")).auto_volume = Some(false);
        let text = c.to_toml().unwrap();
        assert!(
            text.contains("[[volume]]") && text.contains("[[overlay]]"),
            "{text}"
        );
        assert!(text.contains("windows_clone = false"), "{text}");
        assert!(text.contains("kind = \"btrfs\""), "{text}");
        assert!(text.contains("mount = \"udisks\""), "{text}");
        assert!(!text.contains("unit"), "None fields are omitted: {text}");
        assert_eq!(Config::parse(&text).unwrap(), c);
    }

    #[test]
    fn empty_and_partial_configs_parse() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
        let c = Config::parse(
            "auto_volume = true\n[[volume]]\nname='v'\nkind='xfs'\nimage='/i'\nlabel='l'\nmount='fstab'\nmountpoint='/mnt/v'\n",
        )
        .unwrap();
        assert_eq!(c.auto_volume, Some(true));
        assert_eq!(c.volumes[0].kind, VolumeKind::Xfs);
        assert_eq!(c.volumes[0].size, 0);
        assert!(Config::parse("[[volume]]\nname='v'\nkind='ntfs'\n").is_err());
    }

    #[test]
    fn volume_and_auto_resolution_per_repo() {
        let repo = Path::new("/src/app/.git");
        let other = Path::new("/src/other/.git");
        let mut c = Config::default();
        assert!(c.volume_for(repo).is_none());

        c.volumes.push(vol("big"));
        assert_eq!(c.volume_for(repo).unwrap().name, "big", "the only volume");

        c.volumes.push(vol("default"));
        assert_eq!(c.volume_for(repo).unwrap().name, "default");
        c.repo_mut(repo).volume = Some("big".into());
        assert_eq!(c.volume_for(repo).unwrap().name, "big");
        assert_eq!(c.volume_for(other).unwrap().name, "default");

        assert_eq!(c.auto_volume_for(repo), None, "undecided");
        c.auto_volume = Some(true);
        assert_eq!(c.auto_volume_for(repo), Some(true));
        assert_eq!(c.auto_volume_for(other), Some(true));
        c.repo_mut(repo).auto_volume = Some(false);
        assert_eq!(c.auto_volume_for(repo), Some(false));
        assert_eq!(c.auto_volume_for(other), Some(true));
        c.auto_volume = None;
        assert_eq!(c.auto_volume_for(other), None);
        assert!(!Config::default().to_toml().unwrap().contains("auto_volume"));
        assert_eq!(c.repos.len(), 1, "repo_mut reuses the entry");
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert_eq!(parse_size("1G").unwrap(), 1 << 30);
        assert_eq!(parse_size("1GiB").unwrap(), 1 << 30);
        assert_eq!(parse_size("512m").unwrap(), 512 << 20);
        assert_eq!(parse_size("1.5T").unwrap(), 3 << 39);
        assert!(parse_size("lots").is_err());
        assert!(parse_size("-1G").is_err());
    }
}
