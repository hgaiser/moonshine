#!/usr/bin/env python3
"""Opt-in host-side Moonshine encode smoke test; no real Moonlight client."""

import argparse
import contextlib
from datetime import datetime, timezone
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
import xml.etree.ElementTree as ET

VIDEO_PORT = 47998  # moonshine-bench uses VideoStreamConfig::default()
SERVERINFO_PORT = 47989
VCN_SAMPLE_SECONDS = 0.4
SESSION_RE = re.compile(r"Session \[(\d+) frames, ([0-9.]+) fps,")
GPU_RE = re.compile(r"gpu_encode: samples=(\d+)")
DRM_CLIENT_RE = re.compile(r"^drm-client-id:\s*(\S+)", re.MULTILINE)
DRM_VIDEO_ENGINE_RE = re.compile(r"^drm-engine-(dec|enc|vcn|uvd|jpeg):\s*(\d+)\s*ns", re.MULTILINE)


def default_hw_lock_path():
    override = os.environ.get("MOONSHINE_HW_LOCK_PATH")
    if override:
        return Path(override)
    state_home = Path(os.environ.get("XDG_STATE_HOME", Path.home() / ".local/state"))
    return state_home / "moonshine-bench" / "hw.lock"


def check_port_free():
    # Advisory only: another process could bind after the socket closes.
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
        sock.bind(("127.0.0.1", VIDEO_PORT))


def resolve_executable(value):
    if "/" in value:
        path = Path(value).expanduser().resolve()
        return str(path) if path.is_file() and os.access(path, os.X_OK) else None
    return shutil.which(value)


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def binary_provenance(bench_path):
    if not bench_path:
        return None
    resolved = str(Path(bench_path).resolve())
    return {
        "path": resolved,
        "sha256": sha256_file(resolved),
        "cargo_target_dir": os.environ.get("CARGO_TARGET_DIR"),
    }


def load_compare_sha256(target):
    path = Path(target).expanduser().resolve()
    if not path.is_file():
        raise ValueError(f"--compare-against path does not exist: {target}")
    if path.suffix == ".json":
        data = json.loads(path.read_text())
        sha = (data.get("preflight") or {}).get("bench_provenance", {}).get("sha256") or data.get("bench_sha256")
        if not sha:
            raise ValueError(f"report {target} has no recorded bench sha256")
        return sha
    return sha256_file(path)


def sample_vcn_clients(proc_root=Path("/proc")):
    """Return {(pid, client_id): (comm, total_video_engine_ns)} from DRM fdinfo."""
    readings = {}
    try:
        entries = list(proc_root.iterdir())
    except OSError:
        return readings
    for pid_dir in entries:
        if not pid_dir.name.isdigit():
            continue
        fdinfo_dir = pid_dir / "fdinfo"
        try:
            fd_entries = list(fdinfo_dir.iterdir())
        except OSError:
            continue
        comm = None
        for fd_file in fd_entries:
            try:
                text = fd_file.read_text(errors="replace")
            except OSError:
                continue
            if "drm-engine-" not in text:
                continue
            matches = DRM_VIDEO_ENGINE_RE.findall(text)
            if not matches:
                continue
            client_match = DRM_CLIENT_RE.search(text)
            client_id = client_match.group(1) if client_match else fd_file.name
            total_ns = sum(int(ns) for _, ns in matches)
            if comm is None:
                try:
                    comm = (pid_dir / "comm").read_text(errors="replace").strip()
                except OSError:
                    comm = "unknown"
            key = (int(pid_dir.name), client_id)
            if key not in readings or total_ns > readings[key][1]:
                readings[key] = (comm, total_ns)
    return readings


def detect_active_vcn(sample_seconds=VCN_SAMPLE_SECONDS, proc_root=Path("/proc")):
    """Detect processes actively consuming DRM video decode/encode engines."""
    first = sample_vcn_clients(proc_root)
    if sample_seconds > 0:
        time.sleep(sample_seconds)
    second = sample_vcn_clients(proc_root)
    active = []
    for key, (comm, ns_after) in second.items():
        ns_before = first[key][1] if key in first else 0
        delta_ns = ns_after - ns_before
        if delta_ns > 0:
            active.append({"pid": key[0], "comm": comm, "client_id": key[1], "delta_ns": delta_ns})
    active.sort(key=lambda item: (-item["delta_ns"], item["pid"]))
    return active


def check_moonshine_server_free(port=SERVERINFO_PORT):
    """Return active server status dict if Moonshine is streaming, else None."""
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{port}/serverinfo", timeout=1) as response:
            raw = response.read(256 * 1024)
    except (OSError, urllib.error.URLError):
        return None
    try:
        root = ET.fromstring(raw)
    except ET.ParseError as exc:
        return {"error": f"invalid serverinfo XML: {exc}"}
    state = root.findtext("state")
    current_game = root.findtext("currentgame")
    if (state, current_game) != ("MOONSHINE_SERVER_FREE", "0"):
        return {"state": state, "currentgame": current_game}
    return None


def read_lock_meta(lock_path):
    meta_path = Path(f"{lock_path}.meta")
    if not meta_path.is_file():
        return None
    try:
        return json.loads(meta_path.read_text())
    except (OSError, json.JSONDecodeError):
        return None


def check_hw_lock_free(lock_path):
    lock_path = Path(lock_path)
    if not lock_path.exists():
        return None
    try:
        with open(lock_path, "a") as handle:
            fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
            fcntl.flock(handle, fcntl.LOCK_UN)
        return None
    except OSError:
        return read_lock_meta(lock_path) or {"lock_path": str(lock_path)}


@contextlib.contextmanager
def acquire_hw_lock(lock_path, purpose):
    lock_path = Path(lock_path)
    lock_path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    meta_path = Path(f"{lock_path}.meta")
    handle = open(lock_path, "a")
    try:
        fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError as exc:
        handle.close()
        holder = read_lock_meta(lock_path)
        detail = f" (held by {json.dumps(holder)})" if holder else ""
        raise RuntimeError(f"host GPU benchmark lock unavailable: {exc}{detail}") from exc
    meta = {
        "pid": os.getpid(),
        "owner": "moonshine-verify-video",
        "purpose": purpose,
        "started_utc": datetime.now(timezone.utc).isoformat(),
    }
    try:
        meta_path.write_text(json.dumps(meta, indent=2) + "\n")
    except OSError:
        pass
    try:
        yield lock_path
    finally:
        try:
            meta_path.unlink(missing_ok=True)
        except OSError:
            pass
        try:
            fcntl.flock(handle, fcntl.LOCK_UN)
        finally:
            handle.close()


def preflight(app, bench, allow_vcn_busy=False, compare_against=None, lock_path=None):
    problems = []
    app_path = resolve_executable(app)
    bench_path = resolve_executable(bench)
    if not app_path:
        problems.append(f"application not executable: {app}")
    if not bench_path:
        problems.append(f"benchmark binary not executable: {bench} (build moonshine-tools first)")
    provenance = binary_provenance(bench_path)
    if compare_against and provenance:
        try:
            baseline_sha = load_compare_sha256(compare_against)
        except (OSError, ValueError, json.JSONDecodeError) as exc:
            problems.append(f"invalid --compare-against target: {exc}")
        else:
            if provenance["sha256"] == baseline_sha:
                problems.append(
                    f"benchmark binary sha256 ({provenance['sha256'][:12]}) matches --compare-against "
                    f"({compare_against}); possible shared CARGO_TARGET_DIR collision"
                )
    nodes = sorted(Path("/dev/dri").glob("renderD*"))
    usable = [str(p) for p in nodes if os.access(p, os.R_OK | os.W_OK)]
    if not usable:
        problems.append("no accessible /dev/dri/renderD* device (render access is required)")
    try:
        check_port_free()
    except OSError as exc:
        problems.append(f"UDP 127.0.0.1:{VIDEO_PORT} unavailable ({exc}); possible active Moonshine stream")
    active_server = check_moonshine_server_free()
    if active_server is not None:
        problems.append(f"production Moonshine server is not idle ({active_server}); refusing to contend with live stream")
    active_vcn = detect_active_vcn()
    if active_vcn and not allow_vcn_busy:
        summary = ", ".join(f"{item['comm']}(pid={item['pid']})" for item in active_vcn)
        problems.append(
            f"active VCN video decode/encode detected ({summary}); pause playback or pass --allow-vcn-busy"
        )
    resolved_lock = Path(lock_path) if lock_path is not None else default_hw_lock_path()
    lock_holder = check_hw_lock_free(resolved_lock)
    if lock_holder is not None:
        problems.append(f"host GPU benchmark lock held ({lock_holder})")
    return {
        "app": app_path,
        "bench": bench_path,
        "bench_provenance": provenance,
        "render_nodes": usable,
        "active_vcn": active_vcn,
        "moonshine_server_busy": active_server,
        "hw_lock": str(resolved_lock),
        "hw_lock_holder": lock_holder,
        "problems": problems,
    }


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
    parser.add_argument("--allow-vcn-busy", action="store_true", help="allow running while another process uses VCN")
    parser.add_argument("--compare-against", help="baseline binary or report.json; fail if sha256 matches")
    parser.add_argument("--output", type=Path, help="artifact directory (default: target/verification/<timestamp>)")
    args = parser.parse_args(argv)
    if args.duration <= args.warmup or args.warmup < 0 or args.fps <= 0 or args.bitrate <= 0 or args.min_fps < 0:
        parser.error("require duration > warmup >= 0, fps > 0, bitrate > 0 and min-fps >= 0")

    lock_path = default_hw_lock_path()
    if args.action == "doctor":
        checks = preflight(
            args.app,
            args.bench,
            allow_vcn_busy=args.allow_vcn_busy,
            compare_against=args.compare_against,
            lock_path=lock_path,
        )
        print(json.dumps(checks, indent=2))
        return 1 if checks["problems"] else 0

    try:
        lock_cm = acquire_hw_lock(lock_path, f"smoke {args.codec} {args.resolution}@{args.fps}")
        lock_cm.__enter__()
    except RuntimeError as exc:
        print(str(exc), file=sys.stderr)
        return 2

    try:
        checks = preflight(
            args.app,
            args.bench,
            allow_vcn_busy=args.allow_vcn_busy,
            compare_against=args.compare_against,
            lock_path=None,
        )
        # Ignore our own lock in preflight by clearing self-held lock checks.
        if checks.get("hw_lock_holder") and checks["hw_lock_holder"].get("pid") == os.getpid():
            checks["hw_lock_holder"] = None
            checks["problems"] = [p for p in checks["problems"] if not p.startswith("host GPU benchmark lock held")]
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
            "bench_sha256": (checks.get("bench_provenance") or {}).get("sha256"),
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
    finally:
        lock_cm.__exit__(None, None, None)


if __name__ == "__main__":
    raise SystemExit(main())
