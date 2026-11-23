#![cfg(unix)]

mod common;

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::{symlink, MetadataExt};
use std::path::{Path, PathBuf};

use common::{bytes, Sandbox};

type Usage = (u64, Option<u64>);

fn usage(sb: &Sandbox, cwd: &Path, args: &[&str]) -> HashMap<PathBuf, Usage> {
    let mut command = vec!["du", "--raw"];
    command.extend_from_slice(args);
    let out = sb.sharetree(cwd, &command);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|line| {
            let mut fields = line.splitn(3, '\t');
            let apparent = fields.next().unwrap().parse().unwrap();
            let exclusive = match fields.next().unwrap() {
                "-" => None,
                value => Some(value.parse().unwrap()),
            };
            (PathBuf::from(fields.next().unwrap()), (apparent, exclusive))
        })
        .collect()
}

fn ls_size(sb: &Sandbox, root: &Path) -> String {
    let out = sb.sharetree(root, &["ls"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .find_map(|line| line.strip_suffix(&format!("  {:<24} {} [main]", "main", root.display())))
        .expect("main worktree row")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn allocated(path: &Path) -> u64 {
    fs::symlink_metadata(path).unwrap().blocks() * 512
}

#[test]
fn registered_nested_worktree_is_measured_only_in_its_own_row() {
    let sb = Sandbox::home();
    let main = sb.repo("main", &[]).canonicalize().unwrap();
    let container = main.join("checkouts");
    fs::create_dir(&container).unwrap();
    let neighbor = container.join("child with spaces-neighbor");
    fs::File::create(&neighbor).unwrap();
    let child = container.join("child with spaces");
    sb.git(
        &main,
        &["worktree", "add", "-q", "--detach", child.to_str().unwrap()],
    );

    let before = usage(&sb, &child, &["--all"]);
    let before_ls = ls_size(&sb, &main);
    let extra = child.join("untracked.bin");
    fs::write(&extra, bytes(2, 4 << 20)).unwrap();
    fs::File::open(&extra).unwrap().sync_all().unwrap();
    let extra_bytes = allocated(&extra);
    assert!(extra_bytes > 0);

    let after = usage(&sb, &child, &["--all"]);
    assert_eq!(after.len(), 2);
    assert_eq!(after[&main], before[&main]);
    assert_eq!(after[&child].0, before[&child].0 + extra_bytes);
    assert_eq!(ls_size(&sb, &main), before_ls);
    assert_eq!(usage(&sb, &container, &[])[&main], after[&main]);

    let alias = sb.p("main alias");
    symlink(&main, &alias).unwrap();
    assert_eq!(
        usage(&sb, sb.root(), &["main alias"])[Path::new("main alias")],
        after[&main]
    );

    fs::write(&neighbor, bytes(1, 1 << 20)).unwrap();
    let with_neighbor = usage(&sb, &child, &["--all"]);
    assert_eq!(
        with_neighbor[&main].0,
        after[&main].0 + allocated(&neighbor)
    );
    assert_eq!(with_neighbor[&child], after[&child]);
}

#[test]
fn hardlinked_blocks_count_once_and_are_exclusive_only_within_the_scope() {
    let sb = Sandbox::home();
    let scope = sb.p("content");
    let inner = scope.join("inner");
    fs::create_dir_all(&inner).unwrap();
    let data = scope.join("data.bin");
    fs::write(&data, bytes(3, 1 << 20)).unwrap();
    let before = usage(&sb, sb.root(), &["content"])[Path::new("content")];
    let before_directory = allocated(&inner);

    fs::hard_link(&data, inner.join("alias.bin")).unwrap();
    let internal = usage(&sb, sb.root(), &["content"])[Path::new("content")];
    assert_eq!(internal.0, before.0 - before_directory + allocated(&inner));
    assert_eq!(internal.1, before.1);

    fs::hard_link(&data, sb.p("external.bin")).unwrap();
    let external = usage(&sb, sb.root(), &["content"])[Path::new("content")];
    assert_eq!(external, (internal.0, internal.1.map(|_| 0)));
}
