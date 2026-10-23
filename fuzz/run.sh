#!/usr/bin/env bash
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
repo="$(pwd)"

target="${1:-all}"
duration="${2:-1800}"
if [[ ! "$duration" =~ ^[1-9][0-9]*$ ]]; then
    echo "fuzz duration must be a positive number of seconds" >&2
    exit 2
fi

case "$target" in
    all) targets=(grammar yaml_loader mountinfo maps environ) ;;
    grammar|yaml_loader|mountinfo|maps|environ) targets=("$target") ;;
    *) echo "unknown fuzz target: $target" >&2; exit 2 ;;
esac

for name in "${targets[@]}"; do
    corpus="$repo/tmp/fuzz/corpus/$name"
    artifacts="$repo/tmp/fuzz/artifacts/$name"
    logs="$repo/tmp/fuzz/logs"
    mkdir -p "$corpus" "$artifacts" "$logs"
    for seed in "fuzz/seeds/$name"/*; do
        digest="$(sha256sum "$seed" | cut -d' ' -f1)"
        cp -n "$seed" "$corpus/seed-$digest"
    done
    log="$logs/$name-$(date -u +%Y%m%dT%H%M%SZ)-$$.log"
    echo "Fuzzing $name for $duration seconds; log=$log" >&2
    if cargo fuzz run "$name" "$corpus" -- \
        "-max_total_time=$duration" \
        "-max_len=65536" \
        "-artifact_prefix=$artifacts/" \
        "-verbosity=0" \
        "-print_final_stats=1" 2>&1 | tee "$log" | \
        rg --line-buffered '(^[[:space:]]*(Compiling|Finished|Running)|^stat::|ERROR:|SUMMARY:|error:|^==[0-9]+==)'; then
        echo "Fuzzer $name completed without a crash; log=$log" >&2
    else
        echo "Fuzzer $name failed; full log=$log" >&2
        tail -n 120 "$log" >&2
        exit 1
    fi
done
