#!/usr/bin/env python3
"""Reproduce CPU-only packetizer A/B evidence; never change encoding quality."""
import argparse
import collections
import csv
import io
import json
import os
from pathlib import Path
import subprocess
import tarfile

ROOT = Path(__file__).resolve().parents[1]
PACKETIZER = Path('moonshine-core/src/session/stream/video/packetizer.rs')
BENCH = Path('moonshine-tools/src/bin/bench.rs')


def prepare(args):
    dest = args.directory.resolve()
    if dest.exists():
        raise SystemExit('Baseline directory must be new')
    archive = subprocess.check_output(['git', 'archive', args.revision], cwd=ROOT)
    dest.mkdir(parents=True)
    with tarfile.open(fileobj=io.BytesIO(archive)) as files:
        files.extractall(dest, filter='data')
    current = (ROOT / PACKETIZER).read_text()
    start = current.index('\n\t/// CPU-only,')
    end = current.index('\n\t/// `(iv, plaintext)`', start)
    allocator = current[current.index('#[cfg(test)]\nmod measurement_allocator {'):]
    allocator = allocator.replace('\tpub fn reset()', '\tpub fn assembly_copy(bytes: usize) { record(3, bytes); }\n\tpub fn reset()')
    baseline = (dest / PACKETIZER).read_text()
    insertion = baseline.index('\n\t/// `(iv, plaintext)`')
    baseline = baseline[:insertion] + current[start:end] + baseline[insertion:]
    copy = 'all_shards.extend_from(&shard_buf.into_batch());'
    assert baseline.count(copy) == 1
    baseline = baseline.replace(copy, '#[cfg(test)]\n\t\t\tmeasurement_allocator::assembly_copy(total_shards * (requested_shard_size + prefix_size));\n\t\t\t' + copy)
    (dest / PACKETIZER).write_text(baseline + '\n' + allocator)
    # Match socket-completion timestamps and the live receiver in both builds.
    from transport_baseline_instrumentation import instrument
    instrument(dest, ROOT)


def collect(args):
    args.output.mkdir(parents=True, exist_ok=True)
    for trial in range(1, args.trials + 1):
        for name, executable in [('before', args.before), ('after', args.after)]:
            with (args.output / f'{name}-repeat-{trial}.csv').open('w') as output:
                process = subprocess.Popen([str(executable.resolve()), 'transport_packetizer_measurements', '--ignored', '--nocapture'], stdout=output)
                _, status, usage = os.wait4(process.pid, 0)
                process.returncode = os.waitstatus_to_exitcode(status)
                (args.output / f'{name}-repeat-{trial}.resources').write_text(json.dumps({
                    'exit': process.returncode, 'cpu_user_s': usage.ru_utime,
                    'cpu_system_s': usage.ru_stime, 'max_rss_kib': usage.ru_maxrss,
                }, indent=2) + '\n')
                if process.returncode:
                    raise SystemExit(process.returncode)


def rows(path):
    with path.open() as file:
        return [r for r in csv.reader(file) if r and r[0] == 'packetizer']


def compare(args):
    with (args.output / 'comparison.csv').open('w') as file:
        writer = csv.writer(file)
        writer.writerow(['trial', 'encoded_bytes', 'fec', 'encrypted', 'before_p50_ns', 'before_p95_ns', 'before_p99_ns', 'after_p50_ns', 'after_p95_ns', 'after_p99_ns', 'p99_change_pct', 'before_allocations', 'after_allocations', 'before_allocated_bytes', 'after_allocated_bytes', 'before_assembly_copy_bytes', 'after_assembly_copy_bytes'])
        for trial in range(1, args.trials + 1):
            before, after = [rows(args.output / f'{name}-repeat-{trial}.csv') for name in ['before', 'after']]
            assert len(before) == len(after) == 1600
            # Exact whole-batch fingerprints, including nonces and all padding.
            assert all(b[:5] + b[6:9] == a[:5] + a[6:9] for b, a in zip(before, after))
            groups = collections.defaultdict(lambda: [[], []])
            for i, samples in enumerate([before, after]):
                for row in samples:
                    groups[tuple(row[1:4])][i].append(row)
            for key, (b, a) in groups.items():
                def percentiles(samples):
                    times = sorted(int(r[5]) for r in samples)
                    return [times[i] for i in [50, 95, 99]]
                bp, ap = percentiles(b), percentiles(a)
                writer.writerow([trial, *key, *bp, *ap, round((ap[2] / bp[2] - 1) * 100, 2), b[0][9], a[0][9], b[0][10], a[0][10], b[0][12], a[0][12]])
    print('All paired packet fingerprints match; per-trial comparison.csv written.')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    baseline = sub.add_parser('prepare-baseline')
    baseline.add_argument('directory', type=Path)
    baseline.add_argument('--revision', required=True)
    baseline.set_defaults(run=prepare)
    collection = sub.add_parser('collect')
    collection.add_argument('--before', required=True, type=Path)
    collection.add_argument('--after', required=True, type=Path)
    collection.add_argument('--output', required=True, type=Path)
    collection.add_argument('--trials', type=int, default=3)
    collection.set_defaults(run=collect)
    comparison = sub.add_parser('compare')
    comparison.add_argument('--output', required=True, type=Path)
    comparison.add_argument('--trials', type=int, default=3)
    comparison.set_defaults(run=compare)
    args = parser.parse_args()
    args.run(args)
