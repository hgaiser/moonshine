#!/usr/bin/env python3
"""Fork-only Moonlight/webOS launcher; video quality still needs observation."""

import argparse
from datetime import datetime, timezone
import json
import re
from pathlib import Path
import shutil
import subprocess
import sys
import uuid


def launch_command(device, app_id, host_uuid, host_app_id):
    payload = json.dumps({"host_uuid": str(uuid.UUID(host_uuid)), "host_app_id": str(host_app_id)})
    return ["ares-launch", "--device", device, app_id, "--params", payload]


def get_running(device):
    return subprocess.run(["ares-launch", "--device", device, "--running"], capture_output=True, text=True, timeout=20)


def running_apps(status):
    if status.returncode:
        raise RuntimeError("running-app query failed: " + status.stdout + status.stderr)
    lines = [line.strip() for line in status.stdout.splitlines() if line.strip()]
    if not lines:
        raise RuntimeError("empty running-app response")
    apps = []
    for line in lines:
        if re.fullmatch(r"\[Info\] Set target device\s*:\s*\S+", line):
            continue
        if not re.fullmatch(r"[A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]+)+", line):
            raise RuntimeError("unrecognized running-app response: " + line)
        apps.append(line)
    return apps


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("plan", "doctor", "start", "collect", "run", "restore-state"))
    parser.add_argument("--device", required=True, help="existing webOS CLI device alias, not an IP address")
    parser.add_argument("--app-id", required=True, help="Moonlight's installed webOS app ID")
    parser.add_argument("--host-uuid", help="UUID of Moonshine host as known to Moonlight")
    parser.add_argument("--host-app-id", type=int, help="positive integer app ID advertised by Moonshine")
    parser.add_argument("--out", type=Path, help="run's evidence directory (required for start/collect)")
    parser.add_argument("--confirm", action="store_true", help="explicitly allow modifying the TV's running app")
    parser.add_argument("--ssh-alias", help="explicit SSH alias for webOS status and guarded mute restoration")
    parser.add_argument("--host-pid", type=int, help="local Moonshine server PID")
    parser.add_argument("--host-port", type=int, default=47989)
    parser.add_argument("--journal-unit", help="systemd unit supplying host journal")
    parser.add_argument("--journal-scope", choices=("system", "user"), default="system")
    parser.add_argument("--duration", type=int, default=60, help="observation seconds, 1 to 3600")
    parser.add_argument("--interval", type=int, default=15, help="sample interval in seconds")
    parser.add_argument("--settings-confirmed", action="store_true", help="confirm client settings are preconfigured")
    args = parser.parse_args(argv)

    if args.action == "restore-state":
        if not (args.confirm and args.out and args.ssh_alias):
            parser.error("restore-state requires --confirm, --out and --ssh-alias")
        if not re.fullmatch(r"[A-Za-z0-9_.-]+", args.ssh_alias) or args.ssh_alias.startswith("-"):
            parser.error("invalid SSH alias")
        import bounded_tv
        try:
            lock, meta_file = bounded_tv._acquire_tv_lock(f"verify_tv restore-state {args.app_id}")
        except RuntimeError as exc:
            print(f"TV restoration deferred: {exc}", file=sys.stderr)
            return 2
        try:
            state = bounded_tv.state_guard(args.out, args.ssh_alias, args.device, args.app_id,
                                           lambda device: running_apps(get_running(device)))
            state.restore()
        except (OSError, RuntimeError, ValueError, subprocess.TimeoutExpired) as exc:
            print(f"TV restoration deferred: {exc}", file=sys.stderr)
            return 2
        finally:
            bounded_tv._release_tv_lock(lock, meta_file)
        print(json.dumps({"tv_state_restored": True, "next_action": "TV state restored; retain this run's evidence. No TV action is required."}))
        return 0

    if args.action in ("plan", "start", "run"):
        if not args.host_uuid or args.host_app_id is None or args.host_app_id <= 0:
            parser.error("plan/start/run require --host-uuid and positive --host-app-id")
        try:
            command = launch_command(args.device, args.app_id, args.host_uuid, args.host_app_id)
        except ValueError as exc:
            parser.error(f"invalid host UUID: {exc}")
        if args.action == "plan":
            print(json.dumps({"mode": "DRY_RUN", "command": command, "warning": "This only requests Moonlight launch; it does not prove a video stream"}, indent=2))
            return 0

    if args.action == "run":
        if not (args.confirm and args.settings_confirmed and args.out and args.ssh_alias and args.host_pid and args.journal_unit):
            parser.error("run requires --confirm, --settings-confirmed, --out, --ssh-alias, --host-pid and --journal-unit")
        if not (1 <= args.duration <= 3600 and 1 <= args.interval <= args.duration and 1 <= args.host_port <= 65535 and args.host_pid > 0):
            parser.error("invalid duration, interval, host PID or port")
        if not re.fullmatch(r"[A-Za-z0-9_.-]+", args.ssh_alias) or args.ssh_alias.startswith("-"):
            parser.error("invalid SSH alias")
        import bounded_tv
        return bounded_tv.run(args, main, get_running, running_apps)

    # Reject incomplete control requests before even querying the device.
    if args.action == "start" and (not args.confirm or args.out is None):
        parser.error("start requires --confirm and --out (no implicit TV control)")
    if args.action == "collect" and args.out is None:
        parser.error("collect requires --out")

    if not shutil.which("ares-launch"):
        print("ares-launch missing; install/configure webOS TV CLI first", file=sys.stderr)
        return 2
    try:
        status = get_running(args.device)
    except (OSError, subprocess.TimeoutExpired) as exc:
        print(f"Could not query TV state: {exc}", file=sys.stderr)
        return 2
    if status.returncode != 0:
        print(status.stdout + status.stderr, file=sys.stderr)
        return 2
    try:
        is_running = args.app_id in running_apps(status)
    except RuntimeError as exc:
        print(str(exc), file=sys.stderr)
        return 2
    if args.action == "doctor":
        print(json.dumps({"device": args.device, "app_id": args.app_id, "moonlight_running": is_running, "raw": status.stdout}, indent=2))
        return 0
    if args.action == "collect":
        if args.out is None:
            parser.error("collect requires --out")
        args.out.mkdir(parents=True, exist_ok=True)
        dest = args.out / "webos-running.txt"
        if dest.exists():
            print("Refusing to overwrite collected evidence", file=sys.stderr)
            return 2
        dest.write_text(status.stdout)
        print(json.dumps({"running": is_running, "evidence": str(dest), "stream_verified": False}, indent=2))
        return 0
    if is_running:
        print("Moonlight is already running: refusing to interrupt an active client", file=sys.stderr)
        return 2
    args.out.mkdir(parents=True, exist_ok=True)
    if any(args.out.iterdir()):
        print("Refusing to reuse a nonempty evidence directory", file=sys.stderr)
        return 2
    try:
        result = subprocess.run(command, capture_output=True, text=True, timeout=30)
    except (OSError, subprocess.TimeoutExpired) as exc:
        print(f"TV launch failed: {exc}", file=sys.stderr)
        return 2
    report = {
        "time_utc": datetime.now(timezone.utc).isoformat(),
        "device": args.device,
        "app_id": args.app_id,
        "command": command,
        "launch_exit_code": result.returncode,
        "launched": result.returncode == 0,
        "video_stream_verified": False,
        "note": "Launch request only; manual or client telemetry required for decode/HDR/display verification",
    }
    (args.out / "ares-launch.log").write_text(result.stdout + result.stderr)
    (args.out / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"report": str(args.out / "report.json"), "launch_exit_code": result.returncode, "stream_verified": False}, indent=2))
    return 0 if result.returncode == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
