#!/usr/bin/env python3
"""Sample amdgpu engine time per process (DRM fdinfo) plus board metrics.

Usage: gpusample.py --seconds S [--delay D] [--interval I] --out PATH

Per-process engine busy is computed from drm-engine-* ns counters, deduplicated
by drm-client-id, and reported as percent of wall time per engine (it is active
time, not clock-normalized work). Board metrics come from gpu_metrics v1.3:
average gfx activity, UMC (memory controller) activity, socket power and
average gfx/memory clocks.
"""
import argparse, json, os, re, struct, time, glob, statistics

CARD = os.environ.get("GPU_SYSFS") or next(
    (os.path.dirname(p) for p in sorted(glob.glob("/sys/class/drm/card*/device/gpu_metrics"))), "/sys/class/drm/card0/device")


def read_metrics():
    with open(f"{CARD}/gpu_metrics", "rb") as f:
        b = f.read()
    size, fmt, content = struct.unpack_from("<HBB", b, 0)
    if (fmt, content) != (1, 3):
        raise SystemExit(f"unsupported gpu_metrics {fmt}.{content}")
    gfx_act, umc_act, mm_act, power = struct.unpack_from("<HHHH", b, 16)
    clocks = struct.unpack_from("<7H", b, 40)
    gfx_acc, mem_acc = struct.unpack_from("<II", b, 80)
    return {
        "gfx_activity": gfx_act, "umc_activity": umc_act, "power_w": power,
        "gfxclk_mhz": clocks[0], "uclk_mhz": clocks[2],
        "gfx_acc": gfx_acc, "mem_acc": mem_acc,
    }


def read_clients():
    """Return {client_id: (pid, comm, {engine: ns})} for all DRM clients."""
    clients = {}
    for fdinfo in glob.glob("/proc/[0-9]*/fdinfo/*"):
        try:
            with open(fdinfo) as f:
                text = f.read()
        except OSError:
            continue
        if "drm-client-id" not in text:
            continue
        cid = None
        engines = {}
        for line in text.splitlines():
            if line.startswith("drm-client-id:"):
                cid = int(line.split()[1])
            elif line.startswith("drm-engine-") and line.endswith(" ns"):
                k, v = line.split(":", 1)
                engines[k[len("drm-engine-"):]] = int(v.split()[0])
        if cid is None or cid in clients:
            continue
        pid = int(fdinfo.split("/")[2])
        try:
            comm = open(f"/proc/{pid}/comm").read().strip()
        except OSError:
            comm = "?"
        clients[cid] = (pid, comm, engines)
    return clients


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--seconds", type=float, required=True)
    ap.add_argument("--delay", type=float, default=0)
    ap.add_argument("--interval", type=float, default=0.25)
    ap.add_argument("--out", required=True)
    a = ap.parse_args()
    time.sleep(a.delay)
    c0, t0 = read_clients(), time.monotonic()
    samples = []
    end = t0 + a.seconds
    while time.monotonic() < end:
        samples.append(read_metrics())
        time.sleep(a.interval)
    c1, t1 = read_clients(), time.monotonic()
    wall_ns = (t1 - t0) * 1e9
    groups = {}
    for cid, (pid, comm, eng1) in c1.items():
        eng0 = c0.get(cid, (pid, comm, {}))[2]
        g = groups.setdefault(comm, {})
        for e, v in eng1.items():
            d = v - eng0.get(e, 0)
            if d > 0:
                g[e] = g.get(e, 0) + d
    util = {comm: {e: round(100 * ns / wall_ns, 3) for e, ns in eng.items()} for comm, eng in groups.items() if eng}

    def stat(k):
        vals = [s[k] for s in samples]
        return {"mean": round(statistics.fmean(vals), 2), "min": min(vals), "max": max(vals)}

    out = {
        "seconds": round(t1 - t0, 3),
        "process_engine_percent": util,
        "board": {k: stat(k) for k in ("gfx_activity", "umc_activity", "power_w", "gfxclk_mhz", "uclk_mhz")},
        "samples": len(samples),
    }
    with open(a.out, "w") as f:
        json.dump(out, f, indent=1)
    print(json.dumps(out))


if __name__ == "__main__":
    main()
