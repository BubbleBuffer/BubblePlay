"""Phase analysis for decode-latency gate CSVs; standard library only.

    python client-win/spikes/decode-latency/analyze-phases.py --prefix fencev2-

Analyzes the summary entries whose run name starts with --prefix and whose status is ok.
Every run is re-derived from its raw QPC columns rather than trusted from the summary.
A run is excluded, and named in the output, when its status is not ok, a file is missing,
the CSV does not end in a newline (possibly still being written), a row is short or
unparseable, a frame invariant fails, or a recomputed value disagrees with the CSV or
summary columns. Without --allow-excluded nothing is written when any run is excluded.
Each file is read once, so one run is analyzed from one snapshot.

Phases, named as the harness records its endpoints. The names describe shared-context
measurement points; they do not identify isolated hardware execution:
  source_arrival_lateness        arrived - scheduled
  fifo_delay                     submit - arrived
  before_output                  output - submit
  fence_completion_observation   decoded - output
  decoded_presented              presented - decoded
Results describe these intervals only. They do not attribute time to CPU, GPU or driver.

Outputs: <directory>/<prefix>phases.md and <prefix>phases.json.
"""
import argparse
import csv
import hashlib
import io
import json
import math
import re
import sys
from pathlib import Path

ORDER_COLUMNS = ('scheduled_qpc', 't_arrived_qpc', 't_submit_qpc', 't_output_qpc', 't_decoded_qpc', 't_presented_qpc')
COLUMN_PHASES = {  # CSV column -> phase it must equal when recomputed from the timestamps
    'arrival_late_ms': 'source_arrival_lateness',
    'arrival_submit_ms': 'fifo_delay',
    'submit_decoded_ms': 'submit_decoded',
    'decoded_presented_ms': 'decoded_presented',
    'submit_presented_ms': 'submit_presented',
}
REQUIRED_COLUMNS = ('frame', *ORDER_COLUMNS, *COLUMN_PHASES)
SUMMARY_PHASES = {'sd': 'submit_decoded', 'dp': 'decoded_presented', 'sp': 'submit_presented'}  # summary-v2 prefixes
SUMMARY_REQUIRED = ('run', 'status', 'backend', 'codec', 'present', 'expected', 'presented',
                    *(f'{s}_{q}' for s in SUMMARY_PHASES for q in ('median', 'p95', 'p99', 'max')))
PHASES = ('source_arrival_lateness', 'fifo_delay', 'before_output', 'fence_completion_observation',
          'decoded_presented', 'submit_decoded', 'submit_presented')
COLUMN_TOLERANCE_MS = 1e-5     # CSV columns print 6 decimals of ms
SUMMARY_TOLERANCE_MS = 0.0011  # summary columns print 3 decimals of ms


class Excluded(Exception):
    pass


def quantile(values, p):
    """Nearest-rank quantile, the same definition analyze-results.py uses."""
    ordered = sorted(values)
    return ordered[max(0, math.ceil(len(ordered) * p) - 1)]


def distribution(values):
    return {'median': quantile(values, .5), 'p95': quantile(values, .95),
            'p99': quantile(values, .99), 'max': max(values)}


def cell(d):
    return '/'.join(f'{d[k]:.3f}' for k in ('median', 'p95', 'p99', 'max'))


def worst(phases, indices):
    """Frame with the largest submit->decoded time among indices, with every phase."""
    indices = [i for i in indices if 0 <= i < len(phases)]
    if not indices:
        return None
    i = max(indices, key=lambda j: phases[j]['submit_decoded'])
    return {'frame': i, **phases[i]}


def analyze_run(directory, row, args):
    run = row['run']
    if row.get('status') != 'ok':
        raise Excluded(f"summary status is {row.get('status')!r}, not ok")
    if None in row or any(v is None for v in row.values()):
        raise Excluded('summary row is incomplete')
    csv_path, txt_path = directory / f'{run}.csv', directory / f'{run}.txt'
    for path in (csv_path, txt_path):
        if not path.is_file():
            raise Excluded(f'missing {path.name}')
    csv_bytes, txt_bytes = csv_path.read_bytes(), txt_path.read_bytes()
    if not csv_bytes.endswith(b'\n'):
        raise Excluded('CSV does not end in a newline; it may still be being written')

    text = txt_bytes.decode('utf-8', errors='replace')
    sidecar_lines = {line.rstrip('\r') for line in text.splitlines()}
    freq_match = re.search(r'^QPC_frequency=(\d+)\r?$', text, re.M)
    if not freq_match:
        raise Excluded('sidecar has no QPC_frequency line')
    if 'error=none' not in sidecar_lines:
        raise Excluded('sidecar error line is not error=none')
    missing_lines = [line for line in args.require_line if line not in sidecar_lines]
    if missing_lines:
        raise Excluded(f'sidecar lacks {missing_lines}')
    frames_match = re.search(r'^frames_presented=(\d+)\r?$', text, re.M)

    try:
        reader = csv.DictReader(io.StringIO(csv_bytes.decode('utf-8')))
        absent = [c for c in REQUIRED_COLUMNS if c not in (reader.fieldnames or [])]
        if absent:
            raise Excluded(f'CSV lacks columns {absent}')
        rows = list(reader)
    except UnicodeDecodeError as e:
        raise Excluded(f'CSV is not valid UTF-8 ({e})')
    if any(None in r or any(v is None for v in r.values()) for r in rows):
        raise Excluded('CSV has a short or over-long row (partial write?)')
    n = len(rows)
    freq = int(freq_match[1])
    try:
        expected, presented = int(row['expected']), int(row['presented'])
        frames = [int(r['frame']) for r in rows]
        ticks = [{c: int(r[c]) for c in ORDER_COLUMNS} for r in rows]
        columns = {c: [float(r[c]) for r in rows] for c in COLUMN_PHASES}
        summary = {f'{s}_{q}': float(row[f'{s}_{q}']) for s in SUMMARY_PHASES
                   for q in ('median', 'p95', 'p99', 'max')}
    except ValueError as e:
        raise Excluded(f'unparseable value ({e})')
    if not n == expected == presented:
        raise Excluded(f'{n} rows, summary expected {expected}, presented {presented}')
    if frames_match and int(frames_match[1]) != n:
        raise Excluded(f'sidecar frames_presented {frames_match[1]} != {n} rows')
    if frames != list(range(n)):
        raise Excluded('frame column is not exactly 0..N-1 in file order')

    bad = [i for i, t in enumerate(ticks) if not (
        t['scheduled_qpc'] <= t['t_arrived_qpc'] <= t['t_submit_qpc'] <= t['t_output_qpc']
        <= t['t_decoded_qpc'] <= t['t_presented_qpc'])]
    if bad:
        raise Excluded(f'scheduled<=arrived<=submit<=output<=decoded<=presented fails in {len(bad)} frames, first {bad[0]}')
    for c in ORDER_COLUMNS:
        bad = [i for i in range(1, n) if ticks[i][c] < ticks[i - 1][c]]
        if bad:
            raise Excluded(f'{c} decreases at frame {bad[0]} (FIFO order broken)')

    def ms(a, b):
        return (b - a) * 1000 / freq

    phases = [{
        'source_arrival_lateness': ms(t['scheduled_qpc'], t['t_arrived_qpc']),
        'fifo_delay': ms(t['t_arrived_qpc'], t['t_submit_qpc']),
        'before_output': ms(t['t_submit_qpc'], t['t_output_qpc']),
        'fence_completion_observation': ms(t['t_output_qpc'], t['t_decoded_qpc']),
        'decoded_presented': ms(t['t_decoded_qpc'], t['t_presented_qpc']),
        'submit_decoded': ms(t['t_submit_qpc'], t['t_decoded_qpc']),
        'submit_presented': ms(t['t_submit_qpc'], t['t_presented_qpc']),
    } for t in ticks]

    column_diff = 0.0
    for c, phase in COLUMN_PHASES.items():
        diff = max(abs(v - p[phase]) for v, p in zip(columns[c], phases))
        if diff > COLUMN_TOLERANCE_MS:
            raise Excluded(f'{c} differs from its timestamps by {diff:.6f} ms')
        column_diff = max(column_diff, diff)

    summary_diff = 0.0
    for short, phase in SUMMARY_PHASES.items():
        measured = distribution([p[phase] for p in phases])
        for q in ('median', 'p95', 'p99', 'max'):
            key = f'{short}_{q}'
            diff = abs(measured[q] - summary[key])
            if diff > SUMMARY_TOLERANCE_MS:
                raise Excluded(f'summary {key} is {summary[key]} but recomputed {measured[q]:.6f}')
            summary_diff = max(summary_diff, diff)

    startup_end = min(args.startup_frames, n)
    lo, hi = args.focus_window
    run_out = {
        'run': run, 'backend': row['backend'], 'codec': row['codec'], 'present': row['present'],
        'frames': n, 'qpc_frequency_hz': freq,
        'input_sha256': {'csv': hashlib.sha256(csv_bytes).hexdigest(), 'txt': hashlib.sha256(txt_bytes).hexdigest()},
        'checks': {
            'rows_equal_summary_expected_presented': n,
            'frame_index_exact': True,
            'ordering_violations': 0,
            'timestamps_non_decreasing': True,
            'csv_newline_terminated': True,
            'sidecar_error_none': True,
            'sidecar_required_lines': list(args.require_line),
            'column_max_abs_diff_ms': column_diff,
            'summary_max_abs_diff_ms': summary_diff,
        },
        'distribution_ms': {phase: distribution([p[phase] for p in phases]) for phase in PHASES},
        'worst_submit_decoded': {
            'overall': worst(phases, range(n)),
            'startup': worst(phases, range(0, startup_end)),
            'later': worst(phases, range(startup_end, n)),
        },
    }
    if run_out['worst_submit_decoded']['overall']:
        overall = run_out['worst_submit_decoded']['overall']
        overall['window'] = 'startup' if overall['frame'] < args.startup_frames else 'later'
    if row['codec'] == args.worst_codec:
        run_out['focus'] = {
            'frame': args.focus_frame,
            'row': worst(phases, [args.focus_frame]),
            'window': f'{lo}-{hi}',
            'worst_in_window': worst(phases, range(lo, hi + 1)),
        }
    return run_out, phases


def fmt_phase_row(run, label, w):
    if w is None:
        return f'| {run} | {label} | not in run | | | | | | |'
    return (f"| {run} | {label} | {w['frame']} | {w['source_arrival_lateness']:.3f} | {w['fifo_delay']:.3f} | "
            f"{w['before_output']:.3f} | {w['fence_completion_observation']:.3f} | "
            f"{w['decoded_presented']:.3f} | {w['submit_decoded']:.3f} |")


def markdown(prefix, args, runs, excluded, unlisted, pooled):
    lo, hi = args.focus_window
    s = args.startup_frames
    out = [f'# Phase analysis: {prefix}', '',
           f'Summary `{args.summary}`: {len(runs)} analyzed, {len(excluded)} excluded, '
           f'{len(unlisted)} unlisted (not analyzed). Units: ms. Quantiles are nearest-rank over every frame '
           f'of each run; no warm-up is excluded. Startup = frames 0-{s - 1}; later = frames {s}+.', '',
           'Endpoints: submit, output, decoded (shared-context fence completion: shared immediate-context '
           'ID3D11Fence signal, completion event, kernel wait, as recorded in each sidecar), and presented '
           '(CPU Present return). These intervals are reported as recorded. They are not isolated hardware '
           'execution, and no CPU, GPU or driver cause is assigned.', '']
    if excluded:
        out += ['## EXCLUDED (not pooled)', '', *(f"- `{e['run']}`: {e['reason']}" for e in excluded), '']
    out += ['## Checks run per analyzed run', '',
            '| Run | Frames | Ordering violations | Frame index exact | Timestamps monotone | Max column diff ms | Max summary diff ms | Sidecar |',
            '|---|---:|---:|---|---|---:|---:|---|']
    for r in runs:
        c = r['checks']
        out.append(f"| {r['run']} | {r['frames']} | {c['ordering_violations']} | {c['frame_index_exact']} | "
                   f"{c['timestamps_non_decreasing']} | {c['column_max_abs_diff_ms']:.6f} | "
                   f"{c['summary_max_abs_diff_ms']:.6f} | error=none; {len(c['sidecar_required_lines'])} required lines |")
    out += ['', 'Ordering: scheduled<=arrived<=submit<=output<=decoded<=presented for every frame; frame column = input index 0..N-1.', '',
            '## Per-run distributions (median/p95/p99/max)', '',
            '| Run | Source arrival lateness | FIFO delay (arrival->submit) | Before output (submit->output) | Fence completion obs. (output->decoded) | Submit->decoded (reference) |',
            '|---|---|---|---|---|---|']
    for r in runs:
        d = r['distribution_ms']
        out.append(f"| {r['run']} | {cell(d['source_arrival_lateness'])} | {cell(d['fifo_delay'])} | "
                   f"{cell(d['before_output'])} | {cell(d['fence_completion_observation'])} | {cell(d['submit_decoded'])} |")
    out += ['', f'## Startup vs later worst submit->decoded', '',
            f'| Run | Startup worst (frames 0-{s - 1}) | Later worst (frames {s}+) | Overall worst |',
            '|---|---|---|---|']
    for r in runs:
        w = r['worst_submit_decoded']
        def desc(x):
            return 'n/a' if x is None else f"frame {x['frame']}: {x['submit_decoded']:.3f} ms"
        ov = w['overall']
        out.append(f"| {r['run']} | {desc(w['startup'])} | {desc(w['later'])} | "
                   f"{desc(ov)} ({ov['window']}) |")
    out += ['', f'## Worst {args.worst_codec.upper()} frame per run (phase durations, ms)', '',
            '| Run | Row | Frame | Source arrival lateness | FIFO delay | Before output | Fence completion obs. | Decoded->presented | Submit->decoded |',
            '|---|---|---:|---:|---:|---:|---:|---:|---:|']
    any_focus = False
    for r in runs:
        if 'focus' not in r:
            continue
        any_focus = True
        f = r['focus']
        ov = r['worst_submit_decoded']['overall']
        out.append(fmt_phase_row(r['run'], f"worst overall ({ov['window']})", ov))
        out.append(fmt_phase_row(r['run'], f"frame {f['frame']}", f['row']))
        out.append(fmt_phase_row(r['run'], f"worst in {f['window']}", f['worst_in_window']))
    if not any_focus:
        out.append(f'| - | no {args.worst_codec.upper()} runs analyzed | | | | | | | |')
    out += ['', f'Frame {args.focus_frame} is exact; the window is inclusive {lo}-{hi}. "Not in run" means the run has fewer frames.', '',
            '## Pooled by backend/codec/present (all frames, startup included)', '',
            '| Group | Frames | Source arrival lateness | FIFO delay | Before output | Fence completion obs. | Submit->decoded |',
            '|---|---:|---|---|---|---|---|']
    for g in pooled:
        d = g['distribution_ms']
        out.append(f"| {g['group']} | {g['frames']} | {cell(d['source_arrival_lateness'])} | {cell(d['fifo_delay'])} | "
                   f"{cell(d['before_output'])} | {cell(d['fence_completion_observation'])} | {cell(d['submit_decoded'])} |")
    if unlisted:
        out += ['', '## Unlisted files (prefix match, no summary entry; not analyzed, not pooled)', '',
                *(f'- `{name}`' for name in unlisted)]
    out += ['', 'Live-read caveat: an entry counts only with status ok, a CSV ending in a newline, full row count, '
            'and a sidecar with error=none. A run still being written is shown as excluded or unlisted, not analyzed.', '']
    return '\n'.join(out)


def main():
    sys.stdout.reconfigure(encoding='utf-8')
    ap = argparse.ArgumentParser(description='Phase analysis for decode-latency gate CSVs (standard library only).')
    ap.add_argument('--directory', type=Path, default=Path('F:/build/spike-results'))
    ap.add_argument('--prefix', required=True, help='run-name prefix, e.g. fencev2-')
    ap.add_argument('--summary', default='summary-v2.csv')
    ap.add_argument('--startup-frames', type=int, default=120)
    ap.add_argument('--worst-codec', default='hevc')
    ap.add_argument('--focus-frame', type=int, default=1050)
    ap.add_argument('--focus-window', default='1040:1060', help='inclusive lo:hi frame window')
    ap.add_argument('--require-line', action='append', default=[],
                    help='exact sidecar line every run must contain, e.g. gpu_query_polls=0 (repeatable)')
    ap.add_argument('--allow-excluded', action='store_true',
                    help='write results for the completed runs and name the excluded ones')
    args = ap.parse_args()
    if not re.fullmatch(r'[A-Za-z0-9._-]+', args.prefix):
        sys.exit('--prefix must be letters, digits, dot, underscore or hyphen')
    lo, hi = (int(x) for x in args.focus_window.split(':'))
    args.focus_window = (lo, hi)
    directory = args.directory

    summary_reader = csv.DictReader(io.StringIO((directory / args.summary).read_bytes().decode('utf-8')))
    absent = [c for c in SUMMARY_REQUIRED if c not in (summary_reader.fieldnames or [])]
    if absent:
        sys.exit(f'{args.summary} lacks columns {absent}')
    selected = [r for r in summary_reader if (r.get('run') or '').startswith(args.prefix)]
    if not selected:
        sys.exit(f'no {args.summary} rows start with {args.prefix!r}')

    runs, excluded, pooled_frames = [], [], {}
    for row in selected:
        try:
            run_out, phases = analyze_run(directory, row, args)
        except Excluded as e:
            excluded.append({'run': row.get('run'), 'reason': str(e)})
            continue
        runs.append(run_out)
        pooled_frames.setdefault((run_out['backend'], run_out['codec'], run_out['present']), []).extend(phases)

    listed = {f"{r.get('run')}.csv" for r in selected}
    unlisted = sorted(p.name for p in directory.glob(f'{args.prefix}*.csv')
                      if p.name not in listed and not p.name.endswith(('-flip-estimates.csv', '-decode-stalls.csv', '-memory.csv')))

    if excluded:
        for e in excluded:
            print(f"EXCLUDED {e['run']}: {e['reason']}", file=sys.stderr)
    if not runs:
        print('no completed run to analyze; nothing written', file=sys.stderr)
        return 2
    if excluded and not args.allow_excluded:
        print('excluded runs present; nothing written (rerun with --allow-excluded to write completed runs only)',
              file=sys.stderr)
        return 2
    for name in unlisted:
        print(f'UNLISTED {name}: no summary entry; not analyzed', file=sys.stderr)

    pooled = [{'group': '/'.join(key), 'frames': len(ph),
               'distribution_ms': {phase: distribution([p[phase] for p in ph]) for phase in PHASES}}
              for key, ph in sorted(pooled_frames.items())]
    stem = f'{args.prefix}phases'
    md = markdown(args.prefix, args, runs, excluded, unlisted, pooled)
    result = {'prefix': args.prefix, 'summary': args.summary,
              'parameters': {'startup_frames': args.startup_frames, 'worst_codec': args.worst_codec,
                             'focus_frame': args.focus_frame, 'focus_window': list(args.focus_window),
                             'require_line': args.require_line, 'quantile': 'nearest-rank',
                             'column_tolerance_ms': COLUMN_TOLERANCE_MS, 'summary_tolerance_ms': SUMMARY_TOLERANCE_MS},
              'runs': runs, 'pooled': pooled, 'excluded': excluded, 'unlisted': unlisted}
    (directory / f'{stem}.md').write_text(md, encoding='utf-8')
    (directory / f'{stem}.json').write_text(json.dumps(result, indent=2), encoding='utf-8')
    print(md)
    return 0


if __name__ == '__main__':
    sys.exit(main())
