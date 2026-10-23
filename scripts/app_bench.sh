#!/usr/bin/env bash
# Process-global application benchmark: one binary per allocator.
#
# `benches/alloc.rs` calls each GlobalAlloc implementation directly, so it
# measures allocator loops. This script measures the other thing: a whole
# application workload running with the allocator installed as the process
# `#[global_allocator]`, which is how a real deployment uses one. Every
# backend gets its own binary (cargo feature) and its own fresh process, so
# there is no runtime dispatch on the allocation path and no state carried
# between runs.
#
# Usage:
#   scripts/app_bench.sh                    # all backends, 3 s each
#   APP_SECS=10 APP_THREADS=8 scripts/app_bench.sh
#   APP_INDEX=0 scripts/app_bench.sh       # allocation-dense variant
#   APP_P99=1 scripts/app_bench.sh         # per-call latency (extra build)
#
# Env: APP_SECS, APP_THREADS, APP_P99 (sampling divisor; 0 = off),
#      APP_INDEX (0 = skip the hashing pass), APP_REPS (default 3),
#      BACKENDS (space separated subset), CARGO (cargo binary),
#      P99=0 to skip the latency binaries.
set -u -o pipefail

cd "$(dirname "$0")/.."

CARGO="${CARGO:-cargo}"
SECS="${APP_SECS:-3}"
THREADS="${APP_THREADS:-4}"
P99="${APP_P99:-0}"
REPS="${APP_REPS:-3}"
INDEX="${APP_INDEX:-}"
BACKENDS="${BACKENDS:-app-allox app-system app-mimalloc app-snmalloc app-talc}"

build_and_run() {
    local feature="$1" extra="${2:-}"
    local target_dir="${CARGO_TARGET_DIR:-target}"
    local variant="app-$feature$extra"
    local binary_dir="$target_dir/$variant"
    local binary="$binary_dir/release/examples/app_workload"
    local log status
    # cargo puts feature variants in the same profile dir, so build into a
    # per-backend target dir to keep the binaries around for the run.
    log="$(CARGO_TARGET_DIR="$binary_dir" \
        "$CARGO" build --release --example app_workload --features "$feature$extra" 2>&1)"
    status=$?
    if [ "$status" -ne 0 ]; then
        printf '%s\n' "$log" >&2
        echo "  build failed for $feature$extra" >&2
        return 1
    fi
    if [ ! -x "$binary" ]; then
        echo "  missing binary for $feature$extra: $binary" >&2
        return 1
    fi
    APP_SECS="$SECS" APP_THREADS="$THREADS" APP_P99="$P99" APP_INDEX="$INDEX" "$binary"
}

print_result() {
    local feature="$1" line="$2" result
    result="$(printf '%s\n' "$line" | grep '^RESULT ' | head -1 || true)"
    if [ -z "$result" ]; then
        echo "$feature: no RESULT line" >&2
        return 1
    fi
    # RESULT <name> <docs/s> <ns/doc> <peak_rss_kib> <p99_ns>
    set -- $result
    printf '%-10s %4s %14.0f %14.1f %12.1f %10s\n' \
        "$2" "$CURRENT_REP" "$3" "$4" "$(echo "$5" | awk '{print $1/1024}')" "${6:-0}"
}

printf '%-10s %4s %14s %14s %12s %10s\n' backend rep docs/s ns/document peakRSS_MiB p99_ns
printf '%s\n' "-----------------------------------------------------------------------------"

for ((rep = 1; rep <= REPS; rep++)); do
    CURRENT_REP="$rep"
    for feature in $BACKENDS; do
        line="$(build_and_run "$feature")" || { echo "$feature: build failed" >&2; continue; }
        print_result "$feature" "$line" || continue
    done
done

if [ "$P99" != "0" ]; then
    echo
    echo "per-call latency (sampled 1-in-$((P99 < 1 ? 1 : P99)) calls, clock overhead subtracted):"
    for ((rep = 1; rep <= REPS; rep++)); do
        CURRENT_REP="$rep"
        for feature in $BACKENDS; do
            line="$(build_and_run "$feature" ",app-p99")" || continue
            print_result "$feature" "$line" || continue
        done
    done
fi
