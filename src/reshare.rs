use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Result};

pub struct ReshareArgs {
    pub target: Option<PathBuf>,
    pub from: Option<PathBuf>,
    pub dry_run: bool,
    pub min_size: u64,
    pub verbose: bool,
}

fn parse(args: &[OsString]) -> Result<ReshareArgs> {
    let mut r = ReshareArgs {
        target: None,
        from: None,
        dry_run: false,
        min_size: crate::add::DEFAULT_MIN_SIZE,
        verbose: false,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].to_str() {
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
    use std::collections::HashMap;
    use std::fs::{self, File, OpenOptions};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    use anyhow::{bail, Result};

    use super::ReshareArgs;
    use crate::git::{self, MODE_EXEC, MODE_REGULAR};
    use crate::linux::{self, DedupeOutcome};
    use crate::util::{self, bytes_to_path, human};

    fn clean_files(wt: &Path) -> Result<Vec<(Vec<u8>, String)>> {
        let entries = git::ls_files_stage(wt)?;
        let dirty = git::changed_tracked_paths(wt, false)?;
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

    pub fn run(a: ReshareArgs) -> Result<i32> {
        let trace = util::Trace::new(a.verbose);
        let cwd = std::env::current_dir()?;
        let target = match &a.target {
            Some(p) => git::toplevel(&util::absolutize(p, &cwd))?,
            None => git::toplevel(&cwd)?,
        };
        let tcanon = util::canon(&target);
        let dev = fs::metadata(&target)?.dev();
        let donors: Vec<PathBuf> = match &a.from {
            Some(p) => {
                let donor = git::toplevel(&util::absolutize(p, &cwd))?;
                if util::canon(&donor) == tcanon {
                    bail!("the donor and target are the same worktree");
                }
                if fs::metadata(&donor)?.dev() != dev {
                    bail!("the donor and target must be on the same filesystem");
                }
                vec![donor]
            }
            None => git::list_worktrees(&target)?
                .into_iter()
                .filter(|w| !w.bare && !w.prunable && util::canon(&w.path) != tcanon)
                .filter(|w| fs::metadata(&w.path).is_ok_and(|m| m.dev() == dev))
                .map(|w| w.path)
                .collect(),
        };
        if donors.is_empty() {
            bail!("no other worktree on the same filesystem to share with");
        }

        let mut by_oid: HashMap<String, PathBuf> = HashMap::new();
        for d in &donors {
            if util::canon(d) == tcanon {
                continue;
            }
            for (p, oid) in clean_files(d)? {
                by_oid
                    .entry(oid)
                    .or_insert_with(|| d.join(bytes_to_path(&p)));
            }
        }
        let work: Vec<(PathBuf, PathBuf, u64)> = clean_files(&target)?
            .into_iter()
            .filter_map(|(p, oid)| {
                let src = by_oid.get(&oid)?;
                let dst = target.join(bytes_to_path(&p));
                let len = dst.symlink_metadata().ok().filter(|m| m.is_file())?.len();
                (len >= a.min_size).then(|| (src.clone(), dst, len))
            })
            .collect();
        trace.note(format!("{} candidate files with a donor copy", work.len()));

        let next = AtomicUsize::new(0);
        let deduped_files = AtomicUsize::new(0);
        let deduped_bytes = AtomicU64::new(0);
        let already = AtomicUsize::new(0);
        let differ = AtomicUsize::new(0);
        let failed = AtomicUsize::new(0);
        let stat_moved = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..util::workers(work.len()).min(4) {
                s.spawn(|| loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some((src, dst, len)) = work.get(i) else {
                        break;
                    };
                    let (Ok(sf), Ok(df)) = (open_ro(src), open_ro(dst)) else {
                        failed.fetch_add(1, Ordering::Relaxed);
                        continue;
                    };
                    let (Ok(sm), Ok(before)) = (sf.metadata(), df.metadata()) else {
                        failed.fetch_add(1, Ordering::Relaxed);
                        continue;
                    };
                    if !sm.is_file()
                        || !before.is_file()
                        || sm.len() != *len
                        || before.len() != *len
                        || sm.dev() != before.dev()
                        || sm.ino() == before.ino()
                    {
                        failed.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    if same_extents(&sf, &df) {
                        already.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    if a.dry_run {
                        deduped_files.fetch_add(1, Ordering::Relaxed);
                        deduped_bytes.fetch_add(*len, Ordering::Relaxed);
                        continue;
                    }
                    match linux::dedupe(&sf, &df, *len) {
                        Ok(DedupeOutcome::Deduped(n)) => {
                            deduped_files.fetch_add(1, Ordering::Relaxed);
                            deduped_bytes.fetch_add(n, Ordering::Relaxed);
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
                        Ok(DedupeOutcome::Differs) => {
                            differ.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => {
                            if a.verbose {
                                eprintln!("sharetree: {}: {e}", dst.display());
                            }
                            failed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            }
        });
        let verb = if a.dry_run { "would share" } else { "shared" };
        eprintln!(
            "sharetree: {verb} {} files ({}) in {} with {} donor(s); {} already shared, {} differ, {} skipped",
            deduped_files.load(Ordering::Relaxed),
            human(deduped_bytes.load(Ordering::Relaxed)),
            target.display(),
            donors.len(),
            already.load(Ordering::Relaxed),
            differ.load(Ordering::Relaxed),
            failed.load(Ordering::Relaxed)
        );
        let moved = stat_moved.load(Ordering::Relaxed);
        if moved > 0 {
            eprintln!("sharetree: note: this filesystem updated timestamps on {moved} files; the next `git status` re-hashes them once");
        }
        Ok(0)
    }
}
