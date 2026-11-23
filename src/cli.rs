use std::ffi::OsString;

use crate::{add, advise, reshare, rm, setup, space};

const USAGE: &str = "\
sharetree: copy-on-write git worktrees

usage:
  sharetree add [<git worktree add options>] [--from <worktree>] [--warm]
                [--min-size <bytes>] [--no-cow] [--into-volume | --no-volume]
                [--overlay] [--windows-clone] [-v] <path> [<commit-ish>]
  sharetree rm [-f [-f]] [--sync] <worktree>
  sharetree ls [--no-size]
  sharetree du [--all] [--raw] [<path>...]
  sharetree reshare [<worktree> | --all] [--from <worktree>] [--dry-run] [--min-size <bytes>] [-v]
  sharetree doctor [<path>]
  sharetree mount [--unmount] (--all | <worktree>...)
  sharetree setup volume|devdrive|status|mount|teardown|auto ...   (`sharetree setup help`)

add      Create a worktree like `git worktree add`, reflinking every tracked file a
         donor worktree already holds. Every clone is hashed against the target blob
         before git is told it is up to date; git writes everything else. Falls back
         to plain `git worktree add` when the filesystem cannot reflink.
         --from <wt>   donor worktree (default: best of current, main, recent ones)
         --warm        also reflink ignored build caches (policy: .sharetree.toml)
         --min-size N  only reflink files of at least N bytes (default 2049)
         --no-cow      just run `git worktree add`
         --into-volume place the worktree in the sharetree volume (Linux loop image,
                       Windows Dev Drive), leaving a symlink at <path>
         --no-volume   never use the volume, even with auto_volume = true
                       (neither flag and auto_volume unset: ask on a terminal)
         --overlay     Linux, opt-in: fuse-overlayfs worktree over a shared base
                       checkout (whole-file copy-up on write, slower reads)
         --windows-clone  Windows, experimental and untested: ReFS / Dev Drive
                       block cloning (or windows_clone = true in the config)
         -v            show the donor, the plan and per-phase timings
rm       Remove a worktree: same safety checks as `git worktree remove`, then the
         directory is renamed into a trash dir and deleted in the background.
ls       List worktrees with the bytes only they hold (where measurable).
du       Show apparent size (what `du` reports) next to exclusive bytes.
reshare  Linux: re-share files git rewrote, using FIDEDUPERANGE against a donor.
         --all scans the repository's checkouts on this filesystem once.
         The kernel verifies byte equality. The Git index is read-only.
doctor   Filesystem, backend `add` would use, volume, and dependency-store advice
         (pnpm, npm, yarn, bun, uv, pip, cargo, go).
mount    Remount overlay worktrees after a reboot (the login unit runs --all).
setup    One-time copy-on-write volume for filesystems without reflinks:
         `setup volume` (Linux: sparse Btrfs/XFS image via udisks, no root on a
         desktop), `setup devdrive` (Windows: ReFS Dev Drive VHDX, admin once).

Environment: SHARETREE_TRACE=1 prints per-phase timings (same as -v);
SHARETREE_NO_ADVICE=1 silences the dependency hint after `add`;
SHARETREE_CONFIG overrides the config path; SHARETREE_NO_PROMPT=1 never asks
(for agents and scripts running in a pseudo-terminal).
Also usable as `git sharetree <command>` when git-sharetree is on PATH.
";

pub fn run() -> i32 {
    crate::util::init_umask();
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let Some(sub) = args.first().and_then(|s| s.to_str()) else {
        eprint!("{USAGE}");
        return 2;
    };
    let rest = &args[1..];
    let result = match sub {
        "add" => add::run(rest),
        "rm" | "remove" => rm::run(rest),
        "ls" | "list" => space::run_ls(rest),
        "du" => space::run_du(rest),
        "reshare" => reshare::run(rest),
        "doctor" => advise::run_doctor(rest),
        "setup" => setup::run(rest),
        #[cfg(target_os = "linux")]
        "mount" => crate::overlay::run_mount(rest),
        #[cfg(not(target_os = "linux"))]
        "mount" => Err(anyhow::anyhow!(
            "`sharetree mount` remounts overlay worktrees, which exist on Linux only"
        )),
        "__sweep" => rm::sweep_cmd(rest),
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            Ok(0)
        }
        "-V" | "--version" | "version" => {
            println!("sharetree {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        other => {
            eprintln!("sharetree: unknown command '{other}'\n");
            eprint!("{USAGE}");
            Ok(2)
        }
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("sharetree: {e:#}");
            1
        }
    }
}
