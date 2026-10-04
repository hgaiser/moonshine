#!/usr/bin/env python3
"""Run one reproducible measurement: optional GPU-bound game probe, optional
moonshine-bench stream, engine/board sampler; write a JSON record.

  runbench.py --name NAME --out DIR [--bench BIN] [--probe ITERS:TAPS[:FPS]]
              [--seconds S] [--warmup W] [--app vkcube|...] -- <bench args>

Without --bench the probe runs alone (streaming-disabled baseline).
"""
import argparse, json, os, re, subprocess, sys, threading, time, statistics

HERE = os.path.dirname(os.path.abspath(__file__))
ANSI = re.compile(r"\x1b\[[0-9;]*m")
KV = re.compile(r"(\w+)=(Some\()?([-\w.\"]+)\)?")


def kv(line):
    return {m.group(1): m.group(3).strip('"') for m in KV.finditer(line)}


def num(v):
    try:
        return float(v)
    except (TypeError, ValueError):
        return None


def parse_bench(path):
    lines = [ANSI.sub("", l) for l in open(path, errors="replace")]
    res = {"collecting_at": None}
    session_idx = None
    for i, l in enumerate(lines):
        if "Session [" in l and "frames" in l:
            session_idx = i
    if session_idx is not None:
        m = re.search(r"Session \[(\d+) frames, ([\d.]+) fps, ([\d.]+) Mbps encoded", lines[session_idx])
        res["frames"], res["fps"], res["encoded_mbps"] = int(m[1]), float(m[2]), float(m[3])
        block = "".join(lines[session_idx:session_idx + 14])
        for key in ("total", "submit", "enc_wait"):
            m = re.search(key + r":\s+avg=(\d+)us\s+min=(\d+)us\s+max=(\d+)us\s+.*?p50=(\d+)us\s+p95=(\d+)us\s+p99=(\d+)us", block, re.S)
            if m:
                res[key + "_us"] = dict(zip(["avg", "min", "max", "p50", "p95", "p99"], map(int, m.groups())))
        m = re.search(r"avg breakdown: (.*)", block)
        if m:
            res["breakdown_us"] = {k: int(v) for k, v in re.findall(r"(\w+)=(\d+)us", m[1])}
        m = re.search(r"stale compositor frames dropped: (\d+)", block)
        if m:
            res["stale_dropped"] = int(m[1])
        m = re.search(r"frame size: avg=(\d+)B", block)
        if m:
            res["frame_bytes_avg"] = int(m[1])
    # Steady windows: drop the first diagnostic window (warmup overlap).
    def windows(tag):
        return [kv(l) for l in lines if tag in l]
    pw = windows("PyroWave GPU timestamps")[1:]
    if pw:
        keys = ["dwt_gpu_ms", "quant_gpu_ms", "analyze_gpu_ms", "resolve_gpu_ms", "packing_gpu_ms",
                "scaler_conversion_gpu_ms", "pyrowave_stage_gpu_ms_per_delivered_frame"]
        res["pyrowave_gpu_ms"] = {k: round(statistics.fmean([num(w[k]) for w in pw if num(w.get(k)) is not None]), 4)
                                  for k in keys if any(num(w.get(k)) is not None for w in pw)}
    cap = windows("Video capture resources")[1:]
    if cap:
        tot = lambda k: sum(int(num(w.get(k)) or 0) for w in cap)
        res["capture"] = {k: tot(k) for k in ("captured_frames", "direct_export_frames", "composited_frames",
                                               "pre_render_rejected", "stale_after_render")}
        g = [num(w.get("compositor_gpu_ms_per_composited_capture")) for w in cap]
        g = [x for x in g if x is not None]
        if g:
            res["capture"]["compositor_gpu_ms_per_composited_capture"] = round(statistics.fmean(g), 4)
        res["capture"]["timer_supported"] = cap[-1].get("compositor_gpu_timer_supported")
        res["capture"]["path"] = cap[-1].get("capture_path")
    rej = windows("Video direct-export rejections")[1:]
    if rej:
        agg = {}
        for w in rej:
            for k, v in w.items():
                if k.startswith("direct_reject_") and int(v):
                    agg[k[len("direct_reject_"):]] = agg.get(k[len("direct_reject_"):], 0) + int(v)
        res["direct_rejections"] = agg
    pipe = windows("Video pipeline summary")[1:]
    if pipe:
        for k in ("convert_us", "queue_to_readback_us", "submit_us"):
            vals = [num(w.get(k)) for w in pipe if num(w.get(k)) is not None]
            if vals:
                res.setdefault("pipeline", {})[k] = round(statistics.fmean(vals), 1)
        res.setdefault("pipeline", {})["stale_frames_dropped"] = sum(int(num(w.get("stale_frames_dropped")) or 0) for w in pipe)
    errs = [l.strip()[:300] for l in lines if " ERROR " in l and "x11rb" not in l and "Xwayland terminated" not in l]
    if errs:
        res["errors"] = errs[:5]
    return res


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--name", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--bench")
    ap.add_argument("--probe")
    ap.add_argument("--seconds", type=float, default=20)
    ap.add_argument("--warmup", type=float, default=5)
    ap.add_argument("--app", default="vkcube")
    ap.add_argument("--env", action="append", default=[])
    ap.add_argument("rest", nargs=argparse.REMAINDER)
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)
    base = os.path.join(a.out, a.name)
    bench_args = [x for x in a.rest if x != "--"]
    env = dict(os.environ)
    for e in a.env:
        k, v = e.split("=", 1)
        env[k] = v

    record = {"name": a.name, "bench": a.bench, "bench_args": bench_args, "probe": a.probe,
              "seconds": a.seconds, "warmup": a.warmup, "started": time.strftime("%Y-%m-%dT%H:%M:%S")}
    bench = None
    log = None
    if a.bench:
        res = re.search(r"--resolution\s+(\d+)x(\d+)", " ".join(bench_args))
        w, h = (res[1], res[2]) if res else ("1920", "1080")
        if a.app == "vkcube":
            app = ["vkcube", "--wsi", "wayland", "--width", w, "--height", h]
        else:
            app = a.app.split()
        total = int(a.warmup + a.seconds + 2)
        cmd = [a.bench, "--duration", str(total), "--warmup", str(int(a.warmup))] + bench_args + ["--"] + app
        record["cmd"] = cmd
        log = open(base + ".log", "w")
        bench = subprocess.Popen(cmd, stdout=log, stderr=subprocess.STDOUT, env=env)
        # Wait until frames are flowing before starting the measured window.
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            if "Session active" in open(base + ".log", errors="replace").read():
                break
            if bench.poll() is not None:
                break
            time.sleep(0.2)
    probe = None
    if a.probe:
        parts = a.probe.split(":")
        gpuload = os.environ.get("GPULOAD", os.path.join(HERE, "build", "gpuload"))
        pargs = [gpuload, "--iters", parts[0], "--taps", parts[1],
                 "--seconds", str(a.seconds), "--warmup", str(a.warmup), "--json", base + ".probe.json"]
        if len(parts) > 2:
            pargs += ["--fps", parts[2]]
        probe = subprocess.Popen(pargs, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    sampler = subprocess.run([sys.executable, os.path.join(HERE, "gpusample.py"), "--seconds", str(a.seconds - 1),
                              "--delay", str(a.warmup + 0.5), "--out", base + ".gpu.json"],
                             capture_output=True)
    if probe:
        probe.wait()
        try:
            record["probe_result"] = json.load(open(base + ".probe.json"))
        except Exception as e:
            record["probe_result"] = {"error": str(e)}
    if bench:
        bench.wait()
        log.close()
        record["bench_exit"] = bench.returncode
        record["bench_result"] = parse_bench(base + ".log")
    try:
        record["gpu"] = json.load(open(base + ".gpu.json"))
    except Exception as e:
        record["gpu"] = {"error": str(e), "stderr": sampler.stderr.decode()[-500:]}
    json.dump(record, open(base + ".json", "w"), indent=1)
    br = record.get("bench_result", {})
    pr = record.get("probe_result", {})
    g = record["gpu"].get("process_engine_percent", {}) if isinstance(record["gpu"], dict) else {}
    board = record["gpu"].get("board", {}) if isinstance(record["gpu"], dict) else {}
    print(json.dumps({
        "name": a.name,
        "fps": br.get("fps"), "total_avg_us": br.get("total_us", {}).get("avg"),
        "convert_us": br.get("breakdown_us", {}).get("convert"),
        "pw_gpu_ms": br.get("pyrowave_gpu_ms", {}).get("pyrowave_stage_gpu_ms_per_delivered_frame"),
        "capture": br.get("capture"), "rejections": br.get("direct_rejections"),
        "bench_engines": g.get("moonshine-bench"), "app_engines": g.get("vkcube"),
        "probe_fps": pr.get("fps"), "probe_gpu_ms": pr.get("gpu_ms", {}).get("mean"),
        "probe_p99_ms": pr.get("frametime_ms", {}).get("p99"), "probe_1pct_low": pr.get("one_percent_low_fps"),
        "power_w": board.get("power_w", {}).get("mean"), "gfxclk": board.get("gfxclk_mhz", {}).get("mean"),
        "umc": board.get("umc_activity", {}).get("mean"), "gfx_act": board.get("gfx_activity", {}).get("mean"),
        "errors": br.get("errors"),
    }))


if __name__ == "__main__":
    main()
