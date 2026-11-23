set -euo pipefail
set +m

fail() {
    printf '%s\n' "$*" >&2
    return 1
}

[[ ${GITHUB_ACTIONS:-} == true && ${RUNNER_ENVIRONMENT:-} == github-hosted && ${RUNNER_OS:-} == Linux ]] ||
    fail "The filesystem fixture requires a GitHub-hosted Linux runner"

case ${1:-} in
    btrfs|xfs) filesystem=$1 ;;
    *) fail "Usage: bash scripts/test-linux-filesystems.sh {btrfs|xfs}" ;;
esac

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

remove_mount() {
    local target=$1 source=$2 type=$3 mount_status=0
    [[ -d $target ]] || return 0
    mountpoint --quiet -- "$target" || mount_status=$?
    case $mount_status in
        0)
            if [[ -z $source || $(mount_identity --mountpoint "$target") != "$source $type" ]]; then
                fail "Refusing to unmount $target without the job-owned source ($source $type)"
                return 1
            fi
            sudo umount -- "$target" || return
            ;;
        32) ;;
        *) return "$mount_status" ;;
    esac
    rmdir -- "$target" || return
    printf 'Removed fixture mountpoint %s\n' "$target"
}

remove_image_fixture() {
    local attached remaining
    attached=$(owned_loop) || return
    if [[ -n $attached && ! $attached =~ ^/dev/loop[0-9]+$ ]]; then
        fail "Unexpected loop devices for $image: $attached"
        return 1
    fi
    remove_mount "$mount_dir" "$attached" "$filesystem" || return
    if [[ -n $attached ]]; then
        sudo losetup --detach -- "$attached" || return
        sudo udevadm settle --timeout=10 || return
        remaining=$(owned_loop) || return
        if [[ -n $remaining ]]; then
            fail "Loop device still attached to $image: $remaining"
            return 1
        fi
        printf 'Detached %s from %s\n' "$attached" "$image"
    fi
    rm -f -- "$image"
}

cleanup() {
    local status=$? cleanup_status=0
    trap - EXIT
    trap '' INT TERM
    remove_mount "$tmpfs_dir" "$tmpfs_source" tmpfs || cleanup_status=1
    remove_image_fixture || cleanup_status=1
    if (( cleanup_status != 0 )) || ! rmdir -- "$root"; then
        printf '%s fixture cleanup failed for %s (original status %s)\n' "$filesystem" "$root" "$status" >&2
        exit 1
    fi
    printf 'Removed %s fixture %s\n' "$filesystem" "$root"
    exit "$status"
}

run_child() {
    local output=$1 status=0
    shift
    if [[ -n $output ]]; then
        setsid -- "$@" >"$output" 2>&1 &
    else
        setsid -- "$@" &
    fi
    child_pid=$!
    wait "$child_pid" || status=$?
    child_pid=
    return "$status"
}

cancel() {
    local status=$1 attempt
    trap '' INT TERM
    if [[ -n $child_pid ]]; then
        kill -TERM -- "-$child_pid" 2>/dev/null || kill -TERM -- "$child_pid" 2>/dev/null || :
        for attempt in {1..10}; do
            if ! kill -0 -- "-$child_pid" 2>/dev/null && ! kill -0 -- "$child_pid" 2>/dev/null; then
                break
            fi
            sleep 0.1
        done
        kill -KILL -- "-$child_pid" 2>/dev/null || kill -KILL -- "$child_pid" 2>/dev/null || :
        wait "$child_pid" 2>/dev/null || :
    fi
    exit "$status"
}

run_scenario() {
    local suite=$1 scenario=$2 output=$mount_dir/scenario.log status=0
    printf '\nRunning %s::%s\n' "$suite" "$scenario"
    run_child "$output" cargo test --locked --test "$suite" -- "$scenario" --exact --nocapture || status=$?
    cat -- "$output"
    if (( status != 0 )); then
        return "$status"
    fi
    if grep -q 'SKIP:' "$output" ||
        ! grep -Eq '^test result: ok\. 1 passed; 0 failed; 0 ignored;' "$output"; then
        fail "Expected one executed passing test for $suite::$scenario"
    fi
}

root=$(mktemp -d "${RUNNER_TEMP:?}/sharetree-$filesystem.XXXXXX")
image=$root/$filesystem.img
mount_dir=$root/mount
tmpfs_dir=$root/tmpfs
tmpfs_source=sharetree-tmpfs-${root##*.}
child_pid=
trap cleanup EXIT
trap 'cancel 130' INT
trap 'cancel 143' TERM

printf '::group::%s fixture preflight\n' "$filesystem"
mkdir -- "$mount_dir"
truncate -s 2G -- "$image"
case $filesystem in
    btrfs)
        btrfs --version
        run_child '' mkfs.btrfs "$image"
        export SHARETREE_REQUIRE_BTRFS=1
        ;;
    xfs)
        mkfs.xfs -V
        run_child '' mkfs.xfs -m reflink=1 "$image"
        export SHARETREE_REQUIRE_XFS=1
        ;;
esac
loop=$(sudo losetup --find --show --nooverlap -- "$image")
attached=$(owned_loop)
[[ $loop =~ ^/dev/loop[0-9]+$ && $attached == "$loop" ]] ||
    fail "Loop device $loop does not match the job image"
sudo mount -t "$filesystem" -- "$loop" "$mount_dir"
[[ $(mount_identity --mountpoint "$mount_dir") == "$loop $filesystem" ]] ||
    fail "Expected $loop mounted as $filesystem at $mount_dir"
sudo chown "$(id -u):$(id -g)" -- "$mount_dir"

export CARGO_TARGET_DIR=$mount_dir/target
export CARGO_TERM_COLOR=never LC_ALL=C
fixture_dir=$CARGO_TARGET_DIR/tmp
mkdir -p -- "$fixture_dir"
[[ $(mount_identity --target "$fixture_dir") == "$loop $filesystem" ]] ||
    fail "Cargo fixture directory $fixture_dir is outside the job's $filesystem mount"
dd if=/dev/urandom of="$fixture_dir/source" bs=64K count=1 status=none
cp --reflink=always -- "$fixture_dir/source" "$fixture_dir/clone"
cmp -- "$fixture_dir/source" "$fixture_dir/clone"
if [[ $filesystem == btrfs ]]; then
    btrfs filesystem du -s --raw "$fixture_dir/source" "$fixture_dir/clone"
fi
rm -- "$fixture_dir/source" "$fixture_dir/clone"
printf 'Writable %s fixture with working reflinks: %s (%s)\n' "$filesystem" "$fixture_dir" "$loop"

if [[ $filesystem == xfs ]]; then
    mkdir -- "$tmpfs_dir"
    sudo mount -t tmpfs -o "size=64M,uid=$(id -u),gid=$(id -g),mode=0700" -- "$tmpfs_source" "$tmpfs_dir"
    [[ $(mount_identity --mountpoint "$tmpfs_dir") == "$tmpfs_source tmpfs" ]] ||
        fail "Expected $tmpfs_source mounted as tmpfs at $tmpfs_dir"
    native_device=$(stat -c %d -- "$fixture_dir")
    tmpfs_device=$(stat -c %d -- "$tmpfs_dir")
    [[ $native_device != "$tmpfs_device" ]] ||
        fail "The XFS and tmpfs fixture directories share device $native_device"
    touch -- "$tmpfs_dir/probe"
    rm -- "$tmpfs_dir/probe"
    export SHARETREE_TEST_TMPFS_DIR=$tmpfs_dir
    printf 'Writable tmpfs fixture: %s (device %s, XFS device %s)\n' "$tmpfs_dir" "$tmpfs_device" "$native_device"
fi
printf '::endgroup::\n'

run_scenario dropin identity
run_scenario space writes_do_not_leak_between_worktrees
if [[ $filesystem == btrfs ]]; then
    run_scenario space btrfs_exclusive_bytes_are_near_zero
    run_scenario space reshare_recovers_sharing_after_git_rewrites
    run_scenario space reshare_all_inventories_each_checkout_once_and_recovers_sharing
    run_scenario space reshare_all_skips_stale_checkout_registrations
    run_scenario space reshare_all_reports_candidates_before_kernel_comparison
    run_scenario space reshare_all_tries_an_alternate_donor
    run_scenario space reshare_isolates_failed_filters
    run_scenario space reshare_reports_confirmed_bytes_before_a_later_mismatch
    run_scenario warm warm_rollback_removes_clones_from_a_tracked_directory
else
    run_scenario fallback tmpfs_falls_back_to_git_worktree_add
    run_scenario fallback destination_on_another_filesystem_falls_back
    run_scenario fallback warm_on_tmpfs_copies_nothing
fi
