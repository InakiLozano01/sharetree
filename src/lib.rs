//! sharetree: copy-on-write git worktrees.
//!
//! The tracked layer follows the design of josharian/git-cow-worktree (MIT):
//! register with `git worktree add --no-checkout`, reflink every tracked file
//! whose blob a donor worktree already holds, hash each clone against the
//! target object id, hand git a v2 index carrying the clones' real stat data,
//! and let `git checkout -f HEAD` write whatever is left. See README.md and
//! THIRD_PARTY_NOTICES.

// Outside unix, parts of the engine (warm caches, re-sharing, volumes) are
// compiled out; on Windows the clone engine runs only with --windows-clone.
#![cfg_attr(not(unix), allow(dead_code))]

pub mod cli;

mod add;
mod advise;
mod attrs;
mod backend;
mod config;
mod devdrive;
mod git;
mod hash;
mod index;
mod ls;
mod reshare;
mod rm;
mod setup;
mod space;
mod util;
mod volume;
mod winclone;

#[cfg(unix)]
mod reflink;
#[cfg(windows)]
#[path = "reflink_windows.rs"]
mod reflink;
#[cfg(unix)]
mod warm;

#[cfg(target_os = "linux")]
mod linux;
mod overlay;
