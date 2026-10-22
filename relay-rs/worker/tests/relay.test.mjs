// Local workerd tests for the M3-A stub. Starts `wrangler dev --local` from the
// crate root (after `worker-build --release`) on a free loopback port, runs the
// cases, and kills only the process group it started, on every exit path.
import { after, before, test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import net from 'node:net';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const crateRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const wranglerBin = path.join(crateRoot, 'node_modules', '.bin', 'wrangler');
const READY_TIMEOUT_MS = 120_000;
const CASE_TIMEOUT_MS = 30_000;

let child = null;
let base = '';
let output = '';

function freePort() {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.on('error', reject);
    srv.listen(0, '127.0.0.1', () => {
      const { port } = srv.address();
      srv.close(() => resolve(port));
    });
  });
}

// Signals the whole process group that wrangler started (POSIX), so workerd dies
// with it. Nothing else is touched. Graceful first, then forced after 3 s.
function signalGroup(pid, sig) {
  try {
    process.kill(process.platform === 'win32' ? pid : -pid, sig);
  } catch {
    // Already gone.
  }
}

async function stopWrangler() {
  if (!child) return;
  const proc = child;
  child = null;
  if (proc.exitCode !== null || proc.signalCode !== null) return;
  signalGroup(proc.pid, 'SIGTERM');
  const exited = new Promise((resolve) => proc.once('exit', resolve));
  const timer = new Promise((resolve) => setTimeout(resolve, 3000));
  await Promise.race([exited, timer]);
  if (proc.exitCode === null && proc.signalCode === null) signalGroup(proc.pid, 'SIGKILL');
}

// Synchronous last resort for exits that skip the after() hook.
process.on('exit', () => {
  if (child) signalGroup(child.pid, 'SIGKILL');
});
for (const sig of ['SIGINT', 'SIGTERM']) {
  process.on(sig, () => {
    stopWrangler().finally(() => process.exit(1));
  });
}

async function waitReady() {
  const deadline = Date.now() + READY_TIMEOUT_MS;
  let lastErr = 'no response';
  while (Date.now() < deadline) {
    if (child.exitCode !== null) {
      throw new Error(`wrangler exited with ${child.exitCode}\n${output.slice(-4000)}`);
    }
    try {
      const res = await fetch(`${base}/`, { signal: AbortSignal.timeout(2000) });
      await res.arrayBuffer();
      return;
    } catch (err) {
      lastErr = String(err);
    }
    await new Promise((r) => setTimeout(r, 500));
  }
  throw new Error(`wrangler not ready in ${READY_TIMEOUT_MS} ms (${lastErr})\n${output.slice(-4000)}`);
}

before(async () => {
  if (!existsSync(path.join(crateRoot, 'build', 'worker', 'shim.mjs'))) {
    throw new Error('build/worker/shim.mjs is missing: run worker-build --release first');
  }
  if (!existsSync(wranglerBin)) {
    throw new Error('node_modules/.bin/wrangler is missing: install the pinned wrangler first');
  }
  const port = await freePort();
  base = `http://127.0.0.1:${port}`;
  child = spawn(
    wranglerBin,
    ['dev', '--local', '--ip', '127.0.0.1', '--port', String(port), '--show-interactive-dev-session=false'],
    {
      cwd: crateRoot,
      detached: process.platform !== 'win32',
      env: { ...process.env, CI: '1', WRANGLER_SEND_METRICS: 'false' },
      stdio: ['ignore', 'pipe', 'pipe'],
    },
  );
  child.stdout.on('data', (d) => {
    output += d;
  });
  child.stderr.on('data', (d) => {
    output += d;
  });
  await waitReady();
});

after(async () => {
  await stopWrangler();
});

test('GET / returns 404 with an empty body', { timeout: CASE_TIMEOUT_MS }, async () => {
  const res = await fetch(`${base}/`);
  assert.equal(res.status, 404);
  assert.equal(await res.text(), '');
});

test(
  'a text hello sent to /ws closes the socket with 1008 and an empty reason',
  { timeout: CASE_TIMEOUT_MS },
  async () => {
    const ws = new WebSocket(`${base.replace(/^http/, 'ws')}/ws`);
    const closed = new Promise((resolve) => {
      ws.addEventListener('close', (ev) => resolve(ev));
    });
    await new Promise((resolve, reject) => {
      ws.addEventListener('open', () => resolve(), { once: true });
      ws.addEventListener('error', () => reject(new Error('websocket error before open')), { once: true });
    });
    ws.send(JSON.stringify({ type: 'hello' }));
    const ev = await closed;
    assert.equal(ev.code, 1008);
    assert.equal(ev.reason, '');
  },
);
