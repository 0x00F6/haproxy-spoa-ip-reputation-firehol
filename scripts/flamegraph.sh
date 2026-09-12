#!/bin/sh
# Generates a CPU flamegraph of the Rust benchmark workload (benches/spoe) into
# $BENCH_OUTPUT_DIR/flamegraph.svg.
#
# Backends, in order of preference (override with FLAMEGRAPH_BACKEND=perf|pprof):
#   perf  - `cargo flamegraph` (flamegraph-rs/flamegraph) sampling the benchmark process with
#           `perf record -e cpu-clock --call-graph fp` (FLAMEGRAPH_CALLGRAPH=dwarf switches to DWARF
#           unwinding). Host builds carry frame pointers (.cargo/config.toml), which makes fp
#           unwinding reliable for every thread with a small perf.data; perf's DWARF post-unwinding
#           fails for secondary threads with some perf/elfutils combinations. Requirements on Linux:
#             * perf installed (Debian/Ubuntu: linux-perf or linux-tools-<kernel>);
#             * permission to profile: kernel.perf_event_paranoid <= 2
#               (sudo sysctl -w kernel.perf_event_paranoid=1), or FLAMEGRAPH_ROOT=1 so that
#               cargo flamegraph invokes perf through sudo (--root);
#             * cargo-flamegraph: cargo install flamegraph.
#   pprof - the in-process sampling profiler compiled into the benchmark
#           (SPOA_BENCH_PROFILER=pprof, see benches/spoe/profiler.rs). Needs no privilege and no
#           extra tool; used automatically when perf cannot be used.
#
# Both backends run the benchmark selected by BENCH_PROFILE_FILTER in Criterion's
# --profile-time mode: the workload runs for BENCH_PROFILE_TIME seconds without statistical
# analysis, and only that phase is sampled (compilation and fixture start-up are not).
# Other knobs: FLAMEGRAPH_FREQ (sampling rate, default 99 Hz), FLAMEGRAPH_FLAGS (extra
# cargo flamegraph options), BENCH_OUTPUT_DIR (flamegraph.svg and perf.data location).
set -eu

CARGO=${CARGO:-cargo}
OUT_DIR=${BENCH_OUTPUT_DIR:-build/bench}
FILTER=${BENCH_PROFILE_FILTER:-spoe/concurrent/32}
PROFILE_TIME=${BENCH_PROFILE_TIME:-15}
BACKEND=${FLAMEGRAPH_BACKEND:-auto}
SVG="$OUT_DIR/flamegraph.svg"

mkdir -p "$OUT_DIR"
rm -f "$SVG"

perf_usable() {
    command -v perf >/dev/null 2>&1 || { echo "perf: not installed"; return 1; }
    tmp=$(mktemp)
    if perf record -o "$tmp" -- true >/dev/null 2>&1; then
        rm -f "$tmp" "$tmp.old"
        return 0
    fi
    rm -f "$tmp" "$tmp.old"
    paranoid=$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo '?')
    echo "perf: cannot record events as this user (kernel.perf_event_paranoid=$paranoid)"
    return 1
}

cargo_flamegraph_usable() {
    "$CARGO" flamegraph --version >/dev/null 2>&1 || {
        echo "cargo-flamegraph: not installed (cargo install flamegraph)"
        return 1
    }
}

use_perf=0
if [ "$BACKEND" = perf ]; then
    use_perf=1
elif [ "$BACKEND" = auto ]; then
    if [ "${FLAMEGRAPH_ROOT:-0}" = 1 ]; then
        cargo_flamegraph_usable && use_perf=1
    elif perf_usable && cargo_flamegraph_usable; then
        use_perf=1
    fi
fi

if [ "$use_perf" = 1 ]; then
    root_flag=""
    [ "${FLAMEGRAPH_ROOT:-0}" = 1 ] && root_flag="--root"
    case "${FLAMEGRAPH_CALLGRAPH:-fp}" in
        fp) callgraph="fp" ;;
        dwarf) callgraph="dwarf,16384" ;;
        *) echo "FLAMEGRAPH_CALLGRAPH must be 'fp' or 'dwarf'" >&2; exit 2 ;;
    esac
    echo ">> cargo flamegraph (perf, --call-graph $callgraph) profiling '$FILTER' for ${PROFILE_TIME}s"
    # --profile bench reuses the artefact built by `cargo bench`. cargo flamegraph splits --cmd on
    # whitespace, so BENCH_OUTPUT_DIR must not contain spaces; the -o inside --cmd keeps perf.data
    # next to the SVG instead of the repository root.
    "$CARGO" flamegraph --profile bench --bench spoe $root_flag --output "$SVG" \
        --cmd "record -e cpu-clock -F ${FLAMEGRAPH_FREQ:-99} --call-graph $callgraph -g -o $OUT_DIR/perf.data" \
        --title "SPOE agent benchmark: $FILTER" ${FLAMEGRAPH_FLAGS:-} \
        -- --bench --profile-time "$PROFILE_TIME" "$FILTER"
else
    echo ">> in-process profiler (pprof) profiling '$FILTER' for ${PROFILE_TIME}s"
    echo "   (set kernel.perf_event_paranoid <= 2 and install cargo-flamegraph to use perf)"
    SPOA_BENCH_PROFILER=pprof SPOA_BENCH_FLAMEGRAPH="$SVG" \
        "$CARGO" bench --bench spoe -- --profile-time "$PROFILE_TIME" "$FILTER"
fi

[ -s "$SVG" ] || { echo "flamegraph was not generated: $SVG" >&2; exit 1; }
echo "Flamegraph: $SVG ($(wc -c < "$SVG") bytes)"
