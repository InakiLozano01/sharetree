set -euo pipefail

fail() {
    printf '%s\n' "$*" >&2
    return 1
}

[[ ${GITHUB_ACTIONS:-} == true && ${RUNNER_ENVIRONMENT:-} == github-hosted && ${RUNNER_OS:-} == Linux ]] ||
    fail "The Btrfs image fixture requires a GitHub-hosted Linux runner"

revision=$(git rev-parse --verify HEAD)
printf 'Checkout revision: %s\n' "$revision"
[[ $revision == "${GITHUB_SHA:?}" ]] || fail "Checkout does not match GITHUB_SHA"

owned_loop() {
    if [[ -e $image ]]; then
        sudo losetup --list --associated "$image" --noheadings --raw --output NAME
    fi
}

mount_identity() {
    findmnt --kernel --noheadings --raw --nofsroot --output SOURCE,FSTYPE "$@"
}

remove_fixture() {
    local attached mount_status=0
    attached=$(owned_loop) || return
    if [[ -n $attached && ! $attached =~ ^/dev/loop[0-9]+$ ]]; then
        fail "Unexpected loop devices for $image: $attached"
        return 1
    fi
    if [[ -d $mount_dir ]]; then
        mountpoint --quiet -- "$mount_dir" || mount_status=$?
        case $mount_status in
            0)
                if [[ -z $attached || $(mount_identity --mountpoint "$mount_dir") != "$attached btrfs" ]]; then
                    fail "Refusing to unmount $mount_dir without the job image's loop device"
                    return 1
                fi
                sudo umount -- "$mount_dir" || return
                ;;
            32) ;;
            *) return "$mount_status" ;;
        esac
    fi
    if [[ -n $attached ]]; then
        sudo losetup --detach -- "$attached" || return
        sudo udevadm settle --timeout=10 || return
        attached=$(owned_loop) || return
        if [[ -n $attached ]]; then
            fail "Loop device still attached to $image: $attached"
            return 1
        fi
    fi
    if [[ -d $mount_dir ]]; then
        rmdir -- "$mount_dir" || return
    fi
    rm -f -- "$image" || return
    rmdir -- "$root"
}

cleanup() {
    local status=$?
    trap - EXIT
    if ! remove_fixture; then
        printf 'Btrfs fixture cleanup failed for %s (original status %s)\n' "$root" "$status" >&2
        exit 1
    fi
    printf 'Removed Btrfs fixture %s\n' "$root"
    exit "$status"
}

run_scenario() {
    local suite=$1 scenario=$2 output status=0
    printf '\nRunning %s::%s\n' "$suite" "$scenario"
    output=$(cargo test --locked --test "$suite" -- "$scenario" --exact --nocapture 2>&1) || status=$?
    printf '%s\n' "$output"
    if (( status != 0 )); then
        return "$status"
    fi
    if [[ $output == *SKIP:* ]] ||
        ! grep -Eq '^test result: ok\. 1 passed; 0 failed; 0 ignored;' <<<"$output"; then
        fail "Expected one executed passing test for $suite::$scenario"
    fi
}

root=$(mktemp -d "${RUNNER_TEMP:?}/sharetree-btrfs.XXXXXX")
image=$root/btrfs.img
mount_dir=$root/mount
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

printf '::group::Btrfs fixture preflight\n'
btrfs --version
mkdir -- "$mount_dir"
truncate -s 2G -- "$image"
mkfs.btrfs "$image"
loop=$(sudo losetup --find --show --nooverlap -- "$image")
attached=$(owned_loop)
[[ $loop =~ ^/dev/loop[0-9]+$ && $attached == "$loop" ]] ||
    fail "Loop device $loop does not match the job image"
sudo mount -t btrfs -- "$loop" "$mount_dir"
[[ $(mount_identity --mountpoint "$mount_dir") == "$loop btrfs" ]] ||
    fail "Expected $loop mounted as Btrfs at $mount_dir"
sudo chown "$(id -u):$(id -g)" -- "$mount_dir"

export CARGO_TARGET_DIR=$mount_dir/target
export SHARETREE_REQUIRE_BTRFS=1 CARGO_TERM_COLOR=never LC_ALL=C
fixture_dir=$CARGO_TARGET_DIR/tmp
mkdir -p -- "$fixture_dir"
[[ $(mount_identity --target "$fixture_dir") == "$loop btrfs" ]] ||
    fail "Cargo fixture directory $fixture_dir is outside the job's Btrfs mount"
dd if=/dev/urandom of="$fixture_dir/source" bs=64K count=1 status=none
cp --reflink=always -- "$fixture_dir/source" "$fixture_dir/clone"
cmp -- "$fixture_dir/source" "$fixture_dir/clone"
btrfs filesystem du -s --raw "$fixture_dir/source" "$fixture_dir/clone"
rm -- "$fixture_dir/source" "$fixture_dir/clone"
printf 'Writable Btrfs fixture with working reflinks: %s (%s)\n' "$fixture_dir" "$loop"
printf '::endgroup::\n'

run_scenario dropin identity
run_scenario space writes_do_not_leak_between_worktrees
run_scenario space btrfs_exclusive_bytes_are_near_zero
run_scenario warm warm_rollback_removes_clones_from_a_tracked_directory
