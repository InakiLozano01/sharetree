## Sub-features

- Create a worktree through `sharetree add`.
- Confirm Git status and file contents.
- Remove it through `sharetree rm`.

## How to get to it (user POV)

From a Git repository, run `sharetree add <destination> HEAD`, work in the destination, then run `sharetree rm <destination>`.

## Driving it with the CLI harness

Build `target/debug/sharetree`, create an isolated temporary Git repository, run the add and rm commands, and inspect status, bytes, and metadata as described in the parent verification skill.

## Gotchas

A destination on a filesystem without reflinks can legitimately use Git fallback. That is a different journey and must be reported separately.
