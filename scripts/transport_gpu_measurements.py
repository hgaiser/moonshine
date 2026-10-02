#!/usr/bin/env python3
"""Sequential fixed-quality GPU/loopback A/B runs, with raw process/GPU samples."""
import argparse
import csv
import hashlib
import json
import os
from pathlib import Path
import subprocess
import socket
import time


def run(binary, name, case, args):
    label, codec, resolution, fps, bitrate, extra, no_gso = case
    prefix = args.output / f'{name}-gpu-{label}'
    command = [str(binary.resolve()), '--codec', codec, '--resolution', resolution,
               '--fps', str(fps), '--bitrate', str(bitrate), '--duration', str(args.duration),
               '--warmup', str(args.warmup), *extra, args.scene]
    env = os.environ.copy()
    env.pop('MOONSHINE_VIDEO_DISABLE_GSO', None)
    if no_gso:
        env['MOONSHINE_VIDEO_DISABLE_GSO'] = '1'
    started = time.monotonic()
    with prefix.with_suffix('.log').open('w') as log, prefix.with_suffix('.samples.csv').open('w') as samples:
        writer = csv.writer(samples)
        writer.writerow(['elapsed_s', 'process_cpu_ticks', 'rss_kib', 'gpu_busy_percent'])
        process = subprocess.Popen(command, env=env, stdout=log, stderr=subprocess.STDOUT)
        while True:
            pid, status, usage = os.wait4(process.pid, os.WNOHANG)
            if pid:
                process.returncode = os.waitstatus_to_exitcode(status)
                break
            try:
                stat = Path(f'/proc/{process.pid}/stat').read_text().rsplit(')', 1)[1].split()
                cpu = int(stat[11]) + int(stat[12])
                rss = next((line.split()[1] for line in Path(f'/proc/{process.pid}/status').read_text().splitlines() if line.startswith('VmRSS:')), '')
                gpu = Path(args.gpu_sensor).read_text().strip() if Path(args.gpu_sensor).exists() else ''
                writer.writerow([round(time.monotonic() - started, 6), cpu, rss, gpu])
                samples.flush()
            except (FileNotFoundError, ProcessLookupError):
                pass
            time.sleep(0.5)
    prefix.with_suffix('.resources.json').write_text(json.dumps({
        'argv': command, 'disable_gso': no_gso, 'exit': process.returncode,
        'wall_s': time.monotonic() - started, 'cpu_user_s': usage.ru_utime,
        'cpu_system_s': usage.ru_stime, 'max_rss_kib': usage.ru_maxrss,
        'cpu_ticks_per_second': os.sysconf('SC_CLK_TCK'),
    }, indent=2) + '\n')
    print(name, label, process.returncode, flush=True)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--before', required=True, type=Path)
    parser.add_argument('--after', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--duration', type=int, default=10)
    parser.add_argument('--warmup', type=int, default=2)
    parser.add_argument('--scene', default='/usr/bin/vkcube')
    parser.add_argument('--gpu-sensor', default='/sys/class/drm/card1/device/gpu_busy_percent')
    parser.add_argument('--after-first', action='store_true', help='Reverse paired order to investigate order/thermal effects')
    parser.add_argument('--only', default='', help='Run labels containing this substring')
    args = parser.parse_args()
    artifacts = {}
    for name, binary in [('before', args.before), ('after', args.after)]:
        data = binary.read_bytes()
        artifacts[name] = {'sha256': hashlib.sha256(data).hexdigest(),
                           'new_transport_contract': b'resource_release_completions' in data}
    if artifacts['before']['sha256'] == artifacts['after']['sha256']:
        raise SystemExit('Before/after executables are identical; rebuild with separate target directories.')
    if artifacts['before']['new_transport_contract'] or not artifacts['after']['new_transport_contract']:
        raise SystemExit('Executable provenance does not match baseline/remediation.')
    args.output.mkdir(parents=True, exist_ok=True)
    (args.output / 'artifacts.json').write_text(json.dumps(artifacts, indent=2) + '\n')
    # The production benchmark uses the session unit and video port. Refuse
    # to replace a live user's application or flood the occupied port.
    unit = subprocess.run(['systemctl', '--user', 'is-active', 'moonshine-session.service'], capture_output=True, text=True)
    if unit.stdout.strip() == 'active':
        raise SystemExit('A live session owns moonshine-session.service; finish it before benchmarking.')
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as probe:
        try:
            probe.bind(('127.0.0.1', 47998))
        except OSError as error:
            raise SystemExit(f'Video benchmark port is unavailable: {error}')
    cases = []
    for codec in ['h264', 'hevc', 'av1', 'pyrowave']:
        extra = ['--chroma', '444', '--full-range'] if codec == 'pyrowave' else []
        for resolution, fps in [('1920x1080', 60), ('1920x1080', 120), ('3840x2160', 60), ('3840x2160', 120)]:
            cases.append((f'{resolution}-{fps}-{codec}', codec, resolution, fps, 750_000_000, extra, False))
    for fec in ['off', 'fixed']:
        cases.append((f'4k120-hevc-encrypted-fec-{fec}', 'hevc', '3840x2160', 120, 750_000_000, ['--encrypt-video', '--fec-mode', fec], False))
    for codec in ['hevc', 'pyrowave']:
        extra = ['--chroma', '444', '--full-range'] if codec == 'pyrowave' else []
        cases.append((f'4k120-{codec}-no-gso', codec, '3840x2160', 120, 750_000_000, extra, True))
    for bitrate in [650_000_000, 900_000_000]:
        cases.append((f'4k120-hevc-{bitrate}', 'hevc', '3840x2160', 120, bitrate, [], False))
    for case in cases:
        if args.only and not any(label in case[0] for label in args.only.split(',')):
            continue
        for name, binary in ([('after', args.after), ('before', args.before)] if args.after_first else [('before', args.before), ('after', args.after)]):
            run(binary, name, case, args)
