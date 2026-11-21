---
name: lifecycle-state-machine
description: Verify Sharetree's worktree lifecycle transition table after changes to creation, rollback, or completion logic.
---

# Lifecycle state machine verification

Use this skill when a change touches the worktree lifecycle in `src/add.rs`, especially `Lifecycle::advance` or the creation, rollback, and completion paths.

## Property

Every lifecycle transition is accepted only when it is one of the explicit creation paths. A completed or rolled-back worktree cannot be reopened, and a planned worktree cannot skip registration.

## Check

Run from the repository root:

```bash
cargo test --offline --locked --lib add::engine::tests::lifecycle
```

Then run the complete library suite:

```bash
cargo test --offline --locked --lib
```

The focused tests enumerate valid creation and rollback paths and invalid skipped or reopened phases. The full suite checks that the lifecycle change preserves the rest of the library behavior.

## Scope

This verifies the Rust transition table and its callers under the test cases in `src/add.rs`. It does not prove filesystem behavior, process cancellation, Git's own worktree locking, or every interleaving of concurrent processes. Use the Btrfs, XFS, APFS, ReFS, and tmpfs integration fixtures for those properties.

## Maintenance

When lifecycle states or transitions change, update the focused tests and rerun this skill. The coverage was last confirmed at commit `0ef6ea90c45aa681729bae319e808c4936a81242`.
