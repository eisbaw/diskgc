#!/usr/bin/env bash
# Run one mutant or a bounded shard with build output in a disposable container.
# Check modes validate the mounts and selection without compiling anything.
set -euo pipefail

mode="${1:---check}"
case "$mode" in
    --check|--run)
        if (( $# > 2 )); then
            echo "usage: $0 [--check|--run] [one-mutant regex]" >&2
            exit 2
        fi
        source_file="crates/diskgc-core/src/select/parse.rs"
        selector="-F"
        value="${2:-parse\\.rs:134:9:}"
        max_mutants=1
        label="parse-smoke"
        ;;
    --check-shard|--run-shard)
        if (( $# != 3 )); then
            echo "usage: $0 --check-shard|--run-shard parse|check|evaluate|mod|executor INDEX/TOTAL" >&2
            exit 2
        fi
        case "$2" in
            parse) source_file="crates/diskgc-core/src/select/parse.rs" ;;
            check) source_file="crates/diskgc-core/src/select/check.rs" ;;
            evaluate) source_file="crates/diskgc-core/src/select/evaluate.rs" ;;
            mod) source_file="crates/diskgc-core/src/select/mod.rs" ;;
            executor) source_file="crates/diskgc-core/src/executor.rs" ;;
            *) echo "unknown mutation source: $2" >&2; exit 2 ;;
        esac
        if [[ ! "$3" =~ ^(0|[1-9][0-9]*)/([1-9][0-9]*)$ ]]; then
            echo "shard must be INDEX/TOTAL with zero-based INDEX" >&2
            exit 2
        fi
        index="${BASH_REMATCH[1]}"
        total="${BASH_REMATCH[2]}"
        if (( ${#index} > 2 || ${#total} > 2 || total > 64 || index >= total )); then
            echo "shard index must be below its total, which may be at most 64" >&2
            exit 2
        fi
        selector="--shard"
        value="$3"
        max_mutants=16
        label="$2-$index-of-$total"
        ;;
    *)
        echo "usage: $0 [--check|--run] [one-mutant regex]" >&2
        echo "       $0 --check-shard|--run-shard parse|check|evaluate|mod|executor INDEX/TOTAL" >&2
        exit 2
        ;;
esac

repo="$(git rev-parse --show-toplevel)"
git_common="$(git rev-parse --path-format=absolute --git-common-dir)"
registry="${CARGO_HOME:-$HOME/.cargo}/registry"
image="$(python3 "$repo/tools/mutation_scope.py" image)"
image_id="$(podman image inspect --format '{{.Id}}' "$image")"
image_id="${image_id#sha256:}"
if [[ ! "$image_id" =~ ^[0-9a-f]{64}$ ]]; then
    echo "cached mutation image has no stable sha256 ID: $image" >&2
    exit 2
fi
# Cargo's source copy, build and reports stay on tmpfs. Host writes are limited
# to Podman metadata plus the bounded log and copied report. Keep 16 GiB free
# for other host work; the extra start headroom covers container setup.
disk_floor="$((16 * 1024 * 1024 * 1024))"
copy_reserve="$((16 * 1024 * 1024))"
report_limit="$((16 * 1024 * 1024))"
log_limit="$((8 * 1024 * 1024))"

if [[ -z "${NIX_CC:-}" ]]; then
    echo "run this script inside the project Nix development shell" >&2
    exit 2
fi
available="$(df -B1 --output=avail "$repo" | tail -1)"
minimum="$((disk_floor + 64 * 1024 * 1024))"
if (( available < minimum )); then
    echo "need at least 16 GiB + 64 MiB free before any Podman run; have $available bytes" >&2
    exit 2
fi
memory_available_kib="$(awk '/^MemAvailable:/ { print $2 }' /proc/meminfo)"
if (( memory_available_kib < 12 * 1024 * 1024 )); then
    echo "need at least 12 GiB available RAM before the 10 GiB capped container" >&2
    exit 2
fi
graphroot="$(podman info --format '{{.Store.GraphRoot}}')"
runroot="$(podman info --format '{{.Store.RunRoot}}')"
if [[ "$(stat -c %d "$graphroot")" != "$(stat -c %d "$repo")" ]] || \
   [[ "$(findmnt -T "$runroot" -no FSTYPE)" != tmpfs ]]; then
    echo "Podman graphroot must share the repository filesystem and runroot must be tmpfs" >&2
    exit 2
fi
if (( $(df -B1 --output=avail "$runroot" | tail -1) < 64 * 1024 * 1024 )); then
    echo "Podman runroot needs 64 MiB of tmpfs headroom" >&2
    exit 2
fi
if [[ ! -d "$registry" ]] || ! podman image exists "$image_id"; then
    echo "cached Cargo registry and Debian container image are required" >&2
    exit 2
fi
if ! git -C "$repo" check-ignore -q tmp/system; then
    echo "tmp/system must be Git-ignored before cargo-mutants copies the source" >&2
    exit 2
fi

mounts=(
    --volume /nix/store:/nix/store:ro
    --volume "$repo:$repo:ro"
    --volume "$git_common:$git_common:ro"
    --volume "$registry:/work/cargo/registry:ro"
)
environment=(
    --env HOME=/work
    --env CARGO_HOME=/work/cargo
    --env CARGO_TARGET_DIR=/work/target
    --env CARGO_NET_OFFLINE=true
    --env CARGO_INCREMENTAL=0
    --env TMPDIR=/work/tmp
)
for name in PATH CC CC_FOR_BUILD AR AR_FOR_BUILD LD LD_FOR_BUILD \
    NIX_CC NIX_CC_FOR_BUILD NIX_BINTOOLS NIX_BINTOOLS_FOR_BUILD \
    NIX_CFLAGS_COMPILE NIX_CFLAGS_COMPILE_FOR_BUILD \
    NIX_LDFLAGS NIX_LDFLAGS_FOR_BUILD NIX_HARDENING_ENABLE \
    NIX_ENFORCE_NO_NATIVE; do
    if [[ -v "$name" ]]; then
        environment+=(--env "$name")
    fi
done
container=(
    podman run --pull=never --network=none --read-only
    --memory=10g --memory-swap=10g
    --tmpfs /work:rw,size=8g --tmpfs /tmp:rw,size=64m
    "${mounts[@]}" "${environment[@]}"
    --workdir "$repo"
)

container_presence() {
    local names
    if ! names="$(timeout 15s podman ps -a --format '{{.Names}}')"; then
        echo "cannot inspect Podman containers" >&2
        return 2
    fi
    if printf '%s\n' "$names" | grep -Fx -- "$1" >/dev/null; then
        printf 'present\n'
    else
        printf 'absent\n'
    fi
}
remove_disposable_container() {
    local name="$1" state failed=0
    state="$(container_presence "$name")" || return 1
    if [[ "$state" == present ]] &&
       ! timeout 15s podman rm -f "$name" >/dev/null; then
        echo "Podman failed to remove disposable container $name" >&2
        failed=1
    fi
    state="$(container_presence "$name")" || return 1
    if [[ "$state" != absent ]]; then
        echo "disposable container $name remains in Podman storage" >&2
        return 1
    fi
    (( failed == 0 ))
}
preflight_name="diskgc-mutant-preflight-$$"
preflight_cleanup() {
    local status=$?
    trap - EXIT INT TERM
    if ! remove_disposable_container "$preflight_name"; then
        status=1
    fi
    exit "$status"
}
trap preflight_cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
selected="$(
    timeout 180s "${container[@]}" --rm --name "$preflight_name" "$image_id" sh -ec '
        mkdir -p /work/tmp
        test -r Cargo.lock
        grep -F " / ro," /proc/self/mountinfo >/dev/null
        grep -F " /work rw," /proc/self/mountinfo >/dev/null
        grep -F " /tmp rw," /proc/self/mountinfo >/dev/null
        test "$(cat /sys/fs/cgroup/memory.max)" = 10737418240
        test "$(cat /sys/fs/cgroup/memory.swap.max)" = 0
        for mount in "$PWD" /nix/store /work/cargo/registry; do
            grep -F " $mount ro," /proc/self/mountinfo >/dev/null || {
                echo "expected read-only bind mount: $mount" >&2
                exit 2
            }
        done
        cargo --version >/dev/null
        rustc --version >/dev/null
        cc --version >/dev/null
        cargo mutants --list --gitignore true \
            --file "$1" "$2" "$3"
    ' sh "$source_file" "$selector" "$value"
)"
selected_count=0
if [[ -n "$selected" ]]; then
    selected_count="$(printf '%s\n' "$selected" | wc -l)"
fi
if (( selected_count < 1 || selected_count > max_mutants )); then
    echo "selection must contain 1-$max_mutants mutants; found $selected_count" >&2
    printf '%s\n' "$selected" | sed -n '1,5p' >&2
    exit 2
fi
if [[ "$selector" == --shard ]]; then
    if ! python3 "$repo/tools/verify_mutant_sweep.py" check "$2" "$total" "$index" "$image_id" <<< "$selected"; then
        echo "shard selection does not cover a complete, disjoint inventory" >&2
        exit 2
    fi
fi
printf 'Selected %s mutant(s) from %s (%s %s):\n%s\n' \
    "$selected_count" "$source_file" "$selector" "$value" "$selected"
if [[ "$mode" == --check || "$mode" == --check-shard ]]; then
    echo "Container preflight passed; no build was started."
    exit 0
fi

# All Cargo build state is on the container's 8 GiB tmpfs. The read-only root
# filesystem still leaves Podman metadata and copied reports on host disk.
available="$(df -B1 --output=avail "$repo" | tail -1)"
if (( available < minimum )); then
    echo "host disk fell below the start reserve during preflight; have $available bytes" >&2
    exit 2
fi
if (( $(df -B1 --output=avail "$runroot" | tail -1) < 64 * 1024 * 1024 )) ||
   (( $(awk '/^MemAvailable:/ { print $2 }' /proc/meminfo) < 12 * 1024 * 1024 )); then
    echo "Podman runroot or RAM fell below the start reserve during preflight" >&2
    exit 2
fi

log_dir="$repo/tmp/mutants-container-smoke/$label-$(date -u +%Y%m%dT%H%M%SZ)-$$"
mkdir -p "$log_dir"
printf '%s\n' "$selected" >"$log_dir/selected.txt"
if [[ "$selector" == --shard ]]; then
    python3 "$repo/tools/verify_mutant_sweep.py" identity "$2" "$total" "$image_id" >"$log_dir/plan.sha256"
else
    python3 "$repo/tools/verify_mutant_sweep.py" snapshot >"$log_dir/workspace.sha256"
fi
container_name="diskgc-mutant-smoke-$$"
stop_container() {
    timeout 12s podman stop -t 5 "$container_name" >/dev/null 2>&1 || \
        timeout 12s podman kill "$container_name" >/dev/null 2>&1 || true
    if [[ "$(timeout 12s podman inspect --format '{{.State.Running}}' "$container_name" 2>/dev/null)" == true ]]; then
        timeout 12s podman kill "$container_name" >/dev/null 2>&1 || true
    fi
    [[ "$(timeout 12s podman inspect --format '{{.State.Running}}' "$container_name" 2>/dev/null)" == false ]]
}
copy_reports() {
    local report_size available
    report_size="$(podman exec "$container_name" du -sb /work/out | awk '{print $1}')"
    if [[ ! "$report_size" =~ ^[0-9]+$ ]] || (( report_size > report_limit )); then
        echo "report is missing or exceeds the $report_limit byte limit" >&2
        return 1
    fi
    available="$(df -B1 --output=avail "$repo" | tail -1)"
    if (( available < disk_floor + copy_reserve + report_size )); then
        echo "insufficient disk reserve for $report_size report bytes" >&2
        return 1
    fi
    timeout 120s podman exec "$container_name" tar -C /work/out -cf - . | \
        timeout 120s python3 "$repo/tools/copy_mutant_reports.py" "$log_dir/out" \
            "$log_dir/selected.txt" "$disk_floor" "$copy_reserve" "$report_limit"
}
cleanup() {
    local status=$?
    trap - EXIT INT TERM
    if ! remove_disposable_container "$container_name"; then
        status=1
    fi
    if [[ -n "${exec_pid:-}" ]]; then
        wait "$exec_pid" 2>/dev/null || true
    fi
    if [[ ! -f "$log_dir/exit-status" ]]; then
        printf '%s\n' "$status" >"$log_dir/exit-status"
    fi
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# A stopped container loses its tmpfs. Keep it alive while cargo-mutants runs,
# stream its report through podman exec, then stop and remove it.
container_id="$("${container[@]}" -d --name "$container_name" "$image_id" \
    sh -ec 'mkdir -p /work/tmp /work/out; exec sleep infinity')"
printf '%s\n' "$container_id" >"$log_dir/container-id"
run_status=0
pressure_stopped=0
(
    set -o pipefail
    timeout --signal=TERM --kill-after=15s 7200s podman exec "$container_name" sh -c '
    if cargo mutants --gitignore true \
        --file "$1" "$2" "$3" \
        --test-workspace=true --jobs 1 --output /work/out \
        -- --locked --offline; then
        result=0
    else
        result=$?
    fi
    printf "%s\n" "$result" > /work/mutant-exit
    exit "$result"
' sh "$source_file" "$selector" "$value" 2>&1 | \
        python3 "$repo/tools/bounded_host_log.py" "$log_dir/run.log" \
            "$disk_floor" "$((32 * 1024 * 1024))" "$log_limit"
) &
exec_pid=$!
while kill -0 "$exec_pid" 2>/dev/null; do
    available="$(df -B1 --output=avail "$repo" | tail -1)"
    runroot_available="$(df -B1 --output=avail "$runroot" | tail -1)"
    memory_available_kib="$(awk '/^MemAvailable:/ { print $2 }' /proc/meminfo)"
    log_size="$(stat -c %s "$log_dir/run.log" 2>/dev/null || printf '0')"
    if (( available < disk_floor + 32 * 1024 * 1024 ||
          runroot_available < 32 * 1024 * 1024 ||
          memory_available_kib < 4 * 1024 * 1024 ||
          log_size >= log_limit )); then
        echo "stopping disposable container on host disk, Podman runroot, RAM, or log pressure" >&2
        stop_container || true
        if remove_disposable_container "$container_name"; then
            pressure_stopped=1
            run_status=1
            break
        fi
        echo "could not verify disposable container removal" >&2
        exit 1
    fi
    sleep 2
done
wait "$exec_pid" || run_status=$?

if (( pressure_stopped )); then
    echo "run stopped for resource pressure; incomplete run log is at $log_dir/run.log" >&2
    exit 1
fi
if [[ "$selector" == --shard ]]; then
    current_identity="$(python3 "$repo/tools/verify_mutant_sweep.py" identity "$2" "$total" "$image_id")" || exit 1
    if [[ "$current_identity" != "$(cat "$log_dir/plan.sha256")" ]]; then
        echo "mutation plan or runner procedure changed during run; report is not valid evidence" >&2
        exit 1
    fi
else
    current_snapshot="$(python3 "$repo/tools/verify_mutant_sweep.py" snapshot)" || exit 1
    if [[ "$current_snapshot" != "$(cat "$log_dir/workspace.sha256")" ]]; then
        echo "Cargo workspace changed during mutation run; report is not valid evidence" >&2
        exit 1
    fi
fi
state="$(container_presence "$container_name")" || exit 1
if [[ "$state" != present ]]; then
    echo "container disappeared; logs=$log_dir/run.log" >&2
    exit 1
fi
if [[ "$(podman inspect --format '{{.State.Running}}' "$container_name")" != true ]]; then
    echo "container exited before live report copy; logs=$log_dir/run.log" >&2
    exit 1
fi
container_status="$(podman exec "$container_name" cat /work/mutant-exit 2>/dev/null || true)"
if [[ ! "$container_status" =~ ^(0|[1-9][0-9]{0,2})$ ]] ||
   (( container_status > 255 )); then
    echo "missing or invalid cargo-mutants exit marker" >&2
    exit 1
fi
printf '%s\n' "$container_status" >"$log_dir/cargo-exit-status"
if (( container_status != run_status )); then
    echo "podman exec status $run_status differs from cargo-mutants status $container_status" >&2
    exit 1
fi
if ! copy_reports; then
    echo "could not copy or validate mutation results; incomplete logs=$log_dir" >&2
    exit 1
fi
if ! stop_container; then
    echo "reports copied, but container $container_name remains live" >&2
    exit 1
fi
echo "Mutation exit=$run_status; logs=$log_dir"
exit "$run_status"
