# sharetree

Copy-on-write git worktrees. `sharetree add` makes the same worktree as
`git worktree add`, but every tracked file that another worktree already holds
is **reflinked** (the new file shares the existing file's disk blocks) instead of
written again. A new worktree of the Linux kernel costs about 4 MiB of disk
instead of 385 MiB.

```console
$ sharetree add -b fix ../linux-fix
Preparing worktree (new branch 'fix')
sharetree: reflinked 62408 of 97371 files (1.5 GiB) from /src/linux; git wrote the other 34963
```

- **Drop-in.** Git still owns registration, branches, locks and hooks. The
  result has the same files, modes, symlinks, HEAD, index and `git status` as
  `git worktree add`, and the test suite checks this.
- **Verified.** Every cloned file is hashed (collision-detecting SHA-1 as in
  git, or SHA-256) against the target blob before git is told it is up to
  date.
- **Safe fallback.** If the filesystem cannot reflink, or anything is unusual,
  sharetree runs plain `git worktree add` and says why.
- **No hardlinks, ever.** With reflinks a write in one worktree never reaches
  another; with hardlinks it would.
- **ext4 and NTFS too.** These filesystems can't share blocks, so
  `sharetree setup volume` creates a copy-on-write volume inside a file (no root
  on a desktop session), and worktrees placed in it share blocks; `add` asks
  whether to use it. Opt-in alternative: `--overlay` (fuse-overlayfs). See
  [Filesystems without reflinks](#filesystems-without-reflinks-ext4-ntfs).

## Install

**Release binaries** will be available from
[GitHub Releases](https://github.com/InakiLozano01/sharetree/releases) after the
first tag ships. The workflow builds Linux x86_64/aarch64 static musl,
macOS x86_64/arm64, and Windows x86_64 archives. These commands verify and
install a Linux or macOS archive once it is available.

```sh
v=0.1.0 target=x86_64-unknown-linux-musl   # or aarch64-unknown-linux-musl, aarch64-apple-darwin, ...
curl -LO "https://github.com/InakiLozano01/sharetree/releases/download/v$v/sharetree-v$v-$target.tar.gz"
curl -LO "https://github.com/InakiLozano01/sharetree/releases/download/v$v/sharetree-v$v-$target.tar.gz.sha256"
sha256sum -c "sharetree-v$v-$target.tar.gz.sha256"     # macOS: shasum -a 256 -c
tar xzf "sharetree-v$v-$target.tar.gz"
install -m755 "sharetree-v$v-$target"/{sharetree,git-sharetree} ~/.local/bin/
```

**Arch Linux (AUR):** Package templates for `sharetree-bin` (release binaries)
and `sharetree` (source) live in [`packaging/aur/`](packaging/aur). They still
need the first release's checksums and an Arch package build.

**From source** (stable Rust ≥ 1.89; sharetree is not on crates.io yet):

```sh
cargo install --locked --git https://github.com/InakiLozano01/sharetree   # installs sharetree and git-sharetree
cargo install --locked --path .                                            # from a checkout
```

Both binaries are the same program. With `git-sharetree` on `PATH`,
`git sharetree add ...` works too. If you install only `sharetree`, a symlink
or an alias works as well: `ln -s "$(command -v sharetree)"
~/.local/bin/git-sharetree`, or `git config --global alias.sharetree
'!sharetree'`.

Needs git ≥ 2.40 (`check-attr --source`).

**Releasing** (maintainers): push a `v*` tag; `.github/workflows/release.yml`
runs fmt, clippy and the tests, builds the five targets, and attaches the
archives and their `.sha256` files to the release. Then update `pkgver` and
the checksums in both PKGBUILDs and regenerate `.SRCINFO`.

## Usage

```
sharetree add [<git worktree add options>] [--from <worktree>] [--warm]
              [--min-size <bytes>] [--no-cow] [--into-volume | --no-volume]
              [--overlay] [--windows-clone] [-v] <path> [<commit-ish>]
sharetree rm [-f [-f]] [--sync] <worktree>
sharetree ls [--no-size]
sharetree du [--all] [--raw] [<path>...]
sharetree reshare [<worktree> | --all] [--from <worktree>] [--dry-run] [--min-size <bytes>]
sharetree doctor [<path>]
sharetree mount [--unmount] (--all | <worktree>...)
sharetree setup volume|devdrive|status|mount|teardown|auto ...
```

**`add`** takes every `git worktree add` option (`-b`, `-B`, `--detach`,
`--lock`, `--reason`, `-f`, `--track`, `--guess-remote`, `-q`, ...).

| Option | Effect |
|---|---|
| `--from <wt>` | Clone from this worktree. By default sharetree picks the best of: the current worktree, the main one, and the most recently used others. |
| `--warm` | Also reflink ignored build caches from the donor. See below. |
| `--min-size N` | Reflink only files of at least N bytes. Default 2049; see "How it works". |
| `--no-cow` | Just run `git worktree add`. |
| `--into-volume` | Put the worktree in the sharetree volume and leave a symlink at `<path>`. See below. |
| `--no-volume` | Never use the volume, even with `auto_volume = true`. |
| `--overlay` | Linux, opt-in: a fuse-overlayfs worktree over a shared base checkout. See [Overlay worktrees](#linux-overlay-worktrees---overlay). |
| `--windows-clone` | Experimental ReFS / Dev Drive block cloning on Windows (also `windows_clone = true` in the config). See [Windows](#windows-block-cloning-experimental). |
| `-v` | Show the donor, the plan and how long each step took. `SHARETREE_TRACE=1` does the same. |

**`rm`** applies `git worktree remove`'s checks. It refuses a worktree with
modified or untracked files without `-f`, a locked one without `-f -f`, and
the main worktree. It also refuses when your shell is inside the worktree.
Then it renames the directory into `.git/sharetree-trash/`, drops git's
registration, and deletes the files in the background (`--sync` waits).
Branches are kept, as with git.

**`ls`** lists worktrees with their exclusive bytes. **`du`** shows apparent
size next to exclusive bytes; see "Why `du` lies".

**`doctor`** shows the worktree's filesystem, whether reflinks work there,
which backend `add` would pick, the volume's state, and the dependency advice
below in full.

After `add`, sharetree prints a short **dependency hint** when it finds
lockfiles. It covers pnpm, npm, Yarn, bun, uv, pip/poetry, cargo and go, and
says which link mode their stores will use on this filesystem: clone where
reflinks work; hardlink or copy where they don't. Examples:
`package-import-method=clone` for pnpm on Btrfs; checking that the uv cache
is on the same filesystem on ext4; sccache with `SCCACHE_BASEDIRS` for cargo.
sharetree never recommends a shared cargo `target-dir`, because cargo holds an
exclusive lock on the build dir for the whole build, so parallel worktree
builds would wait. `-q` or `SHARETREE_NO_ADVICE=1` silences the hint.

**`reshare`** (Linux) gets sharing back in a worktree where git rewrote files:
one made by plain `git worktree add`, or after a `git switch` away and back. It
asks the kernel to deduplicate (`FIDEDUPERANGE`) each unchanged tracked file
against a donor copy of the same blob. The kernel compares the bytes itself and
shares only if they are equal, so content cannot change. Sharetree reads Git's
index without rewriting it. Filesystems that update file timestamps during
deduplication can make the next `git status` rehash those files once.

Run `sharetree reshare --all` to scan the repository's worktrees on the current filesystem once and
deduplicate eligible files by device, blob ID and file size. Each target
can try up to four donor copies. `--all` takes no worktree argument or
`--from`. Use `--dry-run` to
count candidates before the kernel checks byte equality. Reported shared
bytes describe file ranges. See [space accounting](#why-du-lies-and-how-to-measure)
for physical savings.

### `--warm`: ignored build caches

`--warm` reflinks ignored paths (`node_modules`, `target/`, ...) from the donor,
so the first build in the new worktree starts warm. It is opt-in because caches
**diverge**: the first build rewrites much of `target/`, and from then on each
worktree holds its own copy (oh-my-pi issue #15475). `sharetree ls` shows that
cost growing.

These rules are fixed:

- **Reflink only.** If the filesystem refuses, nothing is copied at all.
- **Never cloned:**
  - directories holding `pyvenv.cfg` or `CMakeCache.txt` (they contain
    absolute paths);
  - untracked `.env*` files;
  - nested repositories;
  - other worktrees and the destination itself.
- **Absolute symlinks** that point into the donor are rewritten to point into
  the new worktree.
- **mtimes and modes are kept.** Build tools compare them.

A `.sharetree.toml` file, in the target commit or in the donor, narrows the
selection:

```toml
[warm]
clone = ["node_modules", "target/release"]  # default: every ignored path
skip  = ["*.log", "target/debug/incremental"]
```

A pattern without `/` matches a name at any depth. A pattern with `/` is
anchored at the worktree root. `*` and `?` match within one path component.

For Rust, sharing compiled output usually beats cloning `target/`: use sccache
with `SCCACHE_BASEDIRS` set to your worktrees' parent. For JavaScript, pnpm's
`packageImportMethod: clone` makes `node_modules` itself copy-on-write.

## How it works

`sharetree add` follows josharian/git-cow-worktree's recipe, with git-twig's
donor scoring:

1. **Preflight.** These cases go straight to plain `git worktree add`:
   - `--no-checkout` or `--orphan`;
   - `core.splitIndex`, or an explicit Git repository, worktree, index or object-store override;
   - a `reference-transaction` or `post-index-change` hook;
   - no donor on the same device;
   - Windows, unless `--windows-clone` / `windows_clone = true`.
2. **Probe.** sharetree reflinks a 4 KiB temporary file next to the
   destination. It does not trust filesystem names: XFS can be made without
   reflink support, OpenZFS gates its block cloning behind a setting, and
   overlayfs depends on the filesystem underneath.
3. **Register.** It runs `git worktree add --no-checkout <your args>` and
   ranks donors by how many target files they can supply by blob ID. The
   final ranking excludes donor paths with local edits. A worktree with
   mostly modified files is likely still being populated and is skipped.
4. **Plan.** A target file is cloned only when all of these hold:
   - it is a regular file of at least `--min-size` bytes;
   - a donor file with the same blob is unmodified (matched by blob id, so
     renamed files count);
   - it is inside the sparse-checkout cone;
   - **git would write the blob's bytes unchanged.** That rules out files
     whose `.gitattributes` (read from the target commit) or `core.autocrlf`
     turn LF into CRLF, and files with `ident`, `working-tree-encoding`, or a
     `filter` whose driver is configured, such as LFS. Those are left for git
     to write.
5. **Clone.** One `FICLONE` per file (`fclonefileat` on macOS,
   `FSCTL_DUPLICATE_EXTENTS_TO_FILE` on Windows) on up to 8 threads. Each new
   file is created with git's mode (0666 or 0777, minus the umask). The donor
   is checked again after cloning, and the clone is thrown away if the donor
   changed meanwhile.
6. **Verify.** Up to 16 threads hash *each clone's own bytes* in-process:
   collision-detecting SHA-1 (sha1-checked, the sha1dc algorithm git ships)
   or SHA-256, by the repository's object format. Size, type and exec bit must
   match too, and the stat data must not move while the file is read. Any
   mismatch is deleted.
7. **Seed the index.** sharetree writes a version 2 index listing only the
   verified files, with their real stat data. Git can leave those files in place
   and preserve their shared blocks.
8. **Finish.** `git reset --hard --no-recurse-submodules` writes the remaining
   files, as in `git worktree add`. If Git rejects the seeded index, sharetree
   retries with an empty index. A failed population rolls back the worktree
   registration. `git hook run post-checkout` then gets the stock arguments. A
   failing hook returns its exit code and leaves the completed worktree in place.

**Why the clones are re-read.** A reflinked file starts with an empty page
cache, even when the donor is cached, so hashing it reads the data from disk
again (and decompresses it on compressed Btrfs). Hashing the donor instead
would be faster, but it trusts the filesystem's clone ioctl to copy exactly
those bytes. That has failed in practice (OpenZFS 2.2's block-cloning
corruption, ReFS zero-filled clones in git-lfs #6312), so sharetree verifies
the clone itself. See [Benchmarks](#benchmarks) for what was tried to hide
the cost.

**Why `--min-size` defaults to 2049.** Btrfs stores small files inside its
metadata ("inline"). Those can't share blocks, so cloning them saves nothing.
On compressed Btrfs, files of a few KiB that compress well are stored inline
too.

[Architecture decision records](docs/adr/README.md) explain the constraints
behind checkout verification and the other worktree backends.

## Why `du` lies, and how to measure

`du` adds up each file's blocks and cannot tell that two files share them. Ten
reflinked worktrees look like ten full copies. **Exclusive** bytes estimate
the size of file extents with a single reference. Extents shared by files
within the same worktree can also be freed when the entire worktree is removed.

- **Btrfs:** `btrfs filesystem du -s <paths>` prints Total, Exclusive and Set
  shared, using the FIEMAP ioctl. Pass the worktree's top-level entries, not
  the main worktree's `.git`. These are *uncompressed* sizes, and files stored
  inline in metadata are not counted.
- **Any filesystem:** compare free space (`df`) before and after creating a
  worktree. Run `sync` first and wait, on an otherwise idle filesystem. This
  is the ground truth.
- **XFS** has no `btrfs fi du` equivalent. The FIEMAP ioctl flags every
  extent another file also references (`FIEMAP_EXTENT_SHARED`), so the sum of
  the *unshared* extents measures their exclusive bytes.
  `xfs_io -c fiemap -v <file>` shows the flags by hand.
- **`sharetree ls` / `sharetree du`:** `btrfs filesystem du` on Btrfs; on
  XFS and other Linux filesystems with FIEMAP, the unshared-extent sum
  above. That sum is exact for extents shared between worktrees, but it
  counts whole blocks (a 10-byte file is 4 KiB), and an extent shared by two
  files *inside the same worktree* counts as shared although deleting the
  worktree would free it. Where FIEMAP is missing (tmpfs, NFS) it prints `?`.

The volume tests check both against the ground truth: in an XFS volume, a
second worktree of the test repository (594 KiB apparent) moved free space
by 8 KiB, and `sharetree du` reported under 64 KiB exclusive.

Measured on the Linux kernel tree below:

| Tool | `du` | `btrfs fi du` Exclusive | `df` delta |
|---|---|---|---|
| `git worktree add` | 1.8 GiB | 1.59 GiB | 385 MiB (zstd-compressed) |
| `sharetree add` | 1.8 GiB | 0 B | 4 MiB (metadata and inline files) |

## Filesystems without reflinks (ext4, NTFS)

ext4 and NTFS can't share blocks between files, and no setting changes that.
sharetree's answer is a **volume**: a copy-on-write filesystem inside a file.
Worktrees placed in it go through the normal reflink path.

### Linux: `sharetree setup volume`

```console
$ sharetree setup volume              # sparse Btrfs image, sized 30 % of free space (max 200 GiB)
sharetree: created a 100.0 GiB btrfs image at ~/.local/share/sharetree/volume.img (sparse: 4.6 MiB allocated now)
sharetree: attached ~/.local/share/sharetree/volume.img as /dev/loop1
sharetree: installed and enabled the systemd user unit sharetree-volume-default.service (re-mounts at login)
sharetree volume 'default' is mounted at /run/media/me/sharetree
$ sharetree add --into-volume -b fix ../fix
sharetree: creating the volume's seed checkout /run/media/me/sharetree/app-1a2b3c4d/.sharetree-seed (once per repository; later worktrees share its blocks)
sharetree: reflinked 2276 of 4876 files (46.6 MiB) from .../.sharetree-seed; git wrote the other 2600
sharetree: the worktree lives in volume 'default' at /run/media/me/sharetree/app-1a2b3c4d/fix (symlink: ../fix)
```

What `setup volume` does (`--print` shows each command first and changes
nothing):

1. **Image.** Creates a sparse image (`--size`, `--image`). On a Btrfs host
   it is made NOCOW before the first write.
   - **Btrfs** (default): `mkfs.btrfs --rootdir <empty dir>`, so the root
     directory belongs to you, not root.
   - **XFS** (`--fs xfs`, needs `mkfs.xfs` from xfsprogs): `-m reflink=1`
     and a protofile that sets the root's owner. Tested like Btrfs (setup,
     seed, reflinked worktrees, `du`, teardown).
   - An existing Btrfs or XFS image passed with `--image` is adopted, not
     reformatted.
2. **Mount without root.** It checks polkit with `pkcheck`, then runs
   `udisksctl loop-setup` and `udisksctl mount`. Polkit allows both for the
   active desktop session (`allow_active=yes`). If an automounter such as
   udiskie mounts the volume first, sharetree uses that mount.
3. **Re-mount at login.** It installs and enables
   `~/.config/systemd/user/sharetree-volume-<name>.service`, which runs
   `sharetree setup mount`. Polkit grants the udisks actions to user services
   through the user's session (checked with `pkcheck` from a
   `systemd-run --user` unit). The unit retries if the session isn't active
   yet. `--no-unit` skips it.
4. **No polkit grant** (SSH, headless): sharetree prints the one-time root
   commands (an fstab line with `loop,nofail,discard`, mounted at
   `/mnt/sharetree-$USER`) and records that mountpoint. It never runs sudo.
5. **Record.** The volume is written to `$XDG_CONFIG_HOME/sharetree/config.toml`
   (`SHARETREE_CONFIG` overrides the path).

**Where worktrees go.** A reflink can't cross filesystems, so the donor must
also live in the volume. On first use, sharetree creates a **seed** for each
repository: a detached worktree at `<mount>/<repo>-<hash>/.sharetree-seed`.

- The seed is one full checkout, made by git with hooks off. It is locked
  with the reason `sharetree seed: ...`.
- Before each `add` into the volume, sharetree moves the seed (if it has no
  local changes) to the target commit. git rewrites only the files that
  differ, and the new worktree then shares nearly every file with the seed.
- Worktrees go next to the seed, at `<mount>/<repo>-<hash>/<name>`. A symlink
  is left at the path you asked for.
- Each worktree in the volume is **locked** (`sharetree volume '<name>': ...`).
  Otherwise `git gc` would prune its registration while the volume is
  unmounted, and git would report "not a git repository" until it's back.
- `sharetree rm` removes these worktrees without `-f -f` and also removes the
  symlink. It refuses while the volume is unmounted, and it refuses to remove
  the seed (`setup teardown` does that).

The main checkout stays where it is, on ext4.

**When the volume is used.** `sharetree add` probes the destination first. If
reflinks work there, the volume is never used automatically. If they don't:

- with `--into-volume`, or `auto_volume = true`, the worktree goes into the
  volume;
- with `--no-volume`, or `auto_volume = false`, git writes a full checkout;
- if you have not chosen yet (no flag, `auto_volume` set neither for the
  repository nor globally), sharetree **asks** when stdin and stderr are a
  terminal:

  ```console
  sharetree: /home/me/src (ext4) cannot share blocks between files, so git would write a full checkout there.
  sharetree: volume 'default' can hold this worktree instead (it shares blocks with a seed checkout; a symlink is left at /home/me/src/fix).
  Use volume 'default'? [y]es this time / [a]lways / [n]ot this time / ne[v]er [n]:
  ```

  "always" and "never" are saved as this repository's `auto_volume` in the
  config. Enter means "not this time". Without a terminal (scripts, CI) or
  with `SHARETREE_NO_PROMPT=1` (for agents that run in a pseudo-terminal),
  nothing is asked: git writes a full checkout and sharetree prints how to
  choose.

`sharetree setup auto on|off [--repo]` sets the preference, and `sharetree
setup auto ask [--repo]` clears it.

If you asked for the volume and it can't be mounted, `add` fails instead of
silently writing a full checkout. A
destination you place inside the volume yourself also gets the seed and the
lock.

**Other subcommands:**
- `sharetree setup status`: image cap vs. allocated, mount state, usage, Btrfs
  bytes still waiting for async discard, seeds, login unit.
- `sharetree setup mount`: mount the volume now (the unit runs this).
- `sharetree setup teardown [--delete-image] [--force]`: unmount, detach,
  remove the unit, and drop the volume from the config.
  - With `--delete-image` it removes the seeds and deletes the image. It
    refuses while your own worktrees live in the volume, unless `--force`.
  - Without it, the image and any worktrees in it are kept and stay
    registered; `setup volume --image <it>` adopts the image again.
  - Symlinks that `add` left outside the volume are not tracked, so a
    `--force` teardown leaves them dangling.

**Measured** on this host: git.git (4,876 files) in a repository on an ext4
loop image, with `sharetree add` and then `--into-volume`. The demo script is
not part of this repo.

| | ext4 used / image growth | `btrfs fi du` Exclusive |
|---|---|---|
| plain worktree on ext4 (`sharetree add` falls back) | +62.2 MiB | — |
| seed + first worktree in the volume | +70 MiB of image | 12 KiB each |
| each further worktree in the volume | +11–12 MiB of image | 12 KiB |

The 11–12 MiB is Btrfs metadata, plus the 2,600 files under 2 KiB that Btrfs
stores inline in metadata, where they can't be shared. With async discard,
space freed inside the volume flows back to the host after a few minutes.

**Costs:**
- The image has a fixed maximum size. It is sparse, so unused space costs
  nothing.
- The loop device adds a block layer.
- Worktrees exist only while the volume is mounted.
- The symlinked path differs from the path `git worktree list` shows.

### Linux: overlay worktrees (`--overlay`)

An opt-in alternative to the volume, for when a loop image is not wanted:
`sharetree add --overlay <path> [<commit-ish>]` (any `git worktree add`
options too) mounts a **fuse-overlayfs** worktree at `<path>`. It needs
`fuse-overlayfs` 1.18 or later (includes the fix for CVE-2026-77478), `fusermount3`
and `/dev/fuse`; no root.

- **Lower layer:** one immutable *base* checkout per commit, a detached,
  locked worktree under
  `~/.local/share/sharetree/overlay/<repo>-<hash>/bases/<commit>`. The overlay
  documentation forbids changing a lower layer while it is mounted, so the
  base is never your main worktree and nothing writes to it. A new commit
  reuses the closest base when under 5 % of the files differ (git writes the
  differences into the upper dir), otherwise it gets its own base.
- **Upper and work dirs** per worktree under `.../wt/<name>/`, on the same
  filesystem as the base, so fuse-overlayfs reports the base's inode numbers.
  The worktree's `.git` file lives in the upper dir.
- **Index seeded from the base:** the base's index is settled (refreshed
  after the clock passed its files' mtimes) and copied, so stat data match
  through the overlay. Nothing is copied up and the first `git status` needs
  no rehash. `git checkout -f HEAD` then fixes whatever differs.
- **Locked** with the reason `sharetree overlay '<name>'`: while unmounted
  the path has no `.git`, and `git gc` would otherwise prune it.
- **Remount:** mounts do not survive a reboot or logout. `sharetree mount
  --all` remounts every overlay (`--unmount` the reverse). The first
  `--overlay` add installs and enables the systemd user unit
  `sharetree-overlays.service`, which runs it at login.
- **`sharetree rm`** applies the usual checks in the mounted view, unmounts,
  deletes upper and work (fixing the `d---------` work dirs first), drops the
  registration, and removes the base once no overlay uses it. The unit goes
  with the last overlay.

**Costs** (measured in the research notes on git.git over ext4):

- the first write to a file copies **the whole file** into the upper dir,
  even if a program only opens it read-write;
- renaming a directory that comes from the base fails with `EXDEV`, and
  `mv` falls back to **copying the tree** (8.5 MiB for `Documentation/`);
- reads go through FUSE: `git grep`, `find` and reading every file were
  **3–6x slower** than on plain ext4;
- the worktree exists only while mounted.

Use the volume for heavy builds. Overlay creation, remounting and removal
are tested end to end on Linux with fuse-overlayfs.

### Windows: `sharetree setup devdrive`

`sharetree setup devdrive [--size 100G] [--letter Q] [--vhdx <path>]` generates
a PowerShell script and runs it elevated (one UAC prompt). `--print` shows it
without running it. The script:

- checks for administrator rights and Windows 11 build 22621.2338 or later.
  It warns below 24H2 (build 26100), where Dev Drive has no block cloning;
- creates a dynamically expanding VHDX of at least 50 GB with `diskpart`
  (`New-VHD` needs Hyper-V, which Home editions lack). The size is a cap; the
  file grows as data is written;
- attaches it, creates a GPT partition, and runs `Format-Volume -DevDrive`;
- registers the logon task `sharetree-devdrive`, which runs as SYSTEM and
  re-attaches the VHDX, since Windows doesn't re-attach VHDs at boot. Its
  command is embedded encoded, and printed decoded in the script header.

`add --into-volume` and `auto_volume` then put worktrees in `Q:\sharetree\`.
Without `--windows-clone` these are full checkouts that get Dev Drive's
faster I/O. The script generator is unit-tested on every platform, but the
script has **not been run** on Windows.

`setup teardown --delete-image` checks the attached image for managed
worktree data before deleting it. Use `--force` to delete a detached image
or an image that still holds worktree data.

### Windows block cloning (experimental)

**Off by default.** `sharetree add --windows-clone ...`, or `windows_clone =
true` in `%APPDATA%\sharetree\config.toml`, runs the same engine as on Linux
with `FSCTL_DUPLICATE_EXTENTS_TO_FILE`, and prints a note saying it is
experimental. It follows Microsoft's block-cloning rules and the research
notes:

- the volume must report `FILE_SUPPORTS_BLOCK_REFCOUNTING` (ReFS; a Dev
  Drive clones only from Windows 11 24H2), and the probe clones a 64 KiB file
  and reads it back;
- each donor is opened denying other writers and **flushed with
  `FlushFileBuffers` before cloning**: cloning data still in the write cache
  produced permanently zero-filled files (git-lfs #6312), and a worktree tool
  clones exactly such freshly written files;
- the destination gets the donor's integrity-stream setting (and sparse flag
  when the donor is sparse), is sized with `SetEndOfFile` first, and is
  cloned in cluster-aligned regions of under 4 GiB each;
- the seeded index uses Git for Windows' stat semantics: ctime is the
  creation time, mtime the last-write time (100 ns resolution), dev, ino, uid
  and gid are 0, and there is no exec bit;
- every clone is still hashed against its blob before it enters the index.

Native Windows CI checks checkout contents, clean Git status, and edit
isolation with `--no-cow` and `--windows-clone` on the runner's temporary
filesystem. The smoke test accepts a plain Git fallback. Region splitting,
alignment, and FILETIME-to-index stat mapping have unit tests on every
platform.

The Windows ReFS workflow provisions a disposable ReFS volume with 4 KiB
allocation units on a GitHub-hosted Windows Server 2025 runner. It requires
the regression below to pass as evidence of Server ReFS block cloning and
Git-equivalent checkout behavior. Windows 11 Dev Drive setup, beyond-EOF
behavior, and physical extent sharing require separate verification.

To run the ReFS regression, set `SHARETREE_TEST_REFS_DIR` to an existing,
writable directory on a ReFS volume with block cloning enabled. The test
creates and cleans up a temporary repository and both worktrees inside that
directory using your normal user account. In PowerShell, run

```powershell
$env:SHARETREE_TEST_REFS_DIR = 'D:\path\to\existing-directory'
cargo test --locked --test windows refs_worktree_uses_verified_clones_and_keeps_edits_isolated -- --ignored --exact
```

The regression checks the filesystem type, requires one verified clone,
compares the checkout and index with stock Git, and checks that editing the
clone leaves the donor unchanged.

With Git for Windows' default `core.autocrlf = true`, Git writes files that
need checkout conversion. Binary or `-text` files can be cloned.

## Platform matrix

| Platform | Filesystem | `add` | `--warm` | `reshare` | `ls` / `du` exclusive |
|---|---|---|---|---|---|
| Linux | Btrfs | reflink (tested) | reflink | FIDEDUPERANGE (tested) | `btrfs filesystem du` |
| Linux | XFS (`reflink=1`) | reflink (tested, in an XFS volume) | reflink | FIDEDUPERANGE | FIEMAP unshared extents (tested) |
| Linux | bcachefs, overlayfs on a reflink filesystem | reflink if the probe succeeds (untested here) | reflink | FIDEDUPERANGE | FIEMAP estimate |
| Linux | OpenZFS ≥ 2.2 with block cloning on | reflink if the probe succeeds; an EAGAIN on dirty data means git writes that file (untested here) | reflink | depends on the kernel | FIEMAP estimate |
| Linux | ext4, tmpfs, NFS, ... | plain `git worktree add` plus a hint (tmpfs and ext4 tested) | nothing copied | no | FIEMAP estimate or n/a |
| Linux | ext4 etc. **with `sharetree setup volume`** | `--into-volume` / `auto_volume` / the prompt: reflink inside a Btrfs or XFS loop image, with a seed donor (both tested, including ext4) | reflink inside the volume | FIDEDUPERANGE inside the volume | `btrfs filesystem du` / FIEMAP |
| Linux | ext4, tmpfs **with `--overlay`** | fuse-overlayfs over a base checkout (tested on Linux) | — | no | upper dir size |
| macOS | APFS | `fclonefileat` per file, then flags and xattrs stripped | reflink | not yet | not measurable (use `df`) |
| Windows | NTFS | plain `git worktree add` plus a hint | — | no | not measurable |
| Windows | ReFS Dev Drive (**`sharetree setup devdrive`**) | Plain checkout with `--into-volume`, block cloning with `--windows-clone` (ReFS execution pending) | — | no | not measurable |

Native macOS CI sets `SHARETREE_REQUIRE_APFS=1`. The CLI identity test requires
an APFS fixture, 15 verified clones, and a checkout equivalent to stock Git.
The separate xattr regression requires a working clone probe and verifies that
the clone strips extended attributes. Local runs with the gate unset can skip
these tests when their fixtures lack filesystem support.

Hosted Linux CI uses separate Btrfs and XFS loop images to check verified
clones, stock Git parity, and write isolation. The Btrfs job also checks
exclusive bytes and warm-cache rollback. The XFS job uses a separate tmpfs
mount to check stock Git fallback within tmpfs and from XFS to tmpfs, and
verifies that `--warm` leaves caches behind on tmpfs.

The dependency advisor (`doctor`, and the hint after `add`) works everywhere.
sharetree has no ProjFS or hardlink backend: hardlinked tracked files leak
edits between worktrees under VS Code's and Vim's default save paths.

## Benchmarks

Host: Linux 7.2, 12 threads, git 2.56.0, shallow clones of git/git (4,876
files, 51 MB) and torvalds/linux (97,371 files, 1.68 GB). Scripts:
[`bench/compare.sh`](bench/compare.sh) (alternates tools, prints each run
with the load average) and [`bench/bench.sh`](bench/bench.sh) (adds the
first `git status` and exclusive bytes). "old" is the previous version of
this code (clone everything, then `git hash-object` in 8 processes, then
check out); "new" is the first published source revision, 871ca1b.

The comparison script accepts one argument per program with `label=program`.
Use `--tool` to pass arguments or environment values containing spaces.
Inside a `--tool` group, write `--tool ''` to pass a literal `--tool` argument.
Start the next group with `--tool <label> <program>`.

```sh
bench/compare.sh ~/src/linux 5 --tool stock git \
  --tool sharetree env SHARETREE_NO_PROMPT=1 '/path/to/sharetree build'
```

**Controlled: XFS image on tmpfs** (no disk, no compression, no encryption;
median of 5 alternating runs):

| Repo | `git worktree add` | sharetree old | sharetree new |
|---|---|---|---|
| git/git | 0.24 s | 0.45 s | 0.44 s |
| linux | 5.81 s | 4.08 s | **3.58 s** |

**Real disk: Btrfs `/home`** (`compress=zstd:3` on dm-crypt), median of 5
(git/git: 10) alternating runs. The host was busy with other work during
every run (load average 6–10, I/O pressure 17–45 %), so single runs varied
by up to 2x; treat these as rough:

| Repo | `git worktree add` | sharetree old | sharetree new | Exclusive: git / sharetree (`btrfs fi du`) |
|---|---|---|---|---|
| git/git | 0.23 s | 0.84 s | 0.89 s | 47.9 MiB / 8 KiB |
| linux | 7.72 s | 9.59 s | **7.31 s** | 1.59 GiB / 0 B |

The first `git status` after `sharetree add` took 0.04 s on git/git and
0.16–0.47 s on linux (git's own worktree: 0.01–0.38 s and 0.2–0.7 s): git
treats files it wrote moments before its index as racily clean and hashes
them again, which doesn't happen to the clones.

What changed, and what was measured and dropped:

- **Kept:** hashing in-process (sha1dc-equivalent SHA-1 / SHA-256) on up to
  16 threads with sequential readahead, instead of `git hash-object`
  processes; `git check-attr` sharded over up to 8 processes (linux: plan
  0.4–0.7 s → 0.15 s); parallel checkout for the files git writes.
- **Dropped, slower or no faster:** hashing each clone as soon as it is made
  (on XFS the clones themselves slowed down: the last clone came at 2.1–3.0
  s instead of 1.5 s); letting `git checkout-index` write the non-cloned
  files while the clones are hashed (on Btrfs a median of 8.5 s with it vs
  8.9 s without, within this host's noise; on XFS it slowed hashing by as
  much as it saved, 3.9 s vs 3.6 s);
  `POSIX_FADV_WILLNEED` on every clone (it queued whole files from all
  hashers at once).
- **Not done, on purpose:** skipping or replacing the clone's hash with a
  hash of the donor. See "Why the clones are re-read" above.

Where the time goes now (linux, Btrfs): about 1.1–1.5 s cloning, 5–6 s
reading the 1.5 GiB of clones back to hash them (decompression and
decryption bound), about 1 s for git to write the 35k small files. On
git/git most of the 0.4–0.9 s is the same cold read of 46 MiB of clones and
a final `git checkout` that sometimes takes 0.3 s for no work.

## Limitations and roadmap

- **Not yet:**
  - running the opt-in Windows regression on a real ReFS volume;
  - kernel overlayfs in a user namespace (`sharetree exec`), ProjFS, and an
    opt-in hardlink backend;
  - growing a volume (`truncate` plus a filesystem resize, which needs
    root);
  - macOS re-sharing;
  - the `recreate` policy class (e.g. run `uv sync`) and a free-space budget
    for `--warm`;
  - submodule worktrees, which git writes as empty directories, like
    `git worktree add` does.
- **Left to git:** sparse-checkout is supported with the cone filter. LFS and
  other configured filters are always written by git.
- **Faster creation:** start cloning while the donor's `diff-index` runs
  (the hash check already rejects dirty donor files); clone into a staging
  area during `git worktree add --no-checkout`, as git-twig does.

## License

MIT, see [LICENSE](LICENSE). Code adapted from josharian/git-cow-worktree,
leostera/git-twig, lukeed/lane and can1357/oh-my-pi (all MIT) is credited in
[THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES).
