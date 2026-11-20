//! Checkout-conversion rules: which paths would git write differently from
//! their raw blob? Those are left to git (bale and git-twig's rule): we only
//! clone a path when the bytes git would write equal the blob's bytes.
//!
//! Attributes are read from the *target* commit (`check-attr --source`),
//! never from the donor's working tree.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;

use crate::git;

pub const ATTRS: [&str; 6] = [
    "text",
    "eol",
    "crlf",
    "ident",
    "working-tree-encoding",
    "filter",
];

/// Repository-wide line-ending policy for the new worktree.
#[derive(Clone, Copy, Debug)]
pub struct EolPolicy {
    /// Would git write CRLF for text files? (core.autocrlf=true or core.eol=crlf)
    pub crlf_output: bool,
}

pub fn eol_policy(worktree: &Path) -> Result<EolPolicy> {
    let autocrlf = git::config_get(worktree, "core.autocrlf")?
        .map(|v| v.to_ascii_lowercase())
        .unwrap_or_default();
    let eol = git::config_get(worktree, "core.eol")?
        .map(|v| v.to_ascii_lowercase())
        .unwrap_or_default();
    let autocrlf_true = matches!(autocrlf.as_str(), "true" | "yes" | "on" | "1");
    let native_crlf = cfg!(windows) && (eol.is_empty() || eol == "native");
    Ok(EolPolicy {
        crlf_output: autocrlf_true || eol == "crlf" || native_crlf,
    })
}

fn unsetish(v: &str) -> bool {
    v == "unspecified" || v == "unset"
}

/// Would checkout write exactly the blob bytes for a path with these values
/// of [`ATTRS`]?
pub fn raw_checkout(vals: &[&str; 6], policy: EolPolicy) -> bool {
    let [text, eol, crlf, ident, wte, filter] = *vals;
    if !unsetish(filter) || !unsetish(ident) || !unsetish(wte) {
        return false; // smudge filters (LFS), $Id$ expansion, re-encoding
    }
    if !unsetish(crlf) {
        return crlf == "unset";
    }
    if !(unsetish(eol) || eol == "lf") {
        return false; // eol=crlf converts LF -> CRLF
    }
    if policy.crlf_output {
        // Text files get CRLF; only explicitly binary paths are raw.
        return text == "unset";
    }
    // LF output never rewrites blob bytes (git does not turn CRLF into LF on
    // checkout), whatever `text` says.
    true
}

/// Return the subset of `paths` that git checks out byte-for-byte.
pub fn raw_paths(worktree: &Path, target: &str, paths: &[&[u8]]) -> Result<HashSet<Vec<u8>>> {
    let policy = eol_policy(worktree)?;
    // One check-attr is single-threaded (0.4 s for the kernel's 62k paths):
    // shard large batches over several processes, contiguously, so each one
    // keeps walking neighbouring directories.
    let shards = if paths.len() < 4096 {
        1
    } else {
        crate::util::workers(paths.len() / 2048)
    };
    let chunk = paths.len().div_ceil(shards).max(1);
    let outs: Vec<Result<Vec<u8>>> = std::thread::scope(|s| {
        let hs: Vec<_> = paths
            .chunks(chunk)
            .map(|part| s.spawn(move || check_attr(worktree, target, part)))
            .collect();
        hs.into_iter()
            .map(|h| h.join().expect("check-attr thread"))
            .collect()
    });
    let outs = outs.into_iter().collect::<Result<Vec<_>>>()?;
    let mut rows: Vec<(&[u8], [&str; 6])> = Vec::with_capacity(paths.len());
    for out in &outs {
        parse_rows(out, &mut rows)?;
    }
    // A `filter` attribute only converts when its driver is configured (git
    // ignores undefined, non-required drivers). Repos like git.git tag files
    // with filters most clones never define.
    let mut active: HashMap<&str, bool> = HashMap::new();
    for (_, vals) in &rows {
        let f = vals[5];
        if !unsetish(f) && !active.contains_key(f) {
            active.insert(f, filter_configured(worktree, f)?);
        }
    }
    let mut ok = HashSet::new();
    for (path, mut vals) in rows {
        if active.get(vals[5]) == Some(&false) {
            vals[5] = "unspecified";
        }
        if raw_checkout(&vals, policy) {
            ok.insert(path.to_vec());
        }
    }
    Ok(ok)
}

fn check_attr(worktree: &Path, target: &str, paths: &[&[u8]]) -> Result<Vec<u8>> {
    let mut input = Vec::new();
    for p in paths {
        input.extend_from_slice(p);
        input.push(0);
    }
    let source = format!("--source={target}");
    let mut args = vec!["check-attr", "-z", "--stdin", source.as_str()];
    args.extend(ATTRS);
    git::output_with_input(worktree, &args, &input)
}

/// Records: <path> NUL <attr> NUL <value> NUL, ATTRS.len() per path, in order.
fn parse_rows<'a>(out: &'a [u8], rows: &mut Vec<(&'a [u8], [&'a str; 6])>) -> Result<()> {
    let fields: Vec<&[u8]> = out.split(|&b| b == 0).collect();
    let mut i = 0;
    while i + 3 * ATTRS.len() <= fields.len() {
        let path = fields[i];
        let mut vals = ["unspecified"; 6];
        for (k, v) in vals.iter_mut().enumerate() {
            let rec = i + 3 * k;
            if fields[rec] != path || fields[rec + 1] != ATTRS[k].as_bytes() {
                anyhow::bail!("unexpected git check-attr output");
            }
            *v = std::str::from_utf8(fields[rec + 2]).unwrap_or("?");
        }
        rows.push((path, vals));
        i += 3 * ATTRS.len();
    }
    Ok(())
}

/// Would git run a smudge for filter driver `name`? (`required` without a
/// driver makes checkout fail; leave that to git too.)
fn filter_configured(worktree: &Path, name: &str) -> Result<bool> {
    for key in ["smudge", "process"] {
        if git::config_get(worktree, &format!("filter.{name}.{key}"))?.is_some() {
            return Ok(true);
        }
    }
    Ok(git::config_bool(worktree, &format!("filter.{name}.required"))? == Some(true))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LF: EolPolicy = EolPolicy { crlf_output: false };
    const CRLF: EolPolicy = EolPolicy { crlf_output: true };
    const U: &str = "unspecified";

    #[test]
    fn plain_files_are_raw() {
        assert!(raw_checkout(&[U, U, U, U, U, U], LF));
        assert!(raw_checkout(&["auto", U, U, U, U, U], LF));
        assert!(raw_checkout(&["set", "lf", U, U, U, U], LF));
    }

    #[test]
    fn conversions_are_delegated() {
        assert!(!raw_checkout(&["set", "crlf", U, U, U, U], LF));
        assert!(!raw_checkout(&[U, U, U, U, U, "lfs"], LF));
        assert!(!raw_checkout(&[U, U, U, "set", U, U], LF));
        assert!(!raw_checkout(&[U, U, U, U, "UTF-16", U], LF));
        assert!(!raw_checkout(&[U, U, U, U, U, U], CRLF));
        assert!(raw_checkout(&["unset", U, U, U, U, U], CRLF));
        assert!(raw_checkout(&[U, U, "unset", U, U, U], LF));
        assert!(!raw_checkout(&[U, U, "input", U, U, U], LF));
    }
}
