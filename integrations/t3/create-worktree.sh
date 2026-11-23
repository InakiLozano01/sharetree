set -eu

: "${T3_PROJECT_ROOT:?}"
: "${T3_WORKTREE_PROJECT_PATH:?}"
: "${T3_WORKTREE_BRANCH:?}"

cd -- "$T3_PROJECT_ROOT"
branch=$T3_WORKTREE_BRANCH
destination=$T3_WORKTREE_PROJECT_PATH
git check-ref-format "refs/heads/$branch"
case "$branch" in
    -*) exit 2 ;;
esac

if git show-ref --verify --quiet "refs/heads/$branch"; then
    set -- -- "$destination" "$branch"
else
    base=$(git rev-parse --verify --end-of-options "${T3_WORKTREE_BASE:-HEAD}^{commit}")
    set -- -b "$branch" -- "$destination" "$base"
fi

if command -v sharetree >/dev/null 2>&1; then
    exec sharetree add --no-volume --no-warm --from "$PWD" "$@"
fi

exec git worktree add "$@"
