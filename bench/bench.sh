#!/usr/bin/env bash
set -euo pipefail

repo=$(cd "$1" && pwd -P)
repeats=${2:-3}
sharetree=${SHARETREE:-sharetree}
parent=$(dirname "$repo")
command -v btrfs >/dev/null || { echo "btrfs-progs needed" >&2; exit 1; }
scratch=$(mktemp -d "$parent/.sharetree-bench.XXXXXXXX")
wt=$scratch/worktree
cleanup() {
  git -C "$repo" worktree remove --force --force -- "$wt" >/dev/null 2>&1 || true
  rmdir -- "$scratch" 2>/dev/null || true
}
trap cleanup EXIT

now() { date +%s.%N; }
ms() { awk -v a="$1" -v b="$2" 'BEGIN { printf "%.0f", (b - a) * 1000 }'; }

exclusive() {
  sync
  find "$1" -mindepth 1 -maxdepth 1 ! -name .git -print0 |
    xargs -r -0 btrfs filesystem du -s --raw |
    awk '$2 ~ /^[0-9]+$/ { sum += $2 } END { printf "%.0f\n", sum }'
}

run_one() {
  local tool=$1 n=$2 t0 t1 t2 t3 add_ms st_ms excl
  sync; sleep "${SETTLE:-2}"
  t0=$(now)
  if [ "$tool" = git ]; then
    git -C "$repo" worktree add -q --detach "$wt" >/dev/null 2>&1
  else
    (cd "$repo" && "$sharetree" add -q --detach "$wt" >/dev/null 2>&1)
  fi
  t1=$(now)
  t2=$(now)
  git -C "$wt" status --porcelain >/dev/null
  t3=$(now)
  add_ms=$(ms "$t0" "$t1")
  st_ms=$(ms "$t2" "$t3")
  excl=$(exclusive "$wt")
  printf '%-8s %3s %10s %12s %14s\n' "$tool" "$n" "$add_ms" "$st_ms" "$excl"
  git -C "$repo" worktree remove --force -- "$wt"
}

files=$(git -C "$repo" ls-files | wc -l)
bytes=$(git -C "$repo" ls-tree -r -l HEAD | awk '$2 == "blob" { s += $4 } END { print s }')
echo "repo: $repo ($files tracked files, $bytes bytes of blobs at HEAD)"
echo "kernel: $(uname -r); fs: $(stat -f -c %T "$repo"); git: $(git --version | cut -d' ' -f3); $("$sharetree" --version)"
printf '%-8s %3s %10s %12s %14s\n' tool run add_ms status_ms exclusive_B
for n in $(seq 1 "$repeats"); do
  if [ $((n % 2)) -eq 1 ]; then run_one git "$n"; run_one sharetree "$n"
  else run_one sharetree "$n"; run_one git "$n"; fi
done
