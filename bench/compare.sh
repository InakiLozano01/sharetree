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
if (( $# < 3 )) || [[ ! "$2" =~ ^[1-9][0-9]*$ ]]; then
  echo "usage: $0 <repo> <rounds> <label>=<program>... | --tool <label> <program> [args]..." >&2
  exit 2
fi
repo=$(cd "$1" && pwd -P)
rounds=$2
shift 2
cd "$repo"
scratch=$(mktemp -d "$(dirname "$repo")/.sharetree-compare.XXXXXXXX")
wt=$scratch/worktree
cleanup() {
  git worktree remove --force --force -- "$wt" >/dev/null 2>&1 || true
  rmdir -- "$scratch" 2>/dev/null || true
}
trap cleanup EXIT

run_tool() {
  local label=$1 start end dirty
  shift
  if [[ -z $label || $# -eq 0 ]]; then
    echo "each tool needs a label and a program" >&2
    return 2
  fi
  if [[ $# -eq 1 && $1 == git ]]; then
    set -- git worktree
  fi
  sync
  sleep "${SETTLE:-3}"
  start=$(date +%s%N)
  "$@" add -q --detach "$wt" >/dev/null
  end=$(date +%s%N)
  dirty=$(git -C "$wt" status --porcelain | wc -l)
  printf '%s %s ms dirty=%s load=%s\n' "$label" "$(( (end - start) / 1000000 ))" "$dirty" "$(cut -d' ' -f1 /proc/loadavg)"
  git worktree remove --force -- "$wt"
}

run_round() {
  local label spec
  local -a args
  while (( $# )); do
    if [[ $1 == --tool ]]; then
      label=${2:-}
      (( $# >= 3 )) || { echo "--tool needs a label and a program" >&2; return 2; }
      shift 2
      args=()
      while (( $# )) && [[ $1 != --tool ]]; do
        args+=("$1")
        shift
      done
      run_tool "$label" "${args[@]}"
    else
      if [[ $1 != *=* ]]; then
        echo "expected <label>=<program> or --tool" >&2
        return 2
      fi
      label=${1%%=*}
      spec=${1#*=}
      shift
      args=()
      while [[ $spec =~ ^([a-zA-Z_][a-zA-Z_0-9]*=[^[:blank:]]*)[[:blank:]]+(.*)$ ]]; do
        args+=("${BASH_REMATCH[1]}")
        spec=${BASH_REMATCH[2]}
      done
      if (( ${#args[@]} )); then
        run_tool "$label" env "${args[@]}" "$spec"
      else
        run_tool "$label" "$spec"
      fi
    fi
  done
}

for ((round=0; round < rounds; round++)); do
  run_round "$@"
done
