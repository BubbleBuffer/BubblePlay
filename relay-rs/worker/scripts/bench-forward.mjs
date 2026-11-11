#!/usr/bin/env node
// CPU cost of one forwarded media frame, measured against local workerd (`wrangler dev --local`).
//
// Starts the relay from tests/m3-support.mjs, registers a throwaway host, joins a throwaway viewer,
// opens the media pair and sends `--frames` binary frames from the host socket. The viewer socket
// counts what arrives. The CPU time of the workerd processes (descendants of the wrangler process,
// found by process tree, never by name) is read before and after the burst. Stdout gets one JSON
// document: frames, bytes per frame, wall milliseconds, messages per second and CPU milliseconds per
// forwarded frame. A forwarded frame is one message in and one message out. Progress goes to stderr.
//
// Needs `worker-build --release` first and the pinned wrangler in node_modules (see the CI job).
// Nothing is written outside a temp directory, and every process it starts is stopped.
//   node scripts/bench-forward.mjs [--frames 20000] [--size 188] [--rounds 3]
// Defaults mimic one 10 ms Opus packet: a 28-byte BPA1 header plus about 160 bytes of payload.

import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { parseArgs } from 'node:util';

import { dataSocket, identity, registerHost, sleep, startRelay, viewerJoin } from '../tests/m3-support.mjs';

const { values } = parseArgs({
  options: {
    frames: { type: 'string', default: '20000' },
    size: { type: 'string', default: '188' },
    rounds: { type: 'string', default: '3' },
  },
});
const FRAMES = Number(values.frames);
const SIZE = Number(values.size);
const ROUNDS = Number(values.rounds);
const log = (message) => process.stderr.write(`bench-forward: ${message}\n`);

// Every descendant pid of `root`, from the OS process table.
function descendants(root) {
  const rows = [];
  if (process.platform === 'win32') {
    const out = execFileSync(
      'powershell',
      ['-NoProfile', '-Command', 'Get-CimInstance Win32_Process | ForEach-Object { "$($_.ProcessId) $($_.ParentProcessId) $($_.Name)" }'],
      { encoding: 'utf8' },
    );
    for (const line of out.split(/\r?\n/)) {
      const [pid, ppid, name] = line.trim().split(/\s+/);
      if (pid) rows.push({ pid: Number(pid), ppid: Number(ppid), name });
    }
  } else {
    const out = execFileSync('ps', ['-eo', 'pid=,ppid=,comm='], { encoding: 'utf8' });
    for (const line of out.split('\n')) {
      const [pid, ppid, name] = line.trim().split(/\s+/);
      if (pid) rows.push({ pid: Number(pid), ppid: Number(ppid), name });
    }
  }
  const found = [];
  const queue = [root];
  while (queue.length > 0) {
    const parent = queue.pop();
    for (const row of rows) {
      if (row.ppid === parent && !found.some((f) => f.pid === row.pid)) {
        found.push(row);
        queue.push(row.pid);
      }
    }
  }
  return found;
}

// Total CPU milliseconds (user + system) of one process.
function cpuMs(pid) {
  if (process.platform === 'win32') {
    const out = execFileSync(
      'powershell',
      ['-NoProfile', '-Command', `(Get-Process -Id ${pid}).TotalProcessorTime.TotalMilliseconds`],
      { encoding: 'utf8' },
    );
    return Number(out.trim().replace(',', '.'));
  }
  const stat = readFileSync(`/proc/${pid}/stat`, 'utf8');
  const fields = stat.slice(stat.lastIndexOf(')') + 2).split(' ');
  return ((Number(fields[11]) + Number(fields[12])) * 1000) / 100;
}

function workerdCpu(root) {
  let total = 0;
  for (const proc of descendants(root)) {
    if (!/^workerd/i.test(proc.name)) continue;
    try {
      total += cpuMs(proc.pid);
    } catch {
      // The process ended between the listing and the read.
    }
  }
  return total;
}

const persistDir = mkdtempSync(path.join(tmpdir(), 'bp-bench-forward-'));
let relay = null;
try {
  relay = await startRelay({ persistDir });
  const child = relay.pid;
  const host = identity(70);
  const { ok: hostOk } = await registerHost(relay.port, host);
  const { socket: viewer, ok: viewerOk } = await viewerJoin(relay.port, host.id);
  const epoch = viewerOk.mediaEpoch;
  const hostMedia = await dataSocket(relay.port, '/host-media-ws', host.id, epoch, 'media-host', hostOk.ticket);
  const viewerMedia = await dataSocket(relay.port, '/media-ws', host.id, epoch, 'media-viewer', viewerOk.ticket);
  const frame = Buffer.alloc(SIZE, 7);

  const results = [];
  for (let round = 0; round < ROUNDS + 1; round += 1) {
    // Round 0 warms the object up and is not reported.
    const before = workerdCpu(child);
    const started = process.hrtime.bigint();
    let received = 0;
    const done = (async () => {
      while (received < FRAMES) {
        await viewerMedia.socket.nextBinary(30_000);
        received += 1;
      }
    })();
    for (let i = 0; i < FRAMES; i += 1) {
      hostMedia.socket.sendBinary(frame);
      // Yield now and then so the socket buffers drain.
      if (i % 500 === 499) await sleep(1);
    }
    await done;
    const wallMs = Number(process.hrtime.bigint() - started) / 1e6;
    const cpu = workerdCpu(child) - before;
    if (round > 0) {
      results.push({
        wallMs: Math.round(wallMs),
        messagesPerSecond: Math.round((FRAMES * 1000) / wallMs),
        cpuMsPerFrame: Math.round((cpu / FRAMES) * 10000) / 10000,
      });
      log(`round ${round}: ${JSON.stringify(results.at(-1))}`);
    }
  }
  viewer.close();
  const cpuValues = results.map((r) => r.cpuMsPerFrame).sort((a, b) => a - b);
  process.stdout.write(
    `${JSON.stringify({ frames: FRAMES, bytes: SIZE, results, medianCpuMsPerFrame: cpuValues[Math.floor(cpuValues.length / 2)] }, null, 2)}\n`,
  );
} finally {
  if (relay) await relay.stop();
  try {
    rmSync(persistDir, { recursive: true, force: true, maxRetries: 10, retryDelay: 500 });
  } catch {
    // workerd can still hold its files for a moment on Windows; the temp directory is harmless.
  }
}
