// Tests for scripts/bench-p99.mjs. The usage test checks the flags. The local workerd run starts the
// relay from tests/m3-support.mjs and runs the bench against it.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const scriptPath = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', 'scripts', 'bench-p99.mjs');
const RUN_TIMEOUT_MS = 180_000;

function runBench(args, env) {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [scriptPath, ...args], { env, stdio: ['ignore', 'pipe', 'pipe'] });
    let stdout = '';
    let stderr = '';
    child.stdout.on('data', (chunk) => {
      stdout += chunk;
    });
    child.stderr.on('data', (chunk) => {
      stderr += chunk;
    });
    const timer = setTimeout(() => {
      child.kill('SIGKILL');
      reject(new Error(`bench did not exit within ${RUN_TIMEOUT_MS} ms\n${stderr.slice(-4000)}`));
    }, RUN_TIMEOUT_MS);
    child.on('error', (err) => {
      clearTimeout(timer);
      reject(err);
    });
    child.on('close', (code) => {
      clearTimeout(timer);
      resolve({ code, stdout, stderr });
    });
  });
}

test('m3 bench script refuses to run without a relay URL and token', async () => {
  const env = { ...process.env, BENCH_URL: '', BENCH_TOKEN: '' };
  const { code, stdout, stderr } = await runBench([], env);
  assert.equal(code, 2);
  assert.equal(stdout, '');
  assert.match(stderr, /--url \(or BENCH_URL\) is required/);
});

test('m3 bench script runs against local workerd', { timeout: RUN_TIMEOUT_MS + 60_000 }, async () => {
  const { startRelay, TOKEN } = await import('./m3-support.mjs');
  const persistDir = mkdtempSync(path.join(tmpdir(), 'bp-m3-bench-'));
  let relay = null;
  try {
    relay = await startRelay({ persistDir });
    const { code, stdout, stderr } = await runBench(
      ['--url', relay.base, '--origin', relay.origin, '--count', '5', '--sizes', '1024,65536'],
      { ...process.env, BENCH_TOKEN: TOKEN },
    );
    assert.equal(code, 0, stderr.slice(-4000));
    const report = JSON.parse(stdout);
    assert.deepEqual(Object.keys(report).sort(), [
      'channel',
      'count',
      'measured_at',
      'node',
      'origin',
      'path',
      'percentiles',
      'relay',
      'sizes',
    ]);
    assert.equal(report.channel, 'ctl');
    assert.equal(report.count, 5);
    assert.equal(report.origin, relay.origin);
    assert.equal(report.sizes.length, 2);
    for (const [index, expected] of [1024, 65536].entries()) {
      const size = report.sizes[index];
      assert.deepEqual(Object.keys(size).sort(), [
        'data_chars',
        'max_ms',
        'p50_ms',
        'p95_ms',
        'p99_ms',
        'payload_bytes',
        'samples',
      ]);
      assert.equal(size.payload_bytes, expected);
      assert.equal(size.samples, 5);
      for (const key of ['data_chars', 'max_ms', 'p50_ms', 'p95_ms', 'p99_ms']) {
        assert.equal(typeof size[key], 'number', key);
        assert.ok(Number.isFinite(size[key]), key);
      }
    }
  } finally {
    if (relay) await relay.stop();
    rmSync(persistDir, { recursive: true, force: true });
  }
});
