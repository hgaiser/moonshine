#!/usr/bin/env bash
#
# Run a full Phase 2 performance analysis session.
#
# This script:
#   1. Builds moonshine in release mode (with debug symbols for flamegraphs).
#   2. Launches moonshine with latency logging enabled.
#   3. Waits for the user to connect a client and stabilize (10s).
#   4. Records perf data and latency logs for 60 seconds.
#   5. Stops moonshine.
#   6. Generates a flamegraph SVG.
#   7. Parses latency logs and produces statistics + histograms.
#
# Prerequisites:
#   - perf (linux-tools-common)
#   - cargo install inferno  (for flamegraph SVG generation)
#   - python3 (for log parsing)
#   - python3 -m pip install matplotlib  (optional, for histogram plots)
#
# Usage:
#   ./tools/run_analysis.sh <config.toml> [DURATION_SECONDS]
#
# Output directory: analysis_<timestamp>/

set -euo pipefail

CONFIG="${1:?Usage: $0 <config.toml> [DURATION_SECONDS]}"
DURATION="${2:-60}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
TIMESTAMP="$(date +%Y%m%d_%H%M%S)"
OUT_DIR="$PROJECT_DIR/analysis_${TIMESTAMP}"

mkdir -p "$OUT_DIR"
echo "=== Phase 2 Performance Analysis ==="
echo "Output directory: $OUT_DIR"
echo "Duration: ${DURATION}s"
echo ""

# Record system info for reproducibility.
{
    echo "=== System Info ==="
    echo "Date: $(date -Iseconds)"
    echo "Kernel: $(uname -r)"
    echo "CPU: $(grep 'model name' /proc/cpuinfo | head -1 | cut -d: -f2 | xargs)"
    echo "CPU governor: $(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null || echo 'unknown')"

    if command -v vulkaninfo &>/dev/null; then
        echo ""
        echo "=== Vulkan Info ==="
        vulkaninfo --summary 2>/dev/null | head -20
    fi

    if command -v nvidia-smi &>/dev/null; then
        echo ""
        echo "=== GPU Info ==="
        nvidia-smi --query-gpu=name,driver_version --format=csv,noheader 2>/dev/null
    fi
} > "$OUT_DIR/system_info.txt" 2>&1
echo "[1/6] System info recorded."

# Step 1: Build release binary.
echo "[2/6] Building release binary..."
cd "$PROJECT_DIR"
cargo build --release 2>&1 | tail -5

BIN="$PROJECT_DIR/target/release/moonshine"
if [[ ! -x "$BIN" ]]; then
    echo "Error: Release binary not found at $BIN"
    exit 1
fi

# Step 2: Launch moonshine with latency logging.
echo "[3/6] Launching moonshine..."
RUST_LOG=moonshine::session::stream::video=debug \
    "$BIN" "$CONFIG" > "$OUT_DIR/session.log" 2>&1 &
MOONSHINE_PID=$!
echo "  PID: $MOONSHINE_PID"

# Give it time to initialize.
sleep 2

if ! kill -0 "$MOONSHINE_PID" 2>/dev/null; then
    echo "Error: moonshine exited early. Check $OUT_DIR/session.log"
    exit 1
fi

echo ""
echo "  --- Connect a moonlight client and start streaming. ---"
echo "  --- Press Enter once the stream is running and stable. ---"
read -r

# Step 3: Stabilize for 10 seconds.
echo "[4/6] Stabilizing for 10 seconds..."
sleep 10

# Capture per-thread CPU usage.
{
    echo "=== Per-thread CPU usage ==="
    top -H -b -n 3 -d 1 -p "$MOONSHINE_PID" 2>/dev/null || \
        ps -L -o pid,lwp,nlwp,%cpu,%mem,comm -p "$MOONSHINE_PID"
} > "$OUT_DIR/thread_cpu.txt" 2>&1

# Step 4: Record perf data.
echo "[5/6] Recording perf data for ${DURATION}s..."
if command -v perf &>/dev/null; then
    # Run perf record in the background and send SIGINT for clean termination.
    perf record -F 997 -g --call-graph dwarf -p "$MOONSHINE_PID" \
        -o "$OUT_DIR/perf.data" 2>"$OUT_DIR/perf_record.log" &
    PERF_PID=$!
    sleep "$DURATION"
    kill -INT "$PERF_PID" 2>/dev/null || true
    wait "$PERF_PID" 2>/dev/null || true
    echo "  perf record finished (see $OUT_DIR/perf_record.log)"

    # Generate flamegraph.
    if command -v inferno-flamegraph &>/dev/null; then
        # Prefer llvm-addr2line for faster/better Rust DWARF support.
        ADDR2LINE_FLAG=""
        if command -v llvm-addr2line &>/dev/null; then
            ADDR2LINE_FLAG="--addr2line=llvm-addr2line"
        fi
        perf script -i "$OUT_DIR/perf.data" $ADDR2LINE_FLAG 2>/dev/null | \
            inferno-collapse-perf 2>/dev/null | \
            inferno-flamegraph > "$OUT_DIR/flamegraph.svg" 2>/dev/null
        echo "  Flamegraph: $OUT_DIR/flamegraph.svg"
    else
        perf script -i "$OUT_DIR/perf.data" > "$OUT_DIR/perf_script.txt" 2>/dev/null
        echo "  perf script output saved (install inferno for SVG: cargo install inferno)"
    fi

    # Also generate perf report summary.
    perf report -i "$OUT_DIR/perf.data" --stdio --no-children --percent-limit 1.0 \
        > "$OUT_DIR/perf_report.txt" 2>/dev/null || true

    # Capture perf stat for the remaining time (brief CPU cycle stats).
    perf stat -p "$MOONSHINE_PID" -o "$OUT_DIR/perf_stat.txt" -- sleep 10 2>&1 || true
else
    echo "  Warning: perf not available, skipping flamegraph capture."
    echo "  Install: sudo apt install linux-tools-common linux-tools-$(uname -r)"
    sleep "$DURATION"
fi

# Step 5: Stop moonshine.
echo "[6/6] Stopping moonshine..."
kill "$MOONSHINE_PID" 2>/dev/null || true
wait "$MOONSHINE_PID" 2>/dev/null || true

# Step 6: Parse latency logs.
echo ""
echo "=== Latency Analysis ==="
python3 "$SCRIPT_DIR/parse_latency_logs.py" "$OUT_DIR/session.log" \
    --csv "$OUT_DIR/latency_data.csv" \
    2>&1 | tee "$OUT_DIR/latency_analysis.txt"

# Try to generate plots (non-fatal if matplotlib is missing).
python3 "$SCRIPT_DIR/parse_latency_logs.py" "$OUT_DIR/session.log" --plot 2>&1 && \
    mv -f latency_histograms.png "$OUT_DIR/" 2>/dev/null || \
    echo "(Histogram plot skipped — install matplotlib for plots)"

# Clean up large perf data file.
rm -f "$OUT_DIR/perf.data" "$OUT_DIR/perf.data.old"

echo ""
echo "=== Analysis Complete ==="
echo "Results in: $OUT_DIR/"
echo ""
ls -lh "$OUT_DIR/"
