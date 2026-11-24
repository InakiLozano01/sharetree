use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Result};

pub struct ReshareArgs {
    pub all: bool,
    pub target: Option<PathBuf>,
    pub from: Option<PathBuf>,
    pub dry_run: bool,
    pub min_size: u64,
    pub verbose: bool,
}

fn parse(args: &[OsString]) -> Result<ReshareArgs> {
    let mut r = ReshareArgs {
        all: false,
        target: None,
        from: None,
        dry_run: false,
        min_size: crate::add::DEFAULT_MIN_SIZE,
        verbose: false,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].to_str() {
            Some("--all") => r.all = true,
            Some("--from") => {
                i += 1;
                r.from = Some(PathBuf::from(
                    args.get(i)
                        .ok_or_else(|| anyhow!("--from requires a path"))?,
                ));
            }
            Some(s) if s.starts_with("--from=") => r.from = Some(PathBuf::from(&s[7..])),
            Some("--dry-run" | "-n") => r.dry_run = true,
            Some("-v" | "--verbose") => r.verbose = true,
            Some("--min-size") => {
                i += 1;
                let v = args
                    .get(i)
                    .and_then(|s| s.to_str())
                    .ok_or_else(|| anyhow!("--min-size requires a number"))?;
                r.min_size = v.parse()?;
            }
            Some(s) if s.starts_with('-') && s.len() > 1 => bail!("reshare: unknown option {s}"),
            _ if r.target.is_none() => r.target = Some(PathBuf::from(&args[i])),
            _ => bail!("reshare: unexpected argument {}", args[i].to_string_lossy()),
        }
        i += 1;
    }
    if r.all && (r.target.is_some() || r.from.is_some()) {
        bail!("reshare: --all cannot be combined with a target or --from");
    }
    Ok(r)
}

pub fn run(args: &[OsString]) -> Result<i32> {
    let a = parse(args)?;
    #[cfg(target_os = "linux")]
    {
        imp::run(a)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = a;
        bail!(
            "reshare needs FIDEDUPERANGE and is Linux-only for now (macOS has no in-place dedupe)"
        )
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::cell::OnceCell;
    use std::collections::hash_map::Entry;
    use std::collections::HashMap;
    use std::fs::{File, OpenOptions};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::Arc;

    use anyhow::{bail, Result};

    use super::ReshareArgs;
    use crate::git::{self, MODE_EXEC, MODE_REGULAR};
    use crate::linux::{self, DedupeOutcome};
    use crate::util::{self, bytes_to_path, human};

    struct Pair {
        sources: Vec<Arc<Path>>,
        dst: PathBuf,
        len: u64,
    }

    struct BlobCopies {
        first_checkout: usize,
        src: PathBuf,
        targets: Vec<PathBuf>,
        has_peer: bool,
    }

    fn isolate_changes(wt: &Path, paths: &[&[u8]], dirty: &mut std::collections::HashSet<Vec<u8>>) {
        match git::changed_tracked_paths_under(wt, paths) {
            Ok(changed) => dirty.extend(changed),
            Err(_) if paths.len() == 1 => {
                dirty.insert(paths[0].to_vec());
            }
            Err(_) => {
                let (left, right) = paths.split_at(paths.len() / 2);
                isolate_changes(wt, left, dirty);
                isolate_changes(wt, right, dirty);
            }
        }
    }

    fn clean_files(wt: &Path) -> Result<Vec<(Vec<u8>, String)>> {
        let entries = git::ls_files_stage(wt)?;
        let dirty = git::changed_tracked_paths(wt, false).unwrap_or_else(|err| {
            eprintln!(
                "sharetree: {}: checking paths separately after Git status failed: {err:#}",
                wt.display()
            );
            let paths = entries
                .iter()
                .map(|(path, _, _)| path.as_slice())
                .collect::<Vec<_>>();
            let mut dirty = std::collections::HashSet::new();
            let mut start = 0;
            let mut bytes = 0;
            for (i, path) in paths.iter().enumerate() {
                let size = path.len() + 32;
                if i > start && bytes + size > 64 * 1024 {
                    isolate_changes(wt, &paths[start..i], &mut dirty);
                    start = i;
                    bytes = 0;
                }
                bytes += size;
            }
            if start < paths.len() {
                isolate_changes(wt, &paths[start..], &mut dirty);
            }
            dirty
        });
        Ok(entries
            .into_iter()
            .filter(|(p, mode, _)| {
                (*mode == MODE_REGULAR || *mode == MODE_EXEC) && !dirty.contains(p)
            })
            .map(|(p, _, oid)| (p, oid))
            .collect())
    }

    fn open_ro(p: &Path) -> std::io::Result<File> {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(p)
    }

    #[repr(C)]
    struct BtrfsFsInfo {
        max_id: u64,
        num_devices: u64,
        fsid: [u8; 16],
        rest: [u8; 992],
    }

    const BTRFS_IOC_FS_INFO: libc::Ioctl = libc::_IOR::<BtrfsFsInfo>(0x94, 31);
    const _: () = {
        assert!(std::mem::size_of::<BtrfsFsInfo>() == 1024);
        assert!(std::mem::offset_of!(BtrfsFsInfo, fsid) == 16);
    };

    fn btrfs_fsid(file: &File) -> Option<[u8; 16]> {
        let fd = file.as_raw_fd();
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstatfs(fd, &mut stat) } != 0
            || stat.f_type != libc::BTRFS_SUPER_MAGIC as _
        {
            return None;
        }
        let mut info: BtrfsFsInfo = unsafe { std::mem::zeroed() };
        (unsafe { libc::ioctl(fd, BTRFS_IOC_FS_INFO, &mut info) } == 0).then_some(info.fsid)
    }

    fn same_filesystem(
        first_dev: u64,
        second_dev: u64,
        first_fsid: impl FnOnce() -> Option<[u8; 16]>,
        second_fsid: impl FnOnce() -> Option<[u8; 16]>,
    ) -> bool {
        first_dev == second_dev || first_fsid().is_some_and(|id| second_fsid() == Some(id))
    }

    fn filesystem_matcher(root: &Path, root_dev: u64) -> impl Fn(&Path, u64) -> bool + '_ {
        let root_fsid = OnceCell::new();
        let fsid_at = |path: &Path| {
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(path)
                .ok()
                .as_ref()
                .and_then(btrfs_fsid)
        };
        move |path, dev| {
            same_filesystem(
                root_dev,
                dev,
                || *root_fsid.get_or_init(|| fsid_at(root)),
                || fsid_at(path),
            )
        }
    }

    fn same_extents(a: &File, b: &File) -> bool {
        match (linux::extents(a, false), linux::extents(b, false)) {
            (Ok(x), Ok(y)) => {
                !x.is_empty()
                    && x.len() == y.len()
                    && x.iter().zip(&y).all(|(p, q)| {
                        p.logical == q.logical
                            && p.physical == q.physical
                            && p.length == q.length
                            && p.shared()
                            && q.shared()
                    })
            }
            _ => false,
        }
    }

    fn plan_target(cwd: &Path, a: &ReshareArgs) -> Result<(Vec<Pair>, String)> {
        let target = match &a.target {
            Some(p) => git::toplevel(&util::absolutize(p, cwd))?,
            None => git::toplevel(cwd)?,
        };
        let tcanon = util::canon(&target);
        let on_filesystem = filesystem_matcher(&target, target.metadata()?.dev());
        let donors: Vec<PathBuf> = match &a.from {
            Some(p) => {
                let donor = git::toplevel(&util::absolutize(p, cwd))?;
                if util::canon(&donor) == tcanon {
                    bail!("the donor and target are the same worktree");
                }
                if !on_filesystem(&donor, donor.metadata()?.dev()) {
                    bail!("the donor and target must be on the same filesystem");
                }
                vec![donor]
            }
            None => git::list_worktrees(&target)?
                .into_iter()
                .filter(|w| !w.bare && !w.prunable && util::canon(&w.path) != tcanon)
                .filter(|w| {
                    w.path
                        .metadata()
                        .is_ok_and(|m| on_filesystem(&w.path, m.dev()))
                })
                .map(|w| w.path)
                .collect(),
        };
        if donors.is_empty() {
            bail!("no other worktree on the same filesystem to share with");
        }

        let mut by_oid: HashMap<String, Vec<Arc<Path>>> = HashMap::new();
        for d in &donors {
            if util::canon(d) == tcanon {
                continue;
            }
            for (p, oid) in clean_files(d)? {
                let sources = by_oid.entry(oid).or_default();
                if sources.len() < 4 {
                    sources.push(d.join(bytes_to_path(&p)).into());
                }
            }
        }
        let work = clean_files(&target)?
            .into_iter()
            .filter_map(|(p, oid)| {
                let src = by_oid.get(&oid)?;
                let dst = target.join(bytes_to_path(&p));
                let len = dst.symlink_metadata().ok().filter(|m| m.is_file())?.len();
                (len >= a.min_size).then(|| Pair {
                    sources: src.clone(),
                    dst,
                    len,
                })
            })
            .collect();
        let scope = format!("in {} with {} donor(s)", target.display(), donors.len());
        Ok((work, scope))
    }

    fn git_dir_identity(dir: &[u8]) -> Option<(u64, u64)> {
        let path = bytes_to_path(dir.strip_suffix(b"\n")?);
        let metadata = path.metadata().ok()?;
        Some((metadata.dev(), metadata.ino()))
    }

    fn plan_all(cwd: &Path, min_size: u64, trace: &util::Trace) -> Result<(Vec<Pair>, String)> {
        let mut blobs = HashMap::new();
        let mut checkouts = 0;
        let on_filesystem = filesystem_matcher(cwd, cwd.metadata()?.dev());
        let common_dir = git::output(
            cwd,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        let common_identity = git_dir_identity(&common_dir);
        for w in git::list_worktrees(cwd)? {
            if w.bare || w.prunable {
                continue;
            }
            let Ok(root) = w.path.metadata() else {
                continue;
            };
            if !root.is_dir() || !on_filesystem(&w.path, root.dev()) {
                continue;
            }
            let identity = git::output(
                &w.path,
                &[
                    "rev-parse",
                    "--show-cdup",
                    "--path-format=absolute",
                    "--git-common-dir",
                ],
            );
            let is_same_repo_checkout = identity
                .as_deref()
                .ok()
                .and_then(|output| output.strip_prefix(b"\n"))
                .is_some_and(|dir| {
                    dir == common_dir
                        || common_identity
                            .is_some_and(|identity| git_dir_identity(dir) == Some(identity))
                });
            if !is_same_repo_checkout {
                trace.note(format!(
                    "skipping worktree registration without a matching checkout at {}",
                    w.path.display()
                ));
                continue;
            }
            checkouts += 1;
            let dev = root.dev();
            for (rel, oid) in clean_files(&w.path)? {
                let path = w.path.join(bytes_to_path(&rel));
                let Ok(m) = path.symlink_metadata() else {
                    continue;
                };
                let len = m.len();
                if !m.is_file()
                    || len == 0
                    || len < min_size
                    || (m.dev() != dev && !on_filesystem(&path, m.dev()))
                {
                    continue;
                }
                match blobs.entry((oid, len)) {
                    Entry::Vacant(entry) => {
                        entry.insert(BlobCopies {
                            first_checkout: checkouts,
                            src: path,
                            targets: Vec::new(),
                            has_peer: false,
                        });
                    }
                    Entry::Occupied(mut entry) => {
                        let copies = entry.get_mut();
                        copies.has_peer |= copies.first_checkout != checkouts;
                        copies.targets.push(path);
                    }
                }
            }
        }

        let mut work = Vec::new();
        for ((_, len), copies) in blobs {
            if copies.has_peer {
                let first: Arc<Path> = copies.src.into();
                let mut recent: Vec<Arc<Path>> = Vec::new();
                for dst in copies.targets {
                    let mut sources = vec![Arc::clone(&first)];
                    sources.extend(recent.iter().rev().cloned());
                    if recent.len() == 3 {
                        recent.remove(0);
                    }
                    recent.push(dst.clone().into());
                    work.push(Pair { sources, dst, len });
                }
            }
        }
        Ok((work, format!("across {checkouts} worktree(s)")))
    }

    pub fn run(a: ReshareArgs) -> Result<i32> {
        let trace = util::Trace::new(a.verbose);
        let cwd = std::env::current_dir()?;
        let (work, scope) = if a.all {
            plan_all(&cwd, a.min_size, &trace)?
        } else {
            plan_target(&cwd, &a)?
        };
        trace.note(format!("{} candidate files with a donor copy", work.len()));

        let next = AtomicUsize::new(0);
        let files = AtomicUsize::new(0);
        let bytes = AtomicU64::new(0);
        let already = AtomicUsize::new(0);
        let differ = AtomicUsize::new(0);
        let failed = AtomicUsize::new(0);
        let stat_moved = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..util::workers(work.len()).min(4) {
                s.spawn(|| loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(Pair { sources, dst, len }) = work.get(i) else {
                        break;
                    };
                    let Ok(df) = open_ro(dst) else {
                        failed.fetch_add(1, Ordering::Relaxed);
                        continue;
                    };
                    let Ok(before) = df.metadata() else {
                        failed.fetch_add(1, Ordering::Relaxed);
                        continue;
                    };
                    let dst_fsid = OnceCell::new();
                    let mut saw_difference = false;
                    let mut accepted = false;
                    let mut confirmed = 0;
                    for src in sources {
                        let Ok(sf) = open_ro(src) else { continue };
                        let Ok(sm) = sf.metadata() else { continue };
                        if !sm.is_file()
                            || !before.is_file()
                            || sm.len() != *len
                            || before.len() != *len
                            || (sm.dev(), sm.ino()) == (before.dev(), before.ino())
                            || !same_filesystem(
                                before.dev(),
                                sm.dev(),
                                || *dst_fsid.get_or_init(|| btrfs_fsid(&df)),
                                || btrfs_fsid(&sf),
                            )
                        {
                            continue;
                        }
                        if confirmed == 0 && same_extents(&sf, &df) {
                            already.fetch_add(1, Ordering::Relaxed);
                            accepted = true;
                            break;
                        }
                        if a.dry_run {
                            files.fetch_add(1, Ordering::Relaxed);
                            bytes.fetch_add(*len, Ordering::Relaxed);
                            accepted = true;
                            break;
                        }
                        match linux::dedupe(&sf, &df, confirmed, *len) {
                            DedupeOutcome::Deduped(n) => {
                                confirmed = n;
                                accepted = true;
                                break;
                            }
                            DedupeOutcome::Differs(n) => {
                                confirmed = n;
                                saw_difference = true;
                            }
                            DedupeOutcome::Failed(n, e) => {
                                confirmed = n;
                                if a.verbose {
                                    eprintln!("sharetree: {}: {e}", dst.display());
                                }
                            }
                        }
                    }
                    if confirmed > 0 {
                        files.fetch_add(1, Ordering::Relaxed);
                        bytes.fetch_add(confirmed, Ordering::Relaxed);
                        if let Ok(after) = df.metadata() {
                            if (
                                after.mtime(),
                                after.mtime_nsec(),
                                after.ctime(),
                                after.ctime_nsec(),
                            ) != (
                                before.mtime(),
                                before.mtime_nsec(),
                                before.ctime(),
                                before.ctime_nsec(),
                            ) {
                                stat_moved.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    if !accepted {
                        if saw_difference {
                            differ.fetch_add(1, Ordering::Relaxed);
                        } else {
                            failed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            }
        });
        let files = files.load(Ordering::Relaxed);
        let bytes = human(bytes.load(Ordering::Relaxed));
        let already = already.load(Ordering::Relaxed);
        let skipped = failed.load(Ordering::Relaxed);
        if a.dry_run {
            eprintln!("sharetree: dry run found {files} candidate files ({bytes}) {scope}; {already} already shared, {skipped} skipped");
        } else {
            let differ = differ.load(Ordering::Relaxed);
            eprintln!("sharetree: shared {files} files ({bytes}) {scope}; {already} already shared, {differ} differ, {skipped} skipped");
        }
        let moved = stat_moved.load(Ordering::Relaxed);
        if moved > 0 {
            eprintln!("sharetree: note: this filesystem updated timestamps on {moved} files; the next `git status` re-hashes them once");
        }
        Ok(0)
    }
}
