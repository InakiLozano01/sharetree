#!/usr/bin/env bash
# Alternate several ways of creating a worktree and print each run's wall time.
#
#   bench/compare.sh <repo> <rounds> <label>=<command>...
#
# <command> is `git` (stock `git worktree add`) or a sharetree binary,
# optionally preceded by VAR=value environment settings, e.g.
#
#   bench/compare.sh ~/src/linux 5 git=git old=X=1\ /tmp/sharetree-old new=X=1\ target/release/sharetree
#
# Each run: sync, sleep $SETTLE (default 3) s, add a detached worktree next to
# the repo, count `git status` changes (must be 0), remove it. The 1-minute
# load average is printed with every run, since a busy host skews the times.
set -euo pipefail
repo=$1 rounds=$2
shift 2
cd "$repo"
for r in $(seq 1 "$rounds"); do
  for v in "$@"; do
    label=${v%%=*} spec=${v#*=}
    sync; sleep "${SETTLE:-3}"
    s=$(date +%s%N)
    if [ "$spec" = git ]; then
      git worktree add -q --detach ../wt-compare >/dev/null 2>&1
    else
      # shellcheck disable=SC2086 # word splitting of VAR=value prefixes is intended
      env $spec add -q --detach ../wt-compare >/dev/null 2>&1
    fi
    e=$(date +%s%N)
    dirty=$(git -C ../wt-compare status --porcelain | wc -l)
    echo "$label $(( (e - s) / 1000000 )) ms dirty=$dirty load=$(cut -d' ' -f1 /proc/loadavg)"
    git worktree remove --force ../wt-compare
  done
done
