# 006 Park worktree state before removing it

Removing a large checkout is slow even after Git's safety checks pass. Rename the checkout into trash on the same filesystem so its path becomes available immediately, then remove the parked files. Validate the Git registration first and rename its admin directory as a unit. If that step fails, putting the checkout back leaves the state usable. A later remove can resume after finding a parked checkout linked to the same admin directory. A volume alias can still identify a requested path when the volume is unmounted, so read its link before resolving the filesystem path.

Keep safety checks for the main worktree, the current directory, locked worktrees, and untracked or modified content. Clean up the alias after unregistering. Use the synchronous path when requested, when a detached sweep cannot start, or when using the Git fallback for a checkout that cannot be parked. The internal sweeper accepts only paths in sharetree's trash directories, and its Unix process group lets an asynchronous deletion survive terminal interrupts.

The park-then-sweep idea came from lukeed/lane. [Third-party notices](../../THIRD_PARTY_NOTICES)
