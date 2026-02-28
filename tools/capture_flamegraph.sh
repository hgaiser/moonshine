#!/usr/bin/env bash
#
# Capture a flame graph from a running moonshine instance.
#
# Prerequisites:
#   cargo install flamegraph
#   # OR: install perf and inferno (perf record + inferno-flamegraph)
#
# Usage:
#   # Option 1: Launch moonshine under flamegraph (simplest)
#   ./tools/capture_flamegraph.sh run config.toml
#
#   # Option 2: Attach to a running moonshine process
#   ./tools/capture_flamegraph.sh attach <PID> [DURATION_SECONDS]
#
# Output:
#   flamegraph_<timestamp>.svg

set -euo pipefail

DURATION=${3:-60}  # Default: 60 seconds

usage() {
    echo "Usage:"
    echo "  $0 run <config.toml>           Launch moonshine and capture flamegraph"
    echo "  $0 attach <PID> [DURATION]     Attach to running PID (default: ${DURATION}s)"
    exit 1
}

check_tools() {
    if ! command -v perf &>/dev/null; then
        echo "Error: 'perf' not found. Install linux-tools-common or equivalent."
        exit 1
    fi
}

timestamp() {
    date +%Y%m%d_%H%M%S
}

cmd_run() {
    local config="$1"
    local out="flamegraph_$(timestamp).svg"

    if command -v cargo-flamegraph &>/dev/null || cargo flamegraph --help &>/dev/null 2>&1; then
        echo "Using cargo-flamegraph..."
        echo "Output: $out"
        RUST_LOG=moonshine::session::stream::video=debug \
            cargo flamegraph --release --output "$out" -- "$config"
    else
        echo "cargo-flamegraph not found, using perf + inferno..."
        check_tools

        echo "Building release binary..."
        cargo build --release

        local bin
        bin=$(cargo metadata --format-version 1 2>/dev/null | python3 -c "
import sys, json
m = json.load(sys.stdin)
for p in m['packages']:
    if p['name'] == 'moonshine':
        print(m['target_directory'] + '/release/moonshine')
        break
" 2>/dev/null || echo "target/release/moonshine")

        echo "Recording perf data for ${DURATION}s..."
        RUST_LOG=moonshine::session::stream::video=debug \
            perf record -F 997 -g --call-graph dwarf -o perf.data -- timeout "${DURATION}" "$bin" "$config" || true

        echo "Generating flamegraph..."
        local addr2line_flag=""
        if command -v llvm-addr2line &>/dev/null; then
            addr2line_flag="--addr2line=llvm-addr2line"
        fi
        if command -v inferno-flamegraph &>/dev/null; then
            perf script -i perf.data $addr2line_flag | inferno-collapse-perf | inferno-flamegraph > "$out"
        else
            perf script -i perf.data $addr2line_flag > perf_script.txt
            echo "perf script output saved to perf_script.txt"
            echo "Install inferno (cargo install inferno) to generate SVG, or use:"
            echo "  cat perf_script.txt | stackcollapse-perf.pl | flamegraph.pl > $out"
            return
        fi

        rm -f perf.data perf.data.old
        echo "Flamegraph saved to: $out"
    fi
}

cmd_attach() {
    local pid="$1"
    local duration="${2:-$DURATION}"
    local out="flamegraph_pid${pid}_$(timestamp).svg"

    check_tools

    if ! kill -0 "$pid" 2>/dev/null; then
        echo "Error: PID $pid not found or not accessible."
        exit 1
    fi

    echo "Recording perf data for PID $pid for ${duration}s..."
    perf record -F 997 -g --call-graph dwarf -p "$pid" -o perf.data -- sleep "$duration"

    echo "Generating flamegraph..."
    local addr2line_flag=""
    if command -v llvm-addr2line &>/dev/null; then
        addr2line_flag="--addr2line=llvm-addr2line"
    fi
    if command -v inferno-flamegraph &>/dev/null; then
        perf script -i perf.data $addr2line_flag | inferno-collapse-perf | inferno-flamegraph > "$out"
        rm -f perf.data perf.data.old
        echo "Flamegraph saved to: $out"
    else
        perf script -i perf.data $addr2line_flag > perf_script.txt
        rm -f perf.data perf.data.old
        echo "perf script saved to perf_script.txt"
        echo "Install inferno (cargo install inferno) to convert to SVG."
    fi
}

# Also capture per-thread CPU usage snapshot.
capture_thread_cpu() {
    local pid="$1"
    local out="thread_cpu_$(timestamp).txt"

    echo "Capturing per-thread CPU snapshot for PID $pid..."
    {
        echo "=== Per-thread CPU usage for moonshine (PID $pid) ==="
        echo "Snapshot at: $(date -Iseconds)"
        echo ""
        top -H -b -n 1 -p "$pid" 2>/dev/null || ps -L -o pid,lwp,nlwp,%cpu,%mem,comm -p "$pid"
    } > "$out"
    echo "Thread CPU snapshot saved to: $out"
}

case "${1:-}" in
    run)
        [[ $# -ge 2 ]] || usage
        cmd_run "$2"
        ;;
    attach)
        [[ $# -ge 2 ]] || usage
        cmd_attach "$2" "${3:-$DURATION}"
        capture_thread_cpu "$2"
        ;;
    *)
        usage
        ;;
esac
