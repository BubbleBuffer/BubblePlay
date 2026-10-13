"""Recount CSV measurements and write reviewable tables; standard library only."""
import argparse
import csv
import math
import re
import statistics
import sys
from pathlib import Path


def quantile(values, p):
    return sorted(values)[max(0, math.ceil(len(values) * p) - 1)] if values else math.nan


def distribution(values):
    return [quantile(values, p) for p in (0.5, 0.95, 0.99, 1.0)]


def stats_from_row(row):
    names = ('stats_present_count', 'present_refresh_count', 'sync_refresh_count', 'sync_qpc')
    if all(row.get(n) for n in names):
        s = [int(row[n]) for n in names]
        if s[0] and s[3]:
            return s


def main():
    sys.stdout.reconfigure(encoding='utf-8')
    ap = argparse.ArgumentParser()
    ap.add_argument('--directory', type=Path, default=Path('F:/build/spike-results'))
    ap.add_argument('--prefix', required=True)
    ap.add_argument('--summary',default='summary.csv')
    ap.add_argument('--compare',action='store_true')
    args = ap.parse_args()
    directory = args.directory
    summaries = [r for r in csv.DictReader((directory / args.summary).open())
                 if r['run'].startswith(args.prefix)]
    assert summaries and all(r['status']=='ok' for r in summaries), 'missing or failed runs; no filtering'
    lines = ['All times are milliseconds. CPU-present endpoint is Present return; every frame is included.', '',
             '| Run | Frames | Submit→decode med/p95/p99/max | Decode→Present med/p95/p99/max | Submit→Present med/p95/p99/max | Interval SD/p99 | App drops | Submit late >1ms / frame late >16.67ms |',
             '|---|---:|---|---|---|---|---:|---:|']
    flips = []
    arrivals = []
    recount = []
    pooled={}
    hevc_stalls=[]
    timer_p99s=[]
    for summary in summaries:
        tag = summary['run']
        rows = list(csv.DictReader((directory / f'{tag}.csv').open()))
        assert len(rows) == int(summary['presented']) == int(summary['expected'])
        assert sorted(int(r['frame']) for r in rows) == list(range(len(rows)))
        if args.compare:
            key=(summary['codec'],summary['present'],summary['backend'])
            pooled.setdefault(key,[]).extend(rows)
            assert 'harness_version=fence-v7-ipc' in (directory/f'{tag}.txt').read_text()
            assert f'gpu_fence_waits={len(rows)}' in (directory/f'{tag}.txt').read_text()
            assert 'gpu_query_polls=0' in (directory/f'{tag}.txt').read_text()
        assert all(int(r['t_submit_qpc']) <= int(r['t_output_qpc']) <= int(r['t_decoded_qpc']) <= int(r['t_presented_qpc']) for r in rows)
        if 't_arrived_qpc' in rows[0]:
            assert all(int(r['scheduled_qpc']) <= int(r['t_arrived_qpc']) <= int(r['t_submit_qpc']) for r in rows)
        info = (directory / f'{tag}.txt').read_text()
        frequency = int(re.search(r'QPC_frequency=(\d+)', info)[1])
        distributions = []
        for key, prefix in [('submit_decoded_ms', 'sd'), ('decoded_presented_ms', 'dp'), ('submit_presented_ms', 'sp')]:
            measured = distribution([float(r[key]) for r in rows])
            expected = [float(summary[f'{prefix}_{n}']) for n in ('median', 'p95', 'p99', 'max')]
            assert all(abs(a - b) < 0.0011 for a, b in zip(measured, expected)), (tag, key, measured, expected)
            distributions.append('/'.join(f'{v:.3f}' for v in measured))
        intervals = [(int(b['t_presented_qpc']) - int(a['t_presented_qpc'])) * 1000 / frequency for a, b in zip(rows, rows[1:])]
        assert abs(statistics.pstdev(intervals) - float(summary['interval_stddev'])) < .0011
        lines.append(f"| {tag} | {len(rows)} | {' | '.join(distributions)} | {summary['interval_stddev']}/{summary['interval_p99']} | {summary['app_dropped']} | {summary['submit_late_over_1ms']}/{summary['frame_late_over_period']} |")
        if 't_arrived_qpc' in rows[0]:
            arrival_late=[float(r['arrival_late_ms']) for r in rows]
            timer_p99s.append((tag,quantile(arrival_late,.99)))
            queued=[float(r['arrival_submit_ms']) for r in rows]
            arrival_present=[float(r['arrival_presented_ms']) for r in rows]
            ordered=sorted(rows,key=lambda r:int(r['frame']))
            arrival_intervals=[(int(b['t_arrived_qpc'])-int(a['t_arrived_qpc']))*1000/frequency for a,b in zip(ordered,ordered[1:])]
            arrivals.append(f"| {tag} | {quantile(arrival_late,.5):.3f}/{quantile(arrival_late,.99):.3f}/{max(arrival_late):.3f} | {sum(v>1 for v in arrival_late)} | {quantile(arrival_intervals,.5):.3f}/{statistics.pstdev(arrival_intervals):.3f}/{quantile(arrival_intervals,.99):.3f} | {quantile(queued,.5):.3f}/{quantile(queued,.99):.3f} | {quantile(arrival_present,.5):.3f}/{quantile(arrival_present,.99):.3f} |")
        if args.compare and summary['codec']=='hevc':
            with (directory / f'{tag}-decode-stalls.csv').open('w', newline='') as out:
                writer = csv.writer(out)
                writer.writerow(['frame', 'submit_decoded_ms', 'submit_output_ms', 'output_fence_observed_ms', 'decoded_presented_ms', 'submit_presented_ms', 'arrival_late_ms', 'arrival_submit_ms'])
                for row in sorted(rows, key=lambda r: int(r['frame'])):
                    if float(row['submit_decoded_ms']) > 1000 / 60:
                        before = (int(row['t_output_qpc']) - int(row['t_submit_qpc'])) * 1000 / frequency
                        fence = (int(row['t_decoded_qpc']) - int(row['t_output_qpc'])) * 1000 / frequency
                        writer.writerow([row['frame'], row['submit_decoded_ms'], f'{before:.6f}', f'{fence:.6f}', row['decoded_presented_ms'], row['submit_presented_ms'], row['arrival_late_ms'], row['arrival_submit_ms']])
            worst=max(rows,key=lambda r:float(r['submit_decoded_ms']))
            local=[r for r in rows if 1040<=int(r['frame'])<=1060]
            local_worst=max(local,key=lambda r:float(r['submit_decoded_ms']))
            at1050=next(r for r in rows if int(r['frame'])==1050)
            before=(int(worst['t_output_qpc'])-int(worst['t_submit_qpc']))*1000/frequency
            completion=(int(worst['t_decoded_qpc'])-int(worst['t_output_qpc']))*1000/frequency
            hevc_stalls.append(f"| {tag} | {worst['frame']} | {float(worst['submit_decoded_ms']):.3f} | {before:.3f}/{completion:.3f} | {float(at1050['submit_decoded_ms']):.3f} | {local_worst['frame']}: {float(local_worst['submit_decoded_ms']):.3f} |")
        available = [s for r in rows if (s := stats_from_row(r))]
        final_path = directory / f'{tag}-final-stats.txt'
        if final_path.exists():
            text = final_path.read_text()
            names = ('PresentCount', 'PresentRefreshCount', 'SyncRefreshCount', 'SyncQPCTime')
            matches = [re.search(rf'{name}: (\d+)', text) for name in names]
            if all(matches):
                available.append([int(m[1]) for m in matches])
        periods = [(b[3] - a[3]) / (b[2] - a[2]) for a, b in zip(available, available[1:]) if b[2] > a[2] and b[3] > a[3]]
        period = statistics.median(periods) if periods else None
        by_id = {int(r['present_id']): r for r in rows}
        estimates = {}
        invalid_estimates = 0
        if period and 15 <= frequency / period <= 400:
            for s in available:
                if s[0] not in by_id:
                    continue
                # PresentRefreshCount says which vblank displayed this present ID.
                # SyncQPCTime is a sampled reference at SyncRefreshCount.
                delta = ((s[1] - s[2] + 2**31) % 2**32) - 2**31
                flip_qpc = s[3] + delta * period
                r = by_id[s[0]]
                latency = (flip_qpc - int(r['t_submit_qpc'])) * 1000 / frequency
                estimates[s[0]] = (int(r['frame']), flip_qpc, latency)
        with (directory / f'{tag}-flip-estimates.csv').open('w', newline='') as out:
            writer = csv.writer(out)
            writer.writerow(['present_id', 'frame', 'estimated_vblank_qpc', 'submit_to_estimated_vblank_ms'])
            writer.writerows((pid, *v) for pid, v in sorted(estimates.items()))
        latency = [v[2] for v in estimates.values()]
        invalid_estimates = sum(v < 0 or v > 1000 for v in latency)
        flip_dist = ('unreliable: ' + str(invalid_estimates) + ' invalid references') if invalid_estimates else ('/'.join(f'{v:.3f}' for v in distribution(latency)) if latency else 'unavailable')
        flips.append(f"| {tag} | {len(available)} | {len(estimates)}/{len(rows)} | {frequency / period:.3f}" if period else f"| {tag} | {len(available)} | {len(estimates)}/{len(rows)} | unavailable")
        flips[-1] += f' | {flip_dist} |'
        recount.append({'run': tag, 'frames': len(rows), 'unique_present_ids': len(by_id), 'valid_stats_observations': len(available), 'mapped_flip_estimates': len(estimates), 'invalid_flip_references': invalid_estimates, 'stats_errors': dict((e, sum(r['stats_hresult'] == e for r in rows)) for e in set(r['stats_hresult'] for r in rows) if e)})
    lines += ['', 'DXGI vblank estimates (separate from CPU Present timing):', '',
              '| Run | Valid statistics samples incl. final | Mapped present IDs | Stats-derived counter rate Hz | Submit→vblank estimate med/p95/p99/max |',
              '|---|---:|---:|---:|---|', *flips, '',
              'Vblank estimates use median ΔSyncQPCTime/ΔSyncRefreshCount, then SyncQPCTime + (PresentRefreshCount−SyncRefreshCount) × period, matched by PresentCount to GetLastPresentCount. Negative latency means the reference predates the matching submission; any such run is marked unreliable and has no aggregate flip latency. Raw estimates including invalid values remain in separate CSVs. These are refresh-level estimates, especially with tearing, not optical scanout timestamps. Missing mappings do not establish display drops.', '']
    if arrivals:
        lines += ['Independent simulated 60 Hz arrival producer and decode backlog:', '',
                  '| Run | Arrival lateness med/p99/max | Arrivals late >1ms | Arrival interval median/SD/p99 | Arrival→submit med/p99 | Arrival→Present med/p99 |',
                  '|---|---|---:|---|---|---|', *arrivals, '']
    if args.compare:
        assert len(summaries)==24 and len(pooled)==8 and all(len(v)==3600 for v in pooled.values())
        order=(directory/f'{args.prefix}-order.txt').read_text().splitlines()
        assert len(order)==24 and all(('-mf-' in order[i]) if i%2==0 else ('-ffmpeg-' in order[i]) for i in range(24))
        side=['Pooled comparison, all 3600 frames (three fresh-process runs) per backend/codec/present mode; no startup exclusion. Values are median / p99 milliseconds. No backend selection is implied.','',
              '| Codec | Present | MF submit→decoded | FFmpeg submit→decoded | MF submit→presented | FFmpeg submit→presented |','|---|---|---|---|---|---|']
        for codec in ('h264','hevc'):
            for mode in ('immediate','vsync'):
                cells=[]
                for metric in ('submit_decoded_ms','submit_presented_ms'):
                    for backend in ('mf','ffmpeg'):
                        values=[float(r[metric]) for r in pooled[(codec,mode,backend)]]
                        cells.append(f'{quantile(values,.5):.3f}/{quantile(values,.99):.3f}')
                side.append(f"| {codec} | {mode} | {' | '.join(cells)} |")
        lines+=['',*side,'','HEVC stall locations (input AU/frame indices; before-output/completion columns split the worst submit→decoded interval):','',
                '| Run | Worst frame | Submit→decoded ms | Before output / fence observation ms | Frame 1050 ms | Worst in frames 1040–1060 |','|---|---:|---:|---|---:|---|',*hevc_stalls,'',
                f'Timer p99 lateness range: {min(v for _,v in timer_p99s):.6f}–{max(v for _,v in timer_p99s):.6f} ms. Under 1 ms for every run: {all(v<1 for _,v in timer_p99s)}.','']
        (directory/f'{args.prefix}-comparison.md').write_text('\n'.join(side),encoding='utf-8')
        (directory/f'{args.prefix}-hevc-stalls.md').write_text('\n'.join(hevc_stalls),encoding='utf-8')
    (directory / f'{args.prefix}-summary.md').write_text('\n'.join(lines), encoding='utf-8')
    import json
    (directory / f'{args.prefix}-recount.json').write_text(json.dumps(recount, indent=2))
    print('\n'.join(lines))


if __name__ == '__main__':
    main()
