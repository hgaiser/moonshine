#!/usr/bin/env python3
"""Summarize raw paired GPU logs without discarding regression signals."""
import argparse
import csv
import json
from pathlib import Path
import re


def read(prefix):
    log = prefix.with_suffix('.log').read_text()
    resources = json.loads(prefix.with_suffix('.resources.json').read_text())
    session = re.search(r'Session \[(\d+) frames, ([\d.]+) fps, ([\d.]+) Mbps encoded, ([\d.]+) Mbps submitted UDP payload\]', log)
    if resources['exit'] or not session:
        return {'exit': resources['exit'], 'valid': False}
    end = log[session.end():]
    total = re.search(r'total: +avg=(\d+)us +min=(\d+)us +max=(\d+)us', end)
    age = re.search(r'p50=(\d+)us +p95=(\d+)us +p99=(\d+)us', end)
    stage = re.search(r'pkt=(\d+)us +enqueue=(\d+)us +send=(\d+)us', end)
    stale = re.search(r'stale compositor frames dropped: (\d+)', end)
    result = dict(zip(['frames', 'fps', 'encoded_mbps', 'submitted_udp_payload_mbps'], map(float, session.groups())))
    result.update(dict(zip(['age_avg_us', 'age_min_us', 'age_max_us'], map(int, total.groups()))))
    result.update(dict(zip(['age_p50_us', 'age_p95_us', 'age_p99_us'], map(int, age.groups()))))
    result.update(dict(zip(['packetize_avg_us', 'enqueue_avg_us', 'socket_send_avg_us'], map(int, stage.groups()))))
    result.update(exit=resources['exit'], valid=True, stale=int(stale[1]),
                  cpu_user_s=resources['cpu_user_s'], cpu_system_s=resources['cpu_system_s'],
                  max_rss_kib=resources['max_rss_kib'])
    samples = list(csv.DictReader(prefix.with_suffix('.samples.csv').open()))
    gpu = [int(row['gpu_busy_percent']) for row in samples if row['gpu_busy_percent']]
    result['sampled_gpu_busy_avg_percent'] = round(sum(gpu) / len(gpu), 2) if gpu else ''
    return result


def compare(directory):
    output = []
    for path in sorted(directory.glob('before-gpu-*.resources.json')):
        label = path.name.removeprefix('before-gpu-').removesuffix('.resources.json')
        try:
            before = read(directory / f'before-gpu-{label}')
            after = read(directory / f'after-gpu-{label}')
        except FileNotFoundError:
            continue
        row = {'case': label}
        for name, data in [('before', before), ('after', after)]:
            row.update({f'{name}_{key}': value for key, value in data.items()})
        if before['valid'] and after['valid']:
            row['submitted_throughput_change_pct'] = round(100 * (after['submitted_udp_payload_mbps'] / before['submitted_udp_payload_mbps'] - 1), 2)
            row['age_p99_change_pct'] = round(100 * (after['age_p99_us'] / before['age_p99_us'] - 1), 2)
            row['investigate'] = row['submitted_throughput_change_pct'] < -5 or row['age_p99_change_pct'] > 10
        output.append(row)
    columns = list(dict.fromkeys(key for row in output for key in row))
    if output:
        with (directory / 'comparison.csv').open('w') as file:
            writer = csv.DictWriter(file, fieldnames=columns)
            writer.writeheader()
            writer.writerows(output)
    for row in output:
        if row.get('investigate'):
            print(row['case'], 'throughput %', row['submitted_throughput_change_pct'], 'p99 %', row['age_p99_change_pct'])
    print(f'{len(output)} paired cases written to {directory}/comparison.csv')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('directory', type=Path)
    compare(parser.parse_args().directory)
