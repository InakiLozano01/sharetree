# 004 Construct overlays from immutable checkouts

On Linux a FUSE overlay can share a checkout on a filesystem without reflinks. Require fuse-overlayfs 1.18 or newer and a successful version probe because earlier releases could resolve hardlinked lower files to the wrong layer. A mounted overlay expects its lower layer to stay unchanged. Keep each base as a detached checkout locked against Git pruning and keep mutable state in per-worktree upper and work directories. A nearby commit can reuse an existing base when fewer than 5 percent of its files differ. Git writes that delta into the upper layer.

Place upper and work directories on the same filesystem as their base so the mounted view can report base inode numbers. Let the base index settle before copying it into a new overlay's Git registration. Wait past the written timestamps, allowing for Git builds that compare whole seconds, then refresh the base index. This gives Git stat data it can trust without copying up the whole base. The overlay's own `.git` file lives in the upper layer, so the overlay worktree registration also stays locked when unmounted.

Persist mount information so a user unit can re-establish overlays. Validate the expected mount before reusing a path. Check for local changes through the mounted view before removing it, and retain a shared base while any overlay references it. Use the smallest necessary permissions change on FUSE-created work directories before deleting them.

The first write copies an entire file into the upper layer. A rename of a lower-layer directory can return `EXDEV` and lead the caller to copy a tree. FUSE added read overhead on the measured ext4 host. See [overlay usage and measurements](../../README.md#linux-overlay-worktrees---overlay).
