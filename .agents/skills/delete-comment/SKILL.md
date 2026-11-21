---
name: delete-comment
description: Audit and remove source comments in sharetree. Use when asked for /delete-comment, a comment review, or a smaller comment lint baseline.
---

# Delete comments

Run the source comment inventory from the repository root. It includes Rust doc comments, scripts, configuration and shell steps embedded in GitHub workflows.

```sh
uv run --no-project --with-requirements scripts/comment-lint-requirements.txt python scripts/comment-lint.py --list
```

Read each comment alongside its implementation and callers. Apply the matching action to every comment in scope.

| Classification | Action |
| --- | --- |
| Tautology | Delete it when the names, tests, or code already say the same thing. |
| Excuse | Delete it and file a focused issue naming the implementation, the failure, and a check for the fix. Reuse an existing issue for the same problem. |
| Stale | Verify what changed, then delete the outdated explanation. |
| Decision record | Record the constraint, choice, and consequence in the relevant [ADR](../../../docs/adr/README.md). Preserve source attribution and remove the comment. |

Preserve shebangs and other annotations consumed by tools. An annotation that papers over a fixable implementation still deserves an issue. Check that removing each token preserves the surrounding code and string literals.

Run the lint with `--write-baseline` to drop deleted entries, then run it without arguments. The baseline is a multiset of comment fingerprints scoped to each file, so it shrinks when comments are deleted and cannot be used to add comments. Verify behavior through the affected project checks. Report removed full lines, remaining full comment lines, and the issues or ADRs created. Use `--list --ref <revision>` for the starting inventory.
