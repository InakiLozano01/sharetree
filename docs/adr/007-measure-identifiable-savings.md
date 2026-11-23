# 007 Measure identifiable savings

Ordinary `du` totals allocated blocks per file. It attributes a shared extent to each reflinked file, so summing those figures treats each checkout as a full copy. Report both apparent allocation and the bytes identifiable as exclusive to a worktree. Exclude its `.git` metadata. A linked worktree's Git storage is managed elsewhere.

Use the filesystem's exclusive count on Btrfs. Elsewhere on Linux, sum FIEMAP extents without `SHARED` as an estimate. FIEMAP reports logical extent lengths and does not distinguish sharing with the same worktree from sharing with another worktree or a snapshot. Sync before mapping when delayed allocation would hide extents. When the platform cannot identify shared blocks, leave exclusivity unknown. An idle-volume free-space delta can measure the change in that case. [Space accounting](../../README.md#why-du-lies-and-how-to-measure)

Alternate tools in benchmarks to reduce run-order effects. Keep the page cache warm and let Btrfs finish deletes between runs so the next timing does not inherit that work. The benchmark scripts report host load, checkout time, and, where available, first `git status` and exclusive space. [Benchmark method and results](../../README.md#benchmarks)
