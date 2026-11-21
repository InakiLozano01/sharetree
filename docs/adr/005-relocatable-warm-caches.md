# 005 Copy only relocatable warm caches

A new worktree can use ignored output from a donor, but a full copy would spend the disk space and time `--warm` aims to save. Clone only where the filesystem supports sharing and preserve timestamps and permissions because build tools use them to decide whether to reuse outputs. The ordinary Git checkout retains precedence over anything selected for warming. Skip attempts to traverse symlinked parents or replace a path the checkout already owns. Apply the donor's directory mode only to newly created directories and roll back only the entries created by this operation if warming cannot finish.

Skip nested repositories and worktrees, `.env` files that may contain credentials, and Python virtualenvs and CMake build directories that embed absolute paths. Retarget an absolute symlink pointing inside the donor to the same relative location in the destination. Paths outside the donor retain their original target. A policy may narrow the selection while keeping these vetoes. [Warm cache usage](../../README.md#--warm-ignored-build-caches)

Cache vetoes and worktree exclusions draw on can1357/oh-my-pi, and absolute symlink retargeting draws on lukeed/lane. [Third-party notices](../../THIRD_PARTY_NOTICES)
