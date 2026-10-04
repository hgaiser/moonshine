#!/usr/bin/env python3
"""Run the benchmark matrix for one build.

  matrix.py --label LABEL --bench BIN [--pyrowave-lib PATH] [--only REGEX]
            [--modes solo,probe] [--seconds S] [--set NAME]

Each case runs once without the game probe (stream cost at natural clocks)
and once with the GPU-bound probe (game interference and stream
sustainability under contention).
"""
import argparse, json, os, re, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
PROBE = "1800:96"
MPV = "mpv --really-quiet --fs --loop-file=inf --no-audio --hwdec=auto --vo=gpu-next --gpu-api=vulkan {content}"

R1080, R1440, R4K = "1920x1080", "2560x1440", "3840x2160"
# name, bench args, app
CASES = [
    # Conventional (pixelforge / Vulkan Video), direct capture.
    ("h264-1080p120-sdr420", f"--codec h264 --resolution {R1080} --fps 120 --bitrate 40000000", "vkcube"),
    ("hevc-1080p120-sdr420", f"--codec hevc --resolution {R1080} --fps 120 --bitrate 40000000", "vkcube"),
    ("hevc-1440p120-sdr420", f"--codec hevc --resolution {R1440} --fps 120 --bitrate 60000000", "vkcube"),
    ("hevc-4k60-sdr420", f"--codec hevc --resolution {R4K} --fps 60 --bitrate 80000000", "vkcube"),
    ("hevc-4k120-sdr420", f"--codec hevc --resolution {R4K} --fps 120 --bitrate 80000000", "vkcube"),
    ("hevc-4k120-hdr420", f"--codec hevc --resolution {R4K} --fps 120 --bitrate 80000000 --hdr", "vkcube"),
    ("av1-4k120-sdr420", f"--codec av1 --resolution {R4K} --fps 120 --bitrate 80000000", "vkcube"),
    ("hevc-4k120-sdr420-comp", f"--codec hevc --resolution {R4K} --fps 120 --bitrate 80000000 --composited", "vkcube"),
    ("hevc-4k120-sdr420-convgfx", f"--codec hevc --resolution {R4K} --fps 120 --bitrate 80000000 --conversion-queue graphics", "vkcube"),
    ("hevc-4k120-sdr420-cursor", f"--codec hevc --resolution {R4K} --fps 120 --bitrate 80000000 --cursor moving", "vkcube"),
    # PyroWave, direct capture.
    ("pw-1080p120-sdr420", f"--codec pyrowave --resolution {R1080} --fps 120 --bitrate 200000000 --chroma 420", "vkcube"),
    ("pw-1440p120-sdr444", f"--codec pyrowave --resolution {R1440} --fps 120 --bitrate 300000000 --chroma 444", "vkcube"),
    ("pw-4k60-hdr444", f"--codec pyrowave --resolution {R4K} --fps 60 --bitrate 400000000 --chroma 444 --hdr", "vkcube"),
    ("pw-4k120-sdr444", f"--codec pyrowave --resolution {R4K} --fps 120 --bitrate 400000000 --chroma 444", "vkcube"),
    ("pw-4k120-hdr444", f"--codec pyrowave --resolution {R4K} --fps 120 --bitrate 400000000 --chroma 444 --hdr", "vkcube"),
    ("pw-4k120-hdr420", f"--codec pyrowave --resolution {R4K} --fps 120 --bitrate 400000000 --chroma 420 --hdr", "vkcube"),
    ("pw-4k120-hdr444-compute", f"--codec pyrowave --resolution {R4K} --fps 120 --bitrate 400000000 --chroma 444 --hdr --pyrowave-queue compute", "vkcube"),
    ("pw-4k120-hdr444-comp", f"--codec pyrowave --resolution {R4K} --fps 120 --bitrate 400000000 --chroma 444 --hdr --composited", "vkcube"),
    ("pw-4k120-hdr444-cursor", f"--codec pyrowave --resolution {R4K} --fps 120 --bitrate 400000000 --chroma 444 --hdr --cursor moving", "vkcube"),
    # High-entropy content (natural-image pan) for codec stage cost.
    ("pw-4k120-sdr444-pan", f"--codec pyrowave --resolution {R4K} --fps 120 --bitrate 400000000 --chroma 444", "mpv"),
    ("hevc-4k120-sdr420-pan", f"--codec hevc --resolution {R4K} --fps 120 --bitrate 80000000", "mpv"),
]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--label", required=True)
    ap.add_argument("--bench", required=True)
    ap.add_argument("--pyrowave-lib")
    ap.add_argument("--only")
    ap.add_argument("--skip")
    ap.add_argument("--modes", default="solo,probe")
    ap.add_argument("--seconds", type=float, default=15)
    ap.add_argument("--warmup", type=float, default=5)
    ap.add_argument("--out-dir", default="gpu-interference-runs")
    ap.add_argument("--content", help="high-detail clip for the *-pan cases (skipped without it)")
    a = ap.parse_args()
    out = os.path.join(a.out_dir, a.label)
    os.makedirs(out, exist_ok=True)
    supports_cursor = b"--cursor" in subprocess.run([a.bench, "--help"], capture_output=True).stdout
    for name, args, app in CASES:
        if a.only and not re.search(a.only, name):
            continue
        if a.skip and re.search(a.skip, name):
            continue
        if app != "vkcube" and not a.content:
            print(json.dumps({"name": name, "skipped": "no --content clip"}), flush=True)
            continue
        if "--cursor" in args and not supports_cursor:
            print(json.dumps({"name": name, "skipped": "bench lacks --cursor"}), flush=True)
            continue
        for mode in a.modes.split(","):
            cmd = [sys.executable, os.path.join(HERE, "runbench.py"), "--name", f"{name}.{mode}", "--out", out,
                   "--bench", a.bench, "--seconds", str(a.seconds), "--warmup", str(a.warmup),
                   "--app", "vkcube" if app == "vkcube" else MPV.format(content=a.content)]
            if a.pyrowave_lib:
                cmd += ["--env", f"MOONSHINE_PYROWAVE_LIBRARY={a.pyrowave_lib}"]
            if mode == "probe":
                cmd += ["--probe", PROBE]
            cmd += ["--"] + args.split()
            r = subprocess.run(cmd, capture_output=True, text=True)
            print(r.stdout.strip() or r.stderr[-800:], flush=True)


if __name__ == "__main__":
    main()
