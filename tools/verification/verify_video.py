#!/usr/bin/env python3
"""Opt-in host-side Moonshine encode smoke test; no real Moonlight client."""

import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import re
import shutil
import signal
import socket
import subprocess
import sys

VIDEO_PORT = 47998  # moonshine-bench uses VideoStreamConfig::default()
SESSION_RE = re.compile(r"Session \[(\d+) frames, ([0-9.]+) fps,")
GPU_RE = re.compile(r"gpu_encode: samples=(\d+)")


def check_port_free():
    # Advisory only: another process could bind after the socket closes.
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
        sock.bind(("127.0.0.1", VIDEO_PORT))


def resolve_executable(value):
    if "/" in value:
        path = Path(value).expanduser().resolve()
        return str(path) if path.is_file() and os.access(path, os.X_OK) else None
    return shutil.which(value)


def preflight(app, bench):
    problems = []
    app_path = resolve_executable(app)
    bench_path = resolve_executable(bench)
    if not app_path:
        problems.append(f"application not executable: {app}")
    if not bench_path:
        problems.append(f"benchmark binary not executable: {bench} (build moonshine-tools first)")
    nodes = sorted(Path("/dev/dri").glob("renderD*"))
    usable = [str(p) for p in nodes if os.access(p, os.R_OK | os.W_OK)]
    if not usable:
        problems.append("no accessible /dev/dri/renderD* device (render access is required)")
    try:
        check_port_free()
    except OSError as exc:
        problems.append(f"UDP 127.0.0.1:{VIDEO_PORT} unavailable ({exc}); possible active Moonshine stream")
    return {"app": app_path, "bench": bench_path, "render_nodes": usable, "problems": problems}


def summarize(log):
    """Read the completed session report, not an interval or frame-level message."""
    matches = list(SESSION_RE.finditer(log))
    match = matches[-1] if matches else None
    gpu = list(GPU_RE.finditer(log))
    return {
        "frames": int(match.group(1)) if match else 0,
        "fps": float(match.group(2)) if match else None,
        "gpu_timing_samples": int(gpu[-1].group(1)) if gpu else None,
    }


def run_benchmark(command, timeout_seconds):
    process = subprocess.Popen(
        command,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        errors="replace",
        start_new_session=True,
        env={**os.environ, "NO_COLOR": "1", "RUST_BACKTRACE": "1"},
    )
    timed_out = False
    try:
        log, _ = process.communicate(timeout=timeout_seconds)
    except subprocess.TimeoutExpired:
        timed_out = True
        # Only signal our own process group; never kill by process name.
        try:
            os.killpg(process.pid, signal.SIGINT)
        except ProcessLookupError:
            pass
        try:
            log, _ = process.communicate(timeout=10)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                log, _ = process.communicate(timeout=3)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                log, _ = process.communicate()
    return process.returncode, timed_out, log


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("doctor", "smoke"))
    parser.add_argument("--app", default="vkcube", help="executable that continuously renders frames")
    parser.add_argument("--bench", default="target/release/moonshine-bench")
    parser.add_argument("--resolution", default="1920x1080")
    parser.add_argument("--fps", type=int, default=60)
    parser.add_argument("--bitrate", type=int, default=20000000)
    parser.add_argument("--codec", choices=("h264", "hevc", "av1"), default="h264")
    parser.add_argument("--duration", type=int, default=12)
    parser.add_argument("--warmup", type=int, default=3)
    parser.add_argument("--hdr", action="store_true", help="HDR session mode; does not prove HDR content")
    parser.add_argument("--min-fps", type=float, default=0.0, help="optional environment-specific throughput gate")
    parser.add_argument("--output", type=Path, help="artifact directory (default: target/verification/<timestamp>)")
    args = parser.parse_args(argv)
    if args.duration <= args.warmup or args.warmup < 0 or args.fps <= 0 or args.bitrate <= 0 or args.min_fps < 0:
        parser.error("require duration > warmup >= 0, fps > 0, bitrate > 0 and min-fps >= 0")
    checks = preflight(args.app, args.bench)
    if args.action == "doctor":
        print(json.dumps(checks, indent=2))
        return 1 if checks["problems"] else 0
    if checks["problems"]:
        print(json.dumps(checks, indent=2), file=sys.stderr)
        return 2

    timestamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S%fZ")
    out = args.output or Path("target/verification") / f"smoke-{timestamp}"
    out.mkdir(parents=True, exist_ok=True)
    if (out / "report.json").exists() or (out / "moonshine-bench.log").exists():
        print(f"Refusing to overwrite existing verification evidence in {out}", file=sys.stderr)
        return 2
    command = [
        checks["bench"], "--duration", str(args.duration), "--warmup", str(args.warmup),
        "--resolution", args.resolution, "--fps", str(args.fps), "--bitrate", str(args.bitrate),
        "--codec", args.codec,
    ]
    if args.hdr:
        command.append("--hdr")
    command.append(checks["app"])
    try:
        sha = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True, stderr=subprocess.DEVNULL).strip()
    except (OSError, subprocess.CalledProcessError):
        sha = None
    try:
        code, timed_out, log = run_benchmark(command, args.duration + 75)
    except OSError as exc:
        code, timed_out, log = None, False, f"Could not execute benchmark: {exc}\n"
    (out / "moonshine-bench.log").write_text(log)
    stats = summarize(log)
    problems = []
    if timed_out:
        problems.append("timed out; process group was stopped")
    if code != 0:
        problems.append(f"benchmark exited with status {code}")
    if stats["frames"] == 0:
        problems.append("no completed frames in session summary")
    if stats["fps"] is not None and stats["fps"] < args.min_fps:
        problems.append(f"fps {stats['fps']} below requested minimum {args.min_fps}")
    report = {
        "verdict": "FAIL" if problems else "PASS",
        "problems": problems,
        "git_sha": sha,
        "command": command,
        "exit_code": code,
        "timed_out": timed_out,
        "results": stats,
        "preflight": checks,
        "note": "Host encode pipeline only; not a Moonlight client, network, decode, or TV test",
    }
    (out / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"report": str(out / "report.json"), "verdict": report["verdict"], "results": stats, "problems": problems}, indent=2))
    return 1 if problems else 0


if __name__ == "__main__":
    raise SystemExit(main())
