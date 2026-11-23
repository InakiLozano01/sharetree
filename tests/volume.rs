#![cfg(target_os = "linux")]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use common::*;

fn on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()))
}

fn polkit_allows(action: &str) -> bool {
    Command::new("pkcheck")
        .args(["--action-id", action, "--process"])
        .arg(std::process::id().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn skip_reason(kind: &str) -> Option<String> {
    if std::env::var_os("SHARETREE_SKIP_VOLUME_TEST").is_some() {
        return Some("SHARETREE_SKIP_VOLUME_TEST is set".into());
    }
    let fs_tools: &[&str] = if kind == "xfs" {
        &["mkfs.xfs"]
    } else {
        &["mkfs.btrfs", "btrfs"]
    };
    for tool in ["udisksctl", "pkcheck"].iter().chain(fs_tools) {
        if !on_path(tool) {
            return Some(format!("{tool} is not installed"));
        }
    }
    for action in [
        "org.freedesktop.udisks2.loop-setup",
        "org.freedesktop.udisks2.filesystem-mount",
    ] {
        if !polkit_allows(action) {
            return Some(format!(
                "polkit does not allow {action} without a password here (not an active desktop session)"
            ));
        }
    }
    if fs_type(Path::new("/tmp")) != "tmpfs" {
        return Some("/tmp is not tmpfs (the test needs a filesystem without reflinks)".into());
    }
    None
}

fn loops_for(image: &Path) -> Vec<String> {
    let want = image.canonicalize().unwrap_or_else(|_| image.to_path_buf());
    std::fs::read_dir("/sys/block")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with("loop"))
        .filter(|n| {
            std::fs::read_to_string(format!("/sys/block/{n}/loop/backing_file"))
                .is_ok_and(|b| Path::new(b.trim_end()) == want)
        })
        .map(|n| format!("/dev/{n}"))
        .collect()
}

struct Cleanup {
    image: PathBuf,
    config: PathBuf,
    repo: Option<PathBuf>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let mut c = Command::new(BIN);
        c.args(["setup", "teardown", "--delete-image", "--force"])
            .env("SHARETREE_CONFIG", &self.config)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(r) = &self.repo {
            c.current_dir(r);
        }
        let _ = c.status();
        for dev in loops_for(&self.image) {
            for args in [["unmount", "-b"], ["loop-delete", "-b"]] {
                let _ = Command::new("udisksctl")
                    .args(args)
                    .arg(&dev)
                    .arg("--no-user-interaction")
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
        let _ = std::fs::remove_file(&self.image);
    }
}

fn run(sb: &Sandbox, dir: &Path, cfg: &Path, args: &[&str]) -> (bool, String, String) {
    let out = sb
        .sharetree_cmd(dir)
        .env("SHARETREE_CONFIG", cfg)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn ok(sb: &Sandbox, dir: &Path, cfg: &Path, args: &[&str]) -> (String, String) {
    let (good, out, err) = run(sb, dir, cfg, args);
    assert!(good, "sharetree {args:?} failed:\n{out}\n{err}");
    (out, err)
}

#[test]
fn setup_btrfs_volume_then_add_into_it() {
    scenario("btrfs");
}

#[test]
fn setup_xfs_volume_then_add_into_it() {
    scenario("xfs");
}

#[test]
fn teardown_force_keeps_an_image_that_cannot_be_inspected() {
    let sb = Sandbox::home();
    let cfg = sb.p("config.toml");
    let image = sb.p("detached.img");
    write(&image, b"disposable image contents");
    let config = format!(
        "[[volume]]\nname = 'test'\nimage = '{}'\nkind = 'btrfs'\nlabel = 'test'\nmount = 'fstab'\nmountpoint = '{}'\n",
        image.display(),
        sb.p("unmounted").display(),
    );
    write(&cfg, config.as_bytes());

    let (success, _, err) = run(
        &sb,
        sb.root(),
        &cfg,
        &["setup", "teardown", "--delete-image", "--force"],
    );
    assert!(
        !success && err.contains("cannot inspect the volume"),
        "{err}"
    );
    assert_eq!(std::fs::read(image).unwrap(), b"disposable image contents");
    assert_eq!(std::fs::read_to_string(cfg).unwrap(), config);
}

fn du_raw(sb: &Sandbox, dir: &Path, cfg: &Path, path: &Path) -> (u64, Option<u64>) {
    let (out, _) = ok(sb, dir, cfg, &["du", "--raw", &path.to_string_lossy()]);
    let mut f = out.lines().next().unwrap_or_default().split('\t');
    let apparent = f.next().and_then(|x| x.parse().ok()).expect(&out);
    (apparent, f.next().and_then(|x| x.parse().ok()))
}

fn free_bytes(p: &Path) -> u64 {
    let _ = Command::new("sync").arg("-f").arg(p).status();
    let out = Command::new("stat")
        .args(["-f", "-c", "%a %S"])
        .arg(p)
        .output()
        .unwrap();
    let t = String::from_utf8_lossy(&out.stdout);
    let mut f = t.split_whitespace().map(|x| x.parse::<u64>().unwrap());
    f.next().unwrap() * f.next().unwrap()
}

fn scenario(kind: &str) {
    if let Some(why) = skip_reason(kind) {
        eprintln!("SKIP setup_{kind}_volume_then_add_into_it: {why}");
        return;
    }
    let state = Sandbox::home();
    let Some(sb) = Sandbox::tmpfs() else { return };
    let cfg = state.p("config.toml");
    let image = state.p(&format!("volume-{kind}.img"));
    let label = format!("st{}{}", &kind[..1], std::process::id() % 100_000_000);
    let mut guard = Cleanup {
        image: image.clone(),
        config: cfg.clone(),
        repo: None,
    };
    let img = image.to_string_lossy().into_owned();
    let setup = [
        "setup",
        "volume",
        "--fs",
        kind,
        "--size",
        "1G",
        "--image",
        &img,
        "--label",
        &label,
        "--no-unit",
    ];

    let mut print_args = setup.to_vec();
    print_args.retain(|a| *a != "--no-unit");
    print_args.push("--print");
    let (plan, _) = ok(&sb, sb.root(), &cfg, &print_args);
    assert!(plan.contains("udisksctl loop-setup"), "{plan}");
    assert!(
        !image.exists() && !cfg.exists(),
        "--print must not change anything"
    );
    let unit: String = plan
        .lines()
        .skip_while(|l| !l.starts_with("# systemd user unit"))
        .skip(1)
        .take_while(|l| l.starts_with("#   "))
        .map(|l| format!("{}\n", &l[4..]))
        .collect();
    assert!(
        unit.contains("ExecStart=") && unit.contains("setup mount --volume"),
        "{unit}"
    );
    if on_path("systemd-analyze") {
        let f = state.p("sharetree-volume-default.service");
        std::fs::write(
            &f,
            unit.replace(
                &format!("ConditionPathExists={img}"),
                "ConditionPathExists=/",
            ),
        )
        .unwrap();
        let v = Command::new("systemd-analyze")
            .args(["--user", "verify"])
            .arg(&f)
            .output()
            .unwrap();
        assert!(
            v.status.success(),
            "systemd-analyze verify: {}",
            String::from_utf8_lossy(&v.stderr)
        );
    }

    let (out, _) = ok(&sb, sb.root(), &cfg, &setup);
    assert!(out.contains("is mounted at"), "{out}");
    ok(&sb, sb.root(), &cfg, &setup);
    let (mp, _) = ok(&sb, sb.root(), &cfg, &["setup", "mount"]);
    let mp = PathBuf::from(mp.trim());
    assert_eq!(fs_type(&mp), kind);
    let (st, _) = ok(&sb, sb.root(), &cfg, &["setup", "status"]);
    assert!(
        st.contains("mounted at") && st.contains(&format!("kind       {kind}")),
        "{st}"
    );

    let main = sb.repo("main", &[]);
    guard.repo = Some(main.clone());
    sb.git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            &sb.p("stock").to_string_lossy(),
        ],
    );

    let (_, err) = ok(
        &sb,
        &main,
        &cfg,
        &["add", "--detach", &sb.p("plain").to_string_lossy()],
    );
    assert!(
        err.contains("cannot reflink") && err.contains("--into-volume"),
        "{err}"
    );
    assert!(sb.p("plain").is_dir() && !sb.p("plain").is_symlink());

    let before_placement = std::fs::read(&cfg).unwrap();
    let cow = sb.p("cow");
    let (_, err) = ok(
        &sb,
        &main,
        &cfg,
        &["add", "--into-volume", "--detach", &cow.to_string_lossy()],
    );
    assert!(err.contains("creating the volume's seed checkout"), "{err}");
    assert!(reflinked(&err) > 0, "{err}");
    assert!(cow.is_symlink(), "a symlink is left at the requested path");
    let real = std::fs::read_link(&cow).unwrap();
    assert!(
        real.starts_with(&mp),
        "{} not in {}",
        real.display(),
        mp.display()
    );
    assert_equivalent(&sb, &sb.p("stock"), &cow);
    let list = sb.git(&main, &["worktree", "list", "--porcelain"]);
    assert!(list.contains("locked sharetree seed"), "{list}");
    assert!(list.contains("locked sharetree volume"), "{list}");
    let (apparent, excl) = du_raw(&sb, &main, &cfg, &real);
    let excl = excl.expect("exclusive bytes are measurable in the volume");
    assert!(apparent > 512 * 1024, "apparent {apparent}");
    assert!(excl < 64 * 1024, "exclusive {excl} of {apparent} bytes");
    if kind == "btrfs" {
        let b = btrfs_exclusive(&content_paths(&real)).expect("btrfs filesystem du");
        assert_eq!(b, excl, "sharetree du uses btrfs filesystem du");
    }

    let inside = real.with_file_name("inside");
    let (_, err) = ok(
        &sb,
        &main,
        &cfg,
        &["add", "--detach", &inside.to_string_lossy()],
    );
    assert!(reflinked(&err) > 0, "{err}");
    assert!(!err.contains("creating the volume's seed"), "{err}");
    assert!(inside.is_dir() && !inside.is_symlink(), "{err}");
    assert_equivalent(&sb, &sb.p("stock"), &inside);
    assert_eq!(std::fs::read(&cfg).unwrap(), before_placement);
    ok(
        &sb,
        &main,
        &cfg,
        &["rm", "--sync", &inside.to_string_lossy()],
    );

    ok(&sb, &main, &cfg, &["setup", "auto", "on", "--repo"]);
    let free_before = free_bytes(&mp);
    let (_, err) = ok(
        &sb,
        &main,
        &cfg,
        &["add", "--detach", &sb.p("auto").to_string_lossy()],
    );
    assert!(sb.p("auto").is_symlink(), "{err}");
    let used = free_before.saturating_sub(free_bytes(&mp));
    eprintln!("{kind}: second worktree used {used} bytes of the volume ({apparent} apparent)");
    assert!(
        used < apparent / 2,
        "statvfs delta {used} vs apparent {apparent}"
    );
    assert!(
        !err.contains("creating the volume's seed"),
        "the seed is reused: {err}"
    );
    write(&main.join("src/mod3/data3.bin"), &bytes(333, 20_000));
    let head = sb.commit_all(&main, "change");
    let (_, err) = ok(
        &sb,
        &main,
        &cfg,
        &[
            "add",
            "--into-volume",
            "--detach",
            &sb.p("next").to_string_lossy(),
        ],
    );
    let seed = real.parent().unwrap().join(".sharetree-seed");
    assert_eq!(sb.git(&seed, &["rev-parse", "HEAD"]).trim(), head, "{err}");
    assert!(reflinked(&err) > 0, "{err}");
    ok(
        &sb,
        &main,
        &cfg,
        &["rm", "--sync", &sb.p("next").to_string_lossy()],
    );
    let (_, err) = ok(
        &sb,
        &main,
        &cfg,
        &[
            "add",
            "--no-volume",
            "--detach",
            &sb.p("novol").to_string_lossy(),
        ],
    );
    assert!(
        sb.p("novol").is_dir() && !sb.p("novol").is_symlink(),
        "{err}"
    );

    let (doc, _) = ok(&sb, &main, &cfg, &["doctor"]);
    assert!(doc.contains("tmpfs") && doc.contains("mounted at"), "{doc}");

    ok(&sb, &main, &cfg, &["rm", "--sync", &cow.to_string_lossy()]);
    assert!(cow.symlink_metadata().is_err() && !real.exists());

    let config = std::fs::read_to_string(&cfg).unwrap();
    let fstab_config = config.replace("mount = \"udisks\"", "mount = \"fstab\"");
    assert_ne!(fstab_config, config);
    write(&cfg, fstab_config.as_bytes());
    let (good, _, err) = run(
        &sb,
        &main,
        &cfg,
        &["setup", "teardown", "--delete-image", "--force"],
    );
    assert!(!good && err.contains("/etc/fstab"), "{err}");
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), fstab_config);
    assert!(image.exists() && seed.join(".git").is_file() && sb.p("auto/.git").is_file());
    write(&cfg, config.as_bytes());

    let (good, _, err) = run(&sb, &main, &cfg, &["setup", "teardown", "--delete-image"]);
    assert!(!good && err.contains("--force"), "{err}");
    ok(
        &sb,
        &main,
        &cfg,
        &["rm", "--sync", &sb.p("auto").to_string_lossy()],
    );
    ok(&sb, &main, &cfg, &["setup", "teardown", "--delete-image"]);
    assert!(loops_for(&image).is_empty(), "loop device left behind");
    assert!(!image.exists() && !mp.exists());
    let list = sb.git(&main, &["worktree", "list", "--porcelain"]);
    assert!(
        !list.contains("sharetree-seed"),
        "seed registration removed: {list}"
    );
    let (st, _) = ok(&sb, &main, &cfg, &["setup", "status"]);
    assert!(st.contains("volumes      none"), "{st}");
}
