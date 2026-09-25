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
#      APP_INDEX (0 = skip the hashing pass), BACKENDS (space separated
#      subset), CARGO (cargo binary), P99=0 to skip the latency binaries.
set -u -o pipefail

cd "$(dirname "$0")/.."

CARGO="${CARGO:-cargo}"
SECS="${APP_SECS:-3}"
THREADS="${APP_THREADS:-4}"
P99="${APP_P99:-0}"
BACKENDS="${BACKENDS:-app-allox app-system app-mimalloc app-snmalloc app-talc}"
name_of() { echo "${1#app-}"; }

build_and_run() {
    local feature="$1" extra="${2:-}"
    local target_dir="${CARGO_TARGET_DIR:-target}"
    # cargo puts feature variants in the same profile dir, so build into a
    # per-backend target dir to keep the binaries around for the run.
    if ! CARGO_TARGET_DIR="$target_dir/app-$feature" \
        "$CARGO" build --release --example app_workload --features "$feature$extra" 2>&1 \
        | grep -q '^error'; then
        : # built (warnings are not failures here)
    else
        echo "  build failed for $feature$extra" >&2
        return 1
    fi
    local binary="$target_dir/app-$feature/release/examples/app_workload"
    APP_SECS="$SECS" APP_THREADS="$THREADS" "$binary"
}

printf '%-10s %14s %14s %12s %10s\n' backend docs/s ns/document peakRSS_MiB p99_ns
printf '%s\n' "-------------------------------------------------------------------"

for feature in $BACKENDS; do
    line="$(build_and_run "$feature")" || { echo "$feature: build failed" >&2; continue; }
    result="$(printf '%s\n' "$line" | grep '^RESULT' || true)"
    if [ -z "$result" ]; then
        echo "$feature: no RESULT line" >&2
        continue
    fi
    # RESULT <name> <docs/s> <ns/doc> <peak_rss_kib> <p99_ns>
    set -- $result
    printf '%-10s %14.0f %14.1f %12.1f %10s\n' \
        "$2" "$3" "$4" "$(echo "$5" | awk '{print $1/1024}')" "${6:-0}"
done

if [ "$P99" != "0" ]; then
    echo
    echo "per-call latency (sampled 1-in-$((P99 < 1 ? 1 : P99)) calls, clock overhead subtracted):"
    for feature in $BACKENDS; do
        line="$(build_and_run "$feature" ",app-p99")" || continue
        printf '  %s\n' "$(printf '%s\n' "$line" | tail -1)"
    done
fi
