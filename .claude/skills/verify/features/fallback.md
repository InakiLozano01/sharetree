## Sub-features

- Detect unsupported reflinks.
- Delegate to stock Git.
- Preserve user-visible checkout behavior.

## How to get to it (user POV)

Run `sharetree add` with a destination on tmpfs or another filesystem where native cloning is unsupported.

## Driving it with the CLI harness

Use the existing `tests/fallback.rs` fixtures with `SHARETREE_TEST_TMPFS_DIR` set to a writable tmpfs directory. Assert the checkout bytes, Git status, and cleanup.

## Gotchas

A passing process with a skipped fixture is not proof. Require one executed test and record the filesystem type.
