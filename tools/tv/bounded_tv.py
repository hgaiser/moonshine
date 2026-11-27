"""Bounded evidence collection for the fork's webOS launcher."""

import contextlib
from datetime import datetime, timezone
import fcntl
import io
import json
import os
from pathlib import Path
import re
import shlex
import signal
import subprocess
import sys
import time
import urllib.request
import uuid
import xml.etree.ElementTree as ET

from tv_state import TvState


def default_lock_path():
    override = os.environ.get("TV_GUI_LOCK_PATH")
    if override:
        return override
    state_home = Path(os.environ.get("XDG_STATE_HOME", Path.home() / ".local/state"))
    return str(state_home / "homelab-tv" / "gui.lock")


LOCK_PATH = default_lock_path()


def _acquire_tv_lock(purpose):
    lock_file = Path(LOCK_PATH)
    lock_file.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    meta_file = Path(f"{LOCK_PATH}.meta")
    lock = None
    try:
        lock = open(lock_file, "a")
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError as exc:
        if lock is not None:
            lock.close()
        holder = ""
        if meta_file.is_file():
            try:
                holder = f" (held by {meta_file.read_text().strip()})"
            except OSError:
                pass
        raise RuntimeError(f"TV GUI lock unavailable ({LOCK_PATH}): {exc}{holder}") from exc
    meta = {
        "pid": os.getpid(),
        "owner": "moonshine-verify-tv",
        "purpose": purpose,
        "started_utc": now(),
    }
    try:
        meta_file.write_text(json.dumps(meta, indent=2) + "\n")
    except OSError:
        pass
    return lock, meta_file


def _release_tv_lock(lock, meta_file):
    try:
        meta_file.unlink(missing_ok=True)
    except OSError:
        pass
    lock.close()


def now():
    return datetime.now(timezone.utc).isoformat()


def checked(command):
    result = subprocess.run(command, capture_output=True, text=True, timeout=20)
    if result.returncode:
        raise RuntimeError(f"command failed {command[0]}: {result.stderr or result.stdout}")
    return result.stdout


def host_status(pid, port, host_uuid):
    inodes = set()
    for fd in Path(f"/proc/{pid}/fd").iterdir():
        match = re.fullmatch(r"socket:\[(\d+)\]", os.readlink(fd))
        if match:
            inodes.add(match.group(1))
    listeners = set()
    for name in ("tcp", "tcp6"):
        lines = Path(f"/proc/net/{name}").read_text().splitlines()
        if not lines or "local_address" not in lines[0]:
            raise RuntimeError("invalid procfs TCP table")
        for line in lines[1:]:
            fields = line.split()
            if len(fields) < 10:
                raise RuntimeError("invalid procfs TCP entry")
            _, hexport = fields[1].split(":")
            if int(hexport, 16) == port and fields[3] == "0A":
                listeners.add(fields[9])
    if not listeners or not listeners <= inodes:
        raise RuntimeError("host listener absent or not exclusively owned by specified PID")
    with urllib.request.urlopen(f"http://127.0.0.1:{port}/serverinfo", timeout=5) as response:
        raw = response.read(1024 * 1024 + 1)
    if len(raw) > 1024 * 1024:
        raise RuntimeError("serverinfo too large")
    root = ET.fromstring(raw)
    if root.tag != "root" or root.get("status_code") != "200":
        raise RuntimeError("invalid serverinfo response")
    identity = root.findtext("uniqueid")
    if not identity or uuid.UUID(identity) != uuid.UUID(host_uuid):
        raise RuntimeError("serverinfo host UUID mismatch")
    state, game = root.findtext("state"), root.findtext("currentgame")
    if (state, game) != ("MOONSHINE_SERVER_FREE", "0"):
        raise RuntimeError(f"host session active or unknown: {state!r}, {game!r}")
    return raw.decode("utf-8")


def tv_request(alias, endpoint, data):
    raw = checked(["ssh", "-tt", "-o", "BatchMode=yes", "-o", "ConnectTimeout=5", alias,
                   "luna-send", "-n", "1", "-w", "3000", "luna://" + endpoint, shlex.quote(json.dumps(data))])
    result = json.loads(raw)
    if not isinstance(result, dict) or result.get("returnValue") is not True:
        raise RuntimeError(f"TV service unavailable: {endpoint}")
    return result


def state_guard(out, alias, device, app_id, running):
    return TvState(out, alias, app_id, lambda: tv_status(alias),
                   lambda muted: tv_request(alias, "com.webos.audio/setMuted", {"muted": muted}),
                   lambda: running(device))


def tv_status(alias):
    result = {}
    for name, endpoint in (("power", "com.webos.service.tvpower/power2/getPowerState"),
                           ("foreground", "com.webos.applicationManager/getForegroundAppInfo"),
                           ("video", "com.webos.service.videooutput/getStatus"),
                           ("volume", "com.webos.audio/getVolume")):
        result[name] = tv_request(alias, endpoint, {})
    return result


def require_idle_tv(tv):
    if tv["volume"].get("muteStatus") is not True:
        raise RuntimeError("TV mute is not confirmed")
    foreground = tv["foreground"].get("appId")
    if foreground != "com.webos.app.home":
        raise RuntimeError("TV is not on its home screen")
    videos = tv["video"].get("video")
    if not isinstance(videos, list) or not videos or any(
        not isinstance(video, dict) or video.get("connected") is not False for video in videos
    ):
        raise RuntimeError("TV video output is active or unknown")


def run(args, cli, get_running, running_apps):
    try:
        lock, meta_file = _acquire_tv_lock(f"verify_tv run {args.app_id}")
    except RuntimeError as exc:
        print(str(exc), file=sys.stderr)
        return 2
    out = args.out
    created = False
    report = {"started_utc": now(), "phase": "preflight", "video_stream_verified": False,
              "playback_verified": False, "cleanup": "No automatic stop. Check ownership and stop through Moonlight's normal UI if needed.",
              "settings": "Operator confirmed client settings are preconfigured; settings are not read or changed.",
              "launch_request_accepted": False}
    launch_attempted = False
    state = None
    exit_code = 0
    old_signals = {value: signal.getsignal(value) for value in (signal.SIGINT, signal.SIGTERM)}

    def interrupt(signum, frame):
        raise KeyboardInterrupt

    try:
        out.mkdir(parents=True, exist_ok=False, mode=0o700)
        created = True
        for value in old_signals:
            signal.signal(value, interrupt)
        base = ["--device", args.device, "--app-id", args.app_id, "--host-uuid", args.host_uuid, "--host-app-id", str(args.host_app_id)]

        def invoke(action, extra=()):
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                rc = cli([action, *base, *extra])
            (out / f"{action}.json").write_text(output.getvalue())
            if rc:
                raise RuntimeError(f"{action} failed with exit code {rc}")
            return json.loads(output.getvalue())

        invoke("plan")
        if invoke("doctor")["moonlight_running"]:
            raise RuntimeError("Moonlight is already running")
        (out / "host-preflight.xml").write_text(host_status(args.host_pid, args.host_port, args.host_uuid))
        tv = tv_status(args.ssh_alias)
        (out / "tv-preflight.json").write_text(json.dumps(tv, indent=2) + "\n")
        state = state_guard(out, args.ssh_alias, args.device, args.app_id,
                            lambda device: running_apps(get_running(device)))
        state.begin()
        tv = tv_status(args.ssh_alias)
        require_idle_tv(tv)
        since = now()
        journal = ["journalctl"]
        if args.journal_scope == "user":
            journal.append("--user")
        journal += ["--unit", args.journal_unit, "--since", since, "--no-pager", "--output", "short-iso-precise"]
        (out / "journal-preflight.log").write_text(checked(journal))
        (out / "host-before-launch.xml").write_text(host_status(args.host_pid, args.host_port, args.host_uuid))
        tv = tv_status(args.ssh_alias)
        (out / "tv-before-launch.json").write_text(json.dumps(tv, indent=2) + "\n")
        require_idle_tv(tv)
        report["phase"] = "launch"
        launch_attempted = True
        invoke("start", ["--confirm", "--out", str(out / "launch")])
        report["launch_request_accepted"] = True
        report["phase"] = "observing"
        deadline = time.monotonic() + args.duration
        index = 0
        while True:
            sample = {"time_utc": now(), "tv": tv_status(args.ssh_alias),
                      "running_apps": running_apps(get_running(args.device))}
            (out / f"sample-{index:04d}.json").write_text(json.dumps(sample, indent=2) + "\n")
            (out / f"journal-{index:04d}.log").write_text(checked(journal))
            index += 1
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                break
            time.sleep(min(args.interval, remaining))
        report["phase"] = "complete"
    except KeyboardInterrupt:
        report["phase"] = "interrupted"
        report["error"] = "Interrupted. The launch may have succeeded; inspect and clean up manually."
        exit_code = 130
    except (OSError, RuntimeError, ValueError, ET.ParseError, subprocess.TimeoutExpired) as exc:
        report["error"] = str(exc)
        print(f"TV test stopped: {exc}", file=sys.stderr)
        exit_code = 2
    finally:
        for value in old_signals:
            signal.signal(value, signal.SIG_IGN)
        if state is not None and state.state is not None:
            try:
                state.restore()
                report["tv_state_restored"] = True
            except (OSError, RuntimeError, ValueError, subprocess.TimeoutExpired) as exc:
                report["tv_state_restored"] = False
                report["state_restoration_error"] = str(exc)
                report["next_action"] = "Stop only the owned test through Moonlight's normal UI, then run: " + state.next_action(args.device)
                if exit_code == 0:
                    exit_code = 2
        if created:
            report.setdefault("next_action", "Inspect client and host evidence before claiming playback or performance; no merge recommendation follows from collection alone.")
            report["finished_utc"] = now()
            report["launch_attempted"] = launch_attempted
            (out / "run-report.json").write_text(json.dumps(report, indent=2) + "\n")
            print(json.dumps({"report": str(out / "run-report.json"), "phase": report["phase"], "cleanup": report["cleanup"], "next_action": report["next_action"]}))
        _release_tv_lock(lock, meta_file)
        for value, handler in old_signals.items():
            signal.signal(value, handler)
    return exit_code
