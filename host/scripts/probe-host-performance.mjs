// Local WSS v4 host-only probe. Binary AUs are ACKed and discarded, never decoded or saved.
import { createRequire } from 'node:module';
import { spawn } from 'node:child_process';
import { createServer } from 'node:http';
import { createHash } from 'node:crypto';
import { createWriteStream } from 'node:fs';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import os from 'node:os';

const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
const require = createRequire(path.join(repo, 'worker/package.json'));
const { WebSocketServer, WebSocket } = require('ws');
const options = Object.fromEntries(process.argv.slice(2).reduce((pairs, value, index, values) => {
  if (index % 2 === 0) pairs.push([value.replace(/^--/, ''), values[index + 1]]);
  return pairs;
}, []));
const label = options.label ?? 'baseline';
if (!['baseline', 'candidate'].includes(label)) throw new Error('--label must be baseline or candidate');
const exe = path.resolve(options.exe ?? path.join(repo, label === 'baseline'
  ? 'host/artifacts/perf/baseline-host.exe' : 'host/target/release/bubbleplay-host.exe'));
const outputRoot = path.join(repo, 'host/artifacts/perf');
if (options.analyze) {
  const source = path.resolve(options.analyze);
  if (!source.toLowerCase().startsWith(`${outputRoot.toLowerCase()}${path.sep}`)) {
    throw new Error('Analysis input must be a probe artifact');
  }
  const report = JSON.parse(await readFile(source, 'utf8'));
  analyze(report);
  const result = { label: report.label, source, executableSha256: report.executableSha256,
    registrations: report.registrations.length, phases: report.phases, logCounts: report.logCounts,
    sessions: report.sessions.map(value => ({ index: value.index, requestedAtMs: value.requestedAtMs,
      registrationAtMs: value.registrationAtMs, firstIdrAfterRequestMs: value.firstIdrAfterRequestMs,
      firstIdrAfterMediaHelloMs: value.firstIdrAfterMediaHelloMs,
      mediaConnectedAfterRequestMs: value.mediaConnectedAfterRequestMs,
      mediaHelloAfterRequestMs: value.mediaHelloAfterRequestMs,
      frames: value.frames, keyframes: value.keyframes, idrsWithinFirstSecond: value.idrsWithinFirstSecond,
      firstFrameReceipts: value.firstFrameReceipts })) };
  await writeFile(path.join(path.dirname(source), 'analysis.json'), `${JSON.stringify(result, null, 2)}\n`);
  console.log(JSON.stringify(result, null, 2));
  process.exit(0);
}
const outDir = path.resolve(options.out ?? path.join(outputRoot,
  `${label}-${new Date().toISOString().replaceAll(':', '-').replaceAll('.', '-')}`));
if (!outDir.toLowerCase().startsWith(`${outputRoot.toLowerCase()}${path.sep}`)) {
  throw new Error('Output must be a new directory under host/artifacts/perf');
}
const attachedMs = Number(options['attached-ms'] ?? 20_000);
const parkedMs = Number(options['parked-ms'] ?? 10_000);
const monitor = options.monitor ?? 'primary';
if (!(attachedMs >= 1_000 && attachedMs <= 60_000 && parkedMs >= 0 && parkedMs <= 60_000)) {
  throw new Error('Invalid bounded phase duration');
}
await mkdir(outDir, { recursive: false });
const executableSha256 = createHash('sha256').update(await readFile(exe)).digest('hex').toUpperCase();
if (label === 'baseline' && executableSha256 !== 'ECBEE16AB0D9AECE37AB20B8712EA30613F82CC0A18EAB8F19B963D2C1715227') {
  throw new Error('Baseline executable identity differs from the preserved baseline');
}

const logStream = createWriteStream(path.join(outDir, 'host.log'));
const summary = {
  label, executable: exe, executableSha256, startedAt: new Date().toISOString(),
  successCriteria: 'Both synthetic peer epochs receive a key AU; completion does not claim decode or presentation.',
  environment: { platform: process.platform, node: process.version, logicalCpusNode: os.cpus().length,
    monitorSelection: monitor, desktopStimulus: 'none; ambient desktop activity uncontrolled',
    profile: 'WSS v4 high/hevc work; received bytes ACKed without decodedSeq or presentation claims',
    limitations: ['No decode, presentation, image quality or end-to-end input latency measurement.',
      'No UAC, lock, Admin mode, monitor switch, follow-window or production relay test.',
      'Only this probe host PID is sampled and stopped; no installed host/tray is managed.',
      'Frame payloads and clipboard content are discarded, never persisted.'] },
  registrations: [], connections: [], sessions: [], phases: [], windows: [], cpuSamples: [], monitorLists: [],
  logCounts: {}, messageCounts: {}, passed: false,
};
let child;
let sampler;
let primary;
let activeSession;
let phase;
let hostExit;
let logBuffer = '';
let sampleBuffer = '';
const peers = new Set();
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
const send = (socket, message) => {
  if (socket?.readyState === WebSocket.OPEN) socket.send(JSON.stringify(message));
};
const waitFor = async (predicate, timeoutMs, description) => {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (predicate()) return;
    if (hostExit) throw new Error(`Host exited before ${description}: ${JSON.stringify(hostExit)}`);
    await sleep(20);
  }
  throw new Error(`Timed out waiting for ${description}`);
};

const server = createServer((_request, response) => { response.writeHead(404).end(); });
const wsServer = new WebSocketServer({ noServer: true, maxPayload: 9 * 1024 * 1024 });
server.on('upgrade', (request, socket, head) => {
  wsServer.handleUpgrade(request, socket, head, connection => wsServer.emit('connection', connection, request));
});
wsServer.on('connection', (socket, request) => {
  const url = new URL(request.url, 'http://127.0.0.1');
  const kind = url.pathname.includes('media') ? 'media' : url.pathname.includes('file') ? 'file' : 'signaling';
  const epoch = url.searchParams.get('epoch');
  const connection = { kind, epoch, connectedAtMs: Date.now(), helloAtMs: null, closedAtMs: null };
  socket.probeConnection = connection;
  summary.connections.push(connection);
  peers.add(socket);
  socket.on('close', () => {
    connection.closedAtMs = Date.now();
    peers.delete(socket);
    if (primary === socket) primary = null;
  });
  socket.on('error', () => {});
  socket.on('message', (bytes, binary) => {
    if (binary) {
      if (kind !== 'media' || bytes.length < 28 || bytes.readUInt32BE(0) !== 0x42505634) return;
      const metadata = { receivedAtMs: Date.now(), bytes: bytes.length, key: Boolean(bytes[5] & 1),
        refine: Boolean(bytes[5] & 2), codec: bytes[6], generation: bytes.readUInt32BE(8),
        sequence: bytes.readUInt32BE(12), captureTimestampUs: bytes.readBigUInt64BE(16).toString(),
        width: bytes.readUInt16BE(24), height: bytes.readUInt16BE(26) };
      const session = summary.sessions.find(value => value.epoch === epoch);
      if (session) {
        session.frames += 1;
        session.frameReceipts.push(metadata);
        session.bytes += metadata.bytes;
        session.keyframes += Number(metadata.key);
        if (!session.firstFrame) session.firstFrame = metadata;
        if (metadata.key && !session.firstIdr) session.firstIdr = metadata;
        if (metadata.key) session.idrReceipts.push(metadata);
      }
      // This acknowledges receipt only. It deliberately makes no decode/presentation claim.
      send(socket, { type: 'ack', generation: metadata.generation, seq: metadata.sequence, recvTsMs: Date.now() });
      return;
    }
    let message;
    try { message = JSON.parse(bytes.toString()); } catch { return; }
    summary.messageCounts[message.type] = (summary.messageCounts[message.type] ?? 0) + 1;
    if (message.type === 'hello') {
      connection.helloAtMs = Date.now();
      socket.hello = true;
      send(socket, { type: 'hello-ok' });
      if (kind === 'signaling' && message.role === 'host') {
        primary = socket;
        summary.registrations.push({ receivedAtMs: Date.now() });
      }
      if (kind === 'media') send(socket, { type: 'media-peer', present: activeSession?.epoch === epoch });
    } else if (message.type === 'ping') {
      send(socket, { type: 'pong' });
    } else if (message.type === 'first-frame') {
      // Only save receipt metadata from this synthetic request, never arbitrary JSON.
      const session = summary.sessions.find(value => value.requestId === message.requestId);
      if (session) session.firstFrameReceipts.push({ receivedAtMs: Date.now(), phase: message.phase,
        generation: message.generation, sequence: message.sequence, codec: message.codec, reason: message.reason });
    } else if (message.type === 'monitor-list' && Array.isArray(message.monitors)) {
      summary.monitorLists.push({ receivedAtMs: Date.now(), monitors: message.monitors.map(value => ({
        selector: value.selector, width: value.width, height: value.height,
        origin_x: value.origin_x, origin_y: value.origin_y, selected: value.selected,
      })) });
    } else if (message.type === 'desktop-state') {
      if (activeSession) activeSession.desktopStates.push({ receivedAtMs: Date.now(), state: message.state,
        desktop: message.desktop, reason: message.reason });
    }
  });
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const workerUrl = `ws://127.0.0.1:${server.address().port}/lab-host-ws?run=123e4567-e89b-42d3-a456-426614174000&compat=wss-v4-bench2-hevc-tiles-bpip4-v16`;
summary.environment.workerUrl = workerUrl;

function onLogs(chunk) {
  logStream.write(chunk);
  logBuffer += chunk.toString();
  while (logBuffer.includes('\n')) {
    const index = logBuffer.indexOf('\n');
    const line = logBuffer.slice(0, index).replace(/\r$/, '');
    logBuffer = logBuffer.slice(index + 1);
    for (const needle of ['NVENC initialized', 'pinned capture D3D11 device for NVENC session', 'NVENC session opened',
      'recreating NVENC for the WSS v4 GOP mode', 'prewarmed', 'prewarming', 'signaling registered as host', 'DDA', 'capture opened',
      'session ended; re-arming', 'viewer left; host pipeline remains warm', 'serving via Worker', 'WSS media egress connected']) {
      if (line.includes(needle)) summary.logCounts[needle] = (summary.logCounts[needle] ?? 0) + 1;
    }
    if (line.includes('WSS producer window')) {
      const fields = {};
      for (const match of line.matchAll(/\b([a-z_]+)=([0-9]+(?:\.[0-9]+)?)/g)) fields[match[1]] = Number(match[2]);
      const loggedAtMs = Date.parse(line.split(' ')[0]);
      const previous = summary.windows.at(-1);
      const sessionIndex = activeSession?.index ?? null;
      const inferredSpanMs = previous?.sessionIndex === sessionIndex && Number.isFinite(loggedAtMs)
        && loggedAtMs > previous.loggedAtMs && loggedAtMs - previous.loggedAtMs < 5_000
        ? loggedAtMs - previous.loggedAtMs : null;
      summary.windows.push({ receivedAtMs: Date.now(), loggedAtMs, sessionIndex,
        phase: phase?.name ?? 'setup', ...fields, normalizedSpanMs: fields.span_ms ?? inferredSpanMs,
        spanSource: fields.span_ms === undefined ? 'successive host log timestamps' : 'host span_ms' });
    }
  }
}

function startSampler(pid) {
  const names = `Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class ProbeThreadNames {
  [DllImport("kernel32.dll", SetLastError=true)] static extern IntPtr OpenThread(uint access, bool inherit, uint id);
  [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
  [DllImport("kernel32.dll")] static extern int GetThreadDescription(IntPtr handle, out IntPtr description);
  [DllImport("kernel32.dll")] static extern uint GetProcessIdOfThread(IntPtr handle);
  [DllImport("kernel32.dll")] static extern IntPtr LocalFree(IntPtr memory);
  public static string LastError;
  public static string Read(int id, int expectedPid) {
    LastError = null;
    IntPtr handle = OpenThread(0x0800, false, (uint)id), description = IntPtr.Zero;
    if (handle == IntPtr.Zero) { LastError = "OpenThread:" + Marshal.GetLastWin32Error(); return null; }
    try {
      if (GetProcessIdOfThread(handle) != (uint)expectedPid) { LastError = "thread-owner-changed"; return null; }
      int result = GetThreadDescription(handle, out description);
      if (result < 0) { LastError = "GetThreadDescription:" + result; return null; }
      return Marshal.PtrToStringUni(description);
    } finally { if (description != IntPtr.Zero) LocalFree(description); CloseHandle(handle); }
  }
}
'@\n`;
  const script = names + `$TaskHostPid = ${pid}\n$LogicalCpus = [Environment]::ProcessorCount\nwhile ($true) {\n  $TaskProcess = Get-Process -Id $TaskHostPid -ErrorAction SilentlyContinue\n  if ($null -eq $TaskProcess) { break }\n  $TaskThreads = @()\n  foreach ($TaskThread in $TaskProcess.Threads) {\n    try {\n      $TaskRow = @{id=$TaskThread.Id; cpuMs=$TaskThread.TotalProcessorTime.TotalMilliseconds; state=$TaskThread.ThreadState.ToString(); name=[ProbeThreadNames]::Read($TaskThread.Id, $TaskHostPid)}\n      $TaskRow.nameError = [ProbeThreadNames]::LastError\n      if ($TaskThread.ThreadState -eq [Diagnostics.ThreadState]::Wait) { $TaskRow.waitReason=$TaskThread.WaitReason.ToString() }\n      $TaskThreads += $TaskRow\n    } catch { }\n  }\n  @{ pid=$TaskHostPid; atMs=[DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds(); cpuMs=$TaskProcess.TotalProcessorTime.TotalMilliseconds; logicalCpus=$LogicalCpus; threads=$TaskThreads } | ConvertTo-Json -Compress -Depth 4\n  Start-Sleep -Milliseconds 1000\n}`;
  const encoded = Buffer.from(script, 'utf16le').toString('base64');
  sampler = spawn('powershell.exe', ['-NoProfile', '-NonInteractive', '-EncodedCommand', encoded],
    { windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] });
  sampler.stdout.on('data', bytes => {
    sampleBuffer += bytes.toString();
    while (sampleBuffer.includes('\n')) {
      const index = sampleBuffer.indexOf('\n');
      const line = sampleBuffer.slice(0, index).trim();
      sampleBuffer = sampleBuffer.slice(index + 1);
      try { summary.cpuSamples.push(JSON.parse(line)); } catch { /* not a sample */ }
    }
  });
  sampler.stderr.on('data', () => {});
}

async function runPhase(name, milliseconds) {
  phase = { name, startAtMs: Date.now(), requestedDurationMs: milliseconds };
  summary.phases.push(phase);
  await sleep(milliseconds);
  phase.endAtMs = Date.now();
}

async function attach(index) {
  await waitFor(() => primary?.hello && primary.readyState === WebSocket.OPEN, 20_000, 'registered signaling host');
  const session = { index, epoch: String(index).repeat(32), requestId: `local_probe_${index}`,
    requestedAtMs: Date.now(), registrationAtMs: primary.probeConnection.helloAtMs,
    frames: 0, bytes: 0, keyframes: 0, frameReceipts: [], idrReceipts: [], firstFrameReceipts: [], desktopStates: [] };
  summary.sessions.push(session);
  activeSession = session;
  send(primary, { type: 'media-control', action: 'start', protocol: 4, epoch: session.epoch,
    codec: 'hevc', quality: 'high', streamMode: 'work' });
  send(primary, { type: 'first-frame', protocol: 1, phase: 'request', requestId: session.requestId,
    transport: 'wss', epoch: session.epoch });
  await waitFor(() => session.firstIdr, 20_000, `session ${index} first received IDR`);
  session.firstIdrAfterRequestMs = session.firstIdr.receivedAtMs - session.requestedAtMs;
  await runPhase(`attached-${index}`, attachedMs);
}

function analyze(summary) {
  for (const item of summary.phases) {
    const samples = summary.cpuSamples.filter(value => value.atMs >= item.startAtMs && value.atMs <= item.endAtMs);
    if (samples.length >= 2) {
      const first = samples[0]; const last = samples.at(-1);
      item.cpuSamples = samples.length;
      item.cpuWallMs = last.atMs - first.atMs;
      item.cpuTimeMs = last.cpuMs - first.cpuMs;
      item.logicalCpus = last.logicalCpus;
      item.hostCpuMachinePct = item.cpuTimeMs / item.cpuWallMs / item.logicalCpus * 100;
      item.hostCpuOneCorePct = item.cpuTimeMs / item.cpuWallMs * 100;
    }
    const threadSamples = new Map();
    for (const sample of samples) {
      for (const thread of sample.threads ?? []) {
        const value = { atMs: sample.atMs, logicalCpus: sample.logicalCpus, ...thread };
        const previous = threadSamples.get(thread.id);
        if (previous) { previous.last = value; previous.samples += 1; }
        else threadSamples.set(thread.id, { first: value, last: value, samples: 1 });
      }
    }
    item.threadCpu = [...threadSamples].filter(([, value]) => value.samples >= 2).map(([id, value]) => {
      const cpuTimeMs = value.last.cpuMs - value.first.cpuMs;
      const wallMs = value.last.atMs - value.first.atMs;
      return { id, samples: value.samples, cpuTimeMs, wallMs,
        cpuMachinePct: cpuTimeMs / wallMs / value.last.logicalCpus * 100,
        name: value.last.name, nameError: value.last.nameError,
        state: value.last.state, waitReason: value.last.waitReason };
    }).sort((left, right) => right.cpuTimeMs - left.cpuTimeMs);
    const windows = summary.windows.filter(value => value.loggedAtMs >= item.startAtMs + 1_000
      && value.loggedAtMs <= item.endAtMs - 500 && value.normalizedSpanMs > 0
      && value.loggedAtMs - value.normalizedSpanMs >= item.startAtMs);
    item.producerWindows = windows.length;
    for (const field of ['producer_loop_iterations', 'capture_acquired', 'capture_new']) {
      const values = windows.map(value => value[field]).filter(Number.isFinite);
      if (values.length) item[`${field}_mean_per_window`] = values.reduce((sum, value) => sum + value, 0) / values.length;
      if (values.length) item[`${field}_normalized_per_second`] = windows.reduce((sum, value) => sum + (value[field] ?? 0), 0)
        / windows.reduce((sum, value) => sum + value.normalizedSpanMs / 1_000, 0);
    }
    const zeroNew = windows.filter(value => value.capture_new === 0);
    if (zeroNew.length) {
      const durationMs = zeroNew.reduce((sum, value) => sum + value.normalizedSpanMs, 0);
      const rates = zeroNew.map(value => value.producer_loop_iterations / value.normalizedSpanMs * 1_000);
      item.zeroNewFrameWindows = { count: zeroNew.length, durationMs,
        producerLoopsPerSecond: zeroNew.reduce((sum, value) => sum + value.producer_loop_iterations, 0)
          / durationMs * 1_000,
        minimumLoopRate: Math.min(...rates), maximumLoopRate: Math.max(...rates),
        windows: zeroNew.map(value => ({ loggedAtMs: value.loggedAtMs, spanMs: value.normalizedSpanMs,
          producerLoops: value.producer_loop_iterations, captureAcquired: value.capture_acquired })) };
    }
    const frames = summary.sessions.flatMap(session => session.frameReceipts).filter(value =>
      value.receivedAtMs >= item.startAtMs && value.receivedAtMs <= item.endAtMs);
    item.receivedFrames = frames.length;
    item.receivedFps = frames.length / ((item.endAtMs - item.startAtMs) / 1_000);
  }
  for (const session of summary.sessions) {
    const connection = summary.connections.find(value => value.kind === 'media' && value.epoch === session.epoch);
    if (connection) {
      session.mediaConnectedAfterRequestMs = connection.connectedAtMs - session.requestedAtMs;
      session.mediaHelloAfterRequestMs = connection.helloAtMs - session.requestedAtMs;
      if (session.firstIdr) session.firstIdrAfterMediaHelloMs = session.firstIdr.receivedAtMs - connection.helloAtMs;
    }
    session.idrsWithinFirstSecond = session.firstIdr
      ? session.idrReceipts.filter(value => value.receivedAtMs <= session.firstIdr.receivedAtMs + 1_000).length : 0;
  }
}

try {
  await mkdir(path.join(outDir, 'profile'), { recursive: true });
  child = spawn(exe, ['--mode', 'serve', '--worker', workerUrl, '--token', 'local-probe-synthetic-token',
    '--host-gate-token', 'local-probe-synthetic-bearer', '--monitor', monitor],
  { cwd: path.join(repo, 'host'), windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'], env: {
    ...process.env, RUST_LOG: 'info', LOCALAPPDATA: path.join(outDir, 'profile'),
    BUBBLEPLAY_FILE_CLIPBOARD: '0', BUBBLEPLAY_DOWNLOADS_DIR: path.join(outDir, 'downloads'),
    BUBBLEPLAY_SENDTO_DIR: path.join(outDir, 'sendto'), BUBBLEPLAY_LAB_RESET_ENCODER_ON_DIAGNOSTIC_KEYFRAME: '0',
  } });
  summary.hostPid = child.pid;
  child.stdout.on('data', onLogs);
  child.stderr.on('data', onLogs);
  child.on('exit', (code, signal) => { hostExit = { code, signal, atMs: Date.now() }; });
  child.on('error', error => { hostExit = { error: error.message, atMs: Date.now() }; });
  startSampler(child.pid);
  await attach(1);
  activeSession = null;
  for (const socket of peers) {
    // Every media connection is ours; presence changes do not close signaling.
    if (socket.probeConnection.kind === 'media') send(socket, { type: 'media-peer', present: false });
  }
  send(primary, { type: 'peer-left' });
  await runPhase('parked', parkedMs);
  await attach(2);
  summary.passed = true;
} catch (error) {
  summary.blocker = error.message;
} finally {
  summary.endedAt = new Date().toISOString();
  if (child && !hostExit) child.kill(); // Only the child PID we spawned.
  if (sampler) sampler.kill();
  for (const socket of peers) socket.terminate();
  await new Promise(resolve => server.close(resolve));
  wsServer.close();
  logStream.end();
  analyze(summary);
  await writeFile(path.join(outDir, 'summary.json'), `${JSON.stringify(summary, null, 2)}\n`);
  console.log(JSON.stringify({ label, outDir, passed: summary.passed, blocker: summary.blocker,
    hostPid: summary.hostPid, registrations: summary.registrations.length,
    sessions: summary.sessions.map(value => ({ index: value.index, firstIdrAfterRequestMs: value.firstIdrAfterRequestMs,
      frames: value.frames, keyframes: value.keyframes, idrsWithinFirstSecond: value.idrsWithinFirstSecond })),
    phases: summary.phases.map(({ threadCpu, ...value }) => ({ ...value, threadCpu: threadCpu?.slice(0, 8) })) }, null, 2));
}
if (!summary.passed) process.exitCode = 1;
