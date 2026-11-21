use std::fs::{File, OpenOptions, TryLockError};
use std::io::Write;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MountMethod {
    Udisks,
    Fstab,
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OverlayPhase {
    Creating,
    #[default]
    Ready,
    Removing,
}

impl OverlayPhase {
    fn is_ready(&self) -> bool {
        *self == Self::Ready
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OverlayEntry {
    pub path: PathBuf,
    pub common_dir: PathBuf,
    pub lower: PathBuf,
    pub upper: PathBuf,
    pub work: PathBuf,
    #[serde(default, skip_serializing_if = "OverlayPhase::is_ready")]
    pub phase: OverlayPhase,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_volume: Option<bool>,
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

pub fn data_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        return env_path("LOCALAPPDATA").map(|p| p.join("sharetree"));
    }
    env_path("XDG_DATA_HOME")
        .or_else(|| home().map(|h| h.join(".local").join("share")))
        .map(|p| p.join("sharetree"))
}

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

    pub fn load() -> Result<Config> {
        let Some(p) = config_path() else {
            return Ok(Config::default());
        };
        Self::load_at(&p)
    }

    fn load_at(p: &Path) -> Result<Config> {
        match std::fs::read_to_string(p) {
            Ok(t) => Config::parse(&t).with_context(|| p.display().to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
        }
    }

    pub fn update<T>(edit: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        Self::update_at(&required_config_path()?, edit)
    }

    fn update_at<T>(p: &Path, edit: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        let lock = open_lock(p, ".lock")?;
        lock.lock()
            .with_context(|| format!("locking the config at {}", p.display()))?;
        let previous = Self::load_at(p)?;
        let mut next = previous.clone();
        let result = edit(&mut next)?;
        if next != previous {
            let dir = p.parent().context("config path has no parent directory")?;
            let mut tmp = tempfile::NamedTempFile::new_in(dir)
                .with_context(|| format!("creating a temporary file in {}", dir.display()))?;
            tmp.write_all(next.to_toml()?.as_bytes())
                .and_then(|()| tmp.as_file().sync_all())
                .with_context(|| format!("writing {}", tmp.path().display()))?;
            tmp.persist(p)
                .with_context(|| format!("replacing {}", p.display()))?;
        }
        Ok(result)
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

    pub fn volume_for(&self, common_dir: &Path) -> Option<&Volume> {
        match self.repo(common_dir).and_then(|r| r.volume.as_deref()) {
            Some(name) => self.volume(name),
            None if self.volumes.len() == 1 => self.volumes.first(),
            None => self.volume("default"),
        }
    }

    pub fn auto_volume_for(&self, common_dir: &Path) -> Option<bool> {
        self.repo(common_dir)
            .and_then(|r| r.auto_volume)
            .or(self.auto_volume)
    }
}

pub fn lock_volume_setup() -> Result<File> {
    let lock = open_lock(&required_config_path()?, ".volume-lock")?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(TryLockError::WouldBlock) => {
            bail!("a sharetree volume operation is active; retry when it finishes")
        }
        Err(TryLockError::Error(e)) => Err(e).context("locking volume setup"),
    }
}

pub fn lock_volume_use() -> Result<File> {
    let lock = open_lock(&required_config_path()?, ".volume-lock")?;
    lock.lock_shared().context("locking volume use")?;
    Ok(lock)
}

fn required_config_path() -> Result<PathBuf> {
    config_path().context("cannot locate the config file (set SHARETREE_CONFIG)")
}

fn open_lock(path: &Path, suffix: &str) -> Result<File> {
    let dir = path
        .parent()
        .context("config path has no parent directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(suffix);
    let lock_path = PathBuf::from(lock_path);
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))
}

fn same_path(a: &Path, b: &Path) -> bool {
    a == b || crate::util::canon(a) == crate::util::canon(b)
}

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
    let num = num.trim();
    let invalid = || anyhow::anyhow!("not a size: {s:?} (try 100G)");
    if let Ok(integer) = num.parse::<u64>() {
        return integer
            .checked_mul(mult)
            .filter(|&bytes| bytes > 0)
            .ok_or_else(invalid);
    }
    let bytes = num.parse::<f64>().map_err(|_| invalid())? * mult as f64;
    if !(1.0..(u64::MAX as f64)).contains(&bytes) {
        bail!("not a size: {s:?}");
    }
    Ok(bytes as u64)
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
                phase: OverlayPhase::Ready,
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
        assert!(parse_size("0.5B").is_err());
        assert!(parse_size("1e300T").is_err());
        assert!(parse_size("18446744073709551616").is_err());
        assert_eq!(parse_size("18446744073709551615").unwrap(), u64::MAX);
        assert_eq!(
            parse_size("9007199254740993").unwrap(),
            9_007_199_254_740_993
        );
    }

    #[test]
    fn concurrent_updates_preserve_every_repository_override() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::thread::scope(|scope| {
            for n in 0..12 {
                let p = &p;
                scope.spawn(move || {
                    Config::update_at(p, |cfg| {
                        cfg.repo_mut(Path::new(&format!("repo-{n}"))).auto_volume =
                            Some(n % 2 == 0);
                        Ok(())
                    })
                    .unwrap();
                });
            }
        });
        assert_eq!(Config::load_at(&p).unwrap().repos.len(), 12);
    }

    #[test]
    fn config_update_keeps_the_original_when_edit_fails() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "auto_volume = true\n").unwrap();
        assert!(Config::update_at::<()>(&p, |cfg| {
            cfg.auto_volume = Some(false);
            bail!("edit failed")
        })
        .is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "auto_volume = true\n");
    }
}
