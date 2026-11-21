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
mod reshare;
mod rm;
mod setup;
mod space;
mod util;
mod volume;
mod winclone;

#[cfg(any(unix, windows))]
mod reflink;
#[cfg(unix)]
mod warm;

#[cfg(target_os = "linux")]
mod linux;
mod overlay;
