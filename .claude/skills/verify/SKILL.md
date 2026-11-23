---
name: verify
description: Verify Sharetree's CLI worktree creation, fallback, rollback, and cleanup behavior through its real command entry points.
---

# Sharetree CLI verification

Sharetree is a short-lived CLI. Build once, then run each command in an isolated temporary Git repository and temporary worktree directory.

## Launch

Build the binary with:

```bash
cargo build --offline --locked
```

The usable entry point is `target/debug/sharetree`. Create a temporary Git repository with one committed file before driving it. There is no long-running server.

## Doctor

Run:

```bash
target/debug/sharetree --help
```

The check is valid only when it exits 0 and prints the `add`, `rm`, `du`, and `setup` commands.

## Drive

Exercise the real CLI from the temporary repository:

```bash
target/debug/sharetree add ../worktree HEAD

git -C ../worktree status --porcelain

target/debug/sharetree rm ../worktree
```

For a filesystem that supports reflinks, compare the result with stock `git worktree add` and edit the created file. The donor and stock checkout must remain unchanged. For tmpfs, use the repository's existing fallback tests instead of claiming clone coverage.

## Evidence

Capture the command transcript, exit codes, `git status --porcelain`, resulting file bytes, and whether the worktree directory was removed. Verify side effects in Git's worktree metadata and on disk. Keep evidence under `/tmp/sharetree-verification-<timestamp>` and never delete it during cleanup.

## Cleanup

Track the exact temporary repository, worktree, and processes started by the run. Remove only those paths after capturing evidence. Do not kill by process name.

## Feature map

See `.claude/skills/verify/features/` for the supported user journeys.
