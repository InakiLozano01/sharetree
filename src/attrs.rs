//! Git checkout rules for reuse. Clone only paths included in this worktree's
//! checkout whose bytes Git would write unchanged from the target blob.
//!
//! Attributes are read from the *target* commit (`check-attr --source`),
//! never from the donor's working tree.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;

use crate::git;

const ATTRS: [&str; 6] = [
    "text",
    "eol",
    "crlf",
    "ident",
    "working-tree-encoding",
    "filter",
];

/// Repository-wide line-ending policy for the new worktree.
#[derive(Clone, Copy, Debug)]
struct EolPolicy {
    /// Would git write CRLF for text files? (core.autocrlf=true or core.eol=crlf)
    crlf_output: bool,
}

fn eol_policy(worktree: &Path) -> Result<EolPolicy> {
    if let Some(autocrlf) = git::config_get(worktree, "core.autocrlf")? {
        if autocrlf.eq_ignore_ascii_case("input") {
            return Ok(EolPolicy { crlf_output: false });
        }
        // Git accepts implicit true and numeric booleans as well as words.
        if git::config_bool(worktree, "core.autocrlf")? == Some(true) {
            return Ok(EolPolicy { crlf_output: true });
        }
    }
    let eol = git::config_get(worktree, "core.eol")?;
    Ok(EolPolicy {
        crlf_output: match eol.as_deref() {
            Some("lf") => false,
            Some("crlf") => true,
            _ => cfg!(windows),
        },
    })
}

fn unsetish(v: &str) -> bool {
    v == "unspecified" || v == "unset"
}

/// Would checkout write exactly the blob bytes for a path with these values
/// of [`ATTRS`]?
fn raw_checkout(vals: &[&str; 6], policy: EolPolicy) -> bool {
    let [text, eol, crlf, ident, wte, filter] = *vals;
    if !unsetish(filter) || !unsetish(ident) || !unsetish(wte) {
        return false; // smudge filters (LFS), $Id$ expansion, re-encoding
    }
    if !unsetish(crlf) {
        return false;
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

/// Filter candidate paths using the worktree's sparse-checkout rules and
/// the target commit's checkout conversions.
pub fn reusable_paths(worktree: &Path, target: &str, paths: &[&[u8]]) -> Result<HashSet<Vec<u8>>> {
    if paths.is_empty() {
        return Ok(HashSet::new());
    }
    if git::config_bool(worktree, "core.sparseCheckout")? == Some(true) {
        let input = nul_terminated(paths);
        let out =
            git::output_with_input(worktree, &["sparse-checkout", "check-rules", "-z"], &input)?;
        let keep: HashSet<Vec<u8>> = git::split_z(&out).into_iter().collect();
        let included: Vec<&[u8]> = paths
            .iter()
            .copied()
            .filter(|p| keep.contains(*p))
            .collect();
        return raw_paths(worktree, target, &included);
    }
    raw_paths(worktree, target, paths)
}

/// Return the subset of `paths` that git checks out byte-for-byte.
fn raw_paths(worktree: &Path, target: &str, paths: &[&[u8]]) -> Result<HashSet<Vec<u8>>> {
    if paths.is_empty() {
        return Ok(HashSet::new());
    }
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
    for (out, part) in outs.iter().zip(paths.chunks(chunk)) {
        parse_rows(out, part, &mut rows)?;
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
    let source = format!("--source={target}");
    let mut args = vec!["check-attr", "-z", "--stdin", source.as_str()];
    args.extend(ATTRS);
    git::output_with_input(worktree, &args, &nul_terminated(paths))
}

fn nul_terminated(paths: &[&[u8]]) -> Vec<u8> {
    let mut input = Vec::new();
    for path in paths {
        input.extend_from_slice(path);
        input.push(0);
    }
    input
}

/// Records: <path> NUL <attr> NUL <value> NUL, ATTRS.len() per path, in order.
fn parse_rows<'a>(
    out: &'a [u8],
    paths: &[&'a [u8]],
    rows: &mut Vec<(&'a [u8], [&'a str; 6])>,
) -> Result<()> {
    let mut fields = out.split(|&b| b == 0);
    for &path in paths {
        let mut vals = ["unspecified"; 6];
        for (v, attr) in vals.iter_mut().zip(ATTRS) {
            if fields.next() != Some(path) || fields.next() != Some(attr.as_bytes()) {
                anyhow::bail!("unexpected git check-attr output");
            }
            let value = fields
                .next()
                .ok_or_else(|| anyhow::anyhow!("missing git attribute value"))?;
            *v = std::str::from_utf8(value)?;
        }
        rows.push((path, vals));
    }
    if fields.next() != Some(b"".as_slice()) || fields.next().is_some() {
        anyhow::bail!("unexpected git check-attr output");
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
    use std::fs::{self, OpenOptions};
    use std::io::Write;

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

    #[test]
    fn reuse_respects_sparse_rules_and_target_attributes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git::output(root, &["init"]).unwrap();
        git::output(root, &["config", "core.autocrlf", "false"]).unwrap();
        git::output(root, &["config", "core.eol", "lf"]).unwrap();
        fs::create_dir(root.join("include")).unwrap();
        fs::create_dir(root.join("omit")).unwrap();
        for path in ["include/plain", "include/convert", "omit/file"] {
            fs::write(root.join(path), b"line\n").unwrap();
        }
        fs::write(
            root.join(".gitattributes"),
            b"include/convert text eol=crlf\n",
        )
        .unwrap();
        git::output(root, &["add", "."]).unwrap();
        git::output(
            root,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgSign=false",
                "commit",
                "-qm",
                "test checkout rules",
            ],
        )
        .unwrap();
        git::output(root, &["sparse-checkout", "set", "--cone", "include"]).unwrap();
        // Only the target commit declares the conversion.
        fs::write(root.join(".gitattributes"), b"").unwrap();
        let paths: &[&[u8]] = &[b"include/plain", b"include/convert", b"omit/file"];
        assert_eq!(
            reusable_paths(root, "HEAD", paths).unwrap(),
            HashSet::from([b"include/plain".to_vec()])
        );
    }

    #[test]
    fn reuse_respects_git_boolean_and_input_line_endings() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git::output(root, &["init"]).unwrap();
        git::output(root, &["config", "core.eol", "lf"]).unwrap();
        let mut config = OpenOptions::new()
            .append(true)
            .open(root.join(".git/config"))
            .unwrap();
        writeln!(config, "[core]\n\tautocrlf").unwrap();
        assert!(eol_policy(root).unwrap().crlf_output);

        git::output(root, &["config", "core.autocrlf", "2"]).unwrap();
        assert!(eol_policy(root).unwrap().crlf_output);
        git::output(root, &["config", "core.autocrlf", "input"]).unwrap();
        git::output(root, &["config", "core.eol", "crlf"]).unwrap();
        assert!(!eol_policy(root).unwrap().crlf_output);
    }
}
