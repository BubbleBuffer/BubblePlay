#!/usr/bin/env node
// Round-trip latency of the relay's ctl channel, measured from a client outside the relay.
//
// Registers one throwaway host and admits one throwaway viewer with the same flow the M3-C1
// tests use (signed hello, register-proof, admit, sync ping, viewer hello). For each payload
// size, the viewer sends `count` ctl messages one at a time. The host echoes each one back, and
// the viewer times from its own send to the echo's arrival. Stdout gets one JSON document with
// p50, p95, p99 and max in milliseconds per size. Progress and errors go to stderr. Nothing is
// written to disk.
//
// Needs Node 24: it uses the built-in WebSocket, so there are no npm packages. Inputs are flags
// or env, and flags win:
//   --url     BENCH_URL     relay origin: https://host or http://127.0.0.1:<port>. wss:// and ws:// also work.
//   --token   BENCH_TOKEN   registration token (the RELAY_TOKEN secret). Required.
//   --origin  BENCH_ORIGIN  the relay's PUBLIC_ORIGIN, which the relay proof is bound to.
//                           Default: the origin of --url.
//   --count   BENCH_COUNT   messages per size (default 1000).
//   --sizes   BENCH_SIZES   payload bytes per message, comma separated (default 1024,16384,65536).
// Prefer BENCH_TOKEN. A --token value shows in the process list and in shell history.
//
// Each run registers a new throwaway PC in the relay's state. It stays there until the relay is torn down.

import crypto from 'node:crypto';
import { parseArgs } from 'node:util';

const DEFAULT_COUNT = 1000;
const MAX_COUNT = 99_999;
const DEFAULT_SIZES = '1024,16384,65536';
// Ctl data is unpadded base64url, capped by CTL_DATA_MAX in relay-rs/core (263168 characters).
// 196608 raw bytes encode to 262144 characters, which fits.
const MAX_PAYLOAD_BYTES = 196_608;
const CTL_KIND = 'bench';
const OPEN_TIMEOUT_MS = 15_000;
const STEP_TIMEOUT_MS = 10_000;
// Reaches the relay object, so its pong comes after the frames sent before it have been applied.
const SYNC_PING = '{ "type": "ping" }';
const RELAY_CONTEXT = Buffer.from('bubbleplay-relay-v1', 'utf8');

const USAGE = `usage: node scripts/bench-p99.mjs --url <relay origin> --token <registration token>
         [--origin <PUBLIC_ORIGIN>] [--count <n>] [--sizes <bytes,bytes,...>]
  env: BENCH_URL, BENCH_TOKEN, BENCH_ORIGIN, BENCH_COUNT, BENCH_SIZES (flags win)`;

class UsageError extends Error {}

const log = (message) => process.stderr.write(`bench: ${message}\n`);
const round = (ms) => Math.round(ms * 1000) / 1000;

function parseOptions(argv, env) {
  let parsed;
  try {
    parsed = parseArgs({
      args: argv,
      strict: true,
      options: {
        url: { type: 'string' },
        token: { type: 'string' },
        origin: { type: 'string' },
        count: { type: 'string' },
        sizes: { type: 'string' },
        help: { type: 'boolean', short: 'h' },
      },
    }).values;
  } catch (err) {
    throw new UsageError(err.message);
  }
  if (parsed.help) return { help: true };

  const pick = (flag, envName) => {
    const value = parsed[flag] !== undefined ? parsed[flag] : env[envName];
    return value ? value : undefined;
  };
  const url = pick('url', 'BENCH_URL');
  if (!url) throw new UsageError('--url (or BENCH_URL) is required');
  const token = pick('token', 'BENCH_TOKEN');
  if (!token) throw new UsageError('--token (or BENCH_TOKEN) is required');

  const bases = relayBases(url);
  const origin = checkOrigin(pick('origin', 'BENCH_ORIGIN') ?? bases.http);
  const count = parseCount(pick('count', 'BENCH_COUNT') ?? String(DEFAULT_COUNT));
  const sizes = parseSizes(pick('sizes', 'BENCH_SIZES') ?? DEFAULT_SIZES);
  return { bases, origin, token, count, sizes };
}

function relayBases(raw) {
  let url;
  try {
    url = new URL(raw);
  } catch {
    throw new UsageError(`--url is not a URL: ${raw}`);
  }
  const secure = url.protocol === 'https:' || url.protocol === 'wss:';
  if (!secure && url.protocol !== 'http:' && url.protocol !== 'ws:') {
    throw new UsageError('--url must start with https://, http://, wss:// or ws://');
  }
  if (url.pathname !== '/' || url.search || url.hash) {
    throw new UsageError('--url is the relay origin only, with no path, query or fragment');
  }
  return {
    http: `${secure ? 'https' : 'http'}://${url.host}`,
    ws: `${secure ? 'wss' : 'ws'}://${url.host}`,
  };
}

function checkOrigin(value) {
  let parsed = null;
  try {
    parsed = new URL(value);
  } catch {
    // Reported below.
  }
  if (!parsed || parsed.origin !== value) {
    throw new UsageError(`--origin must be exactly an origin such as https://relay.example.com (got ${value})`);
  }
  return value;
}

function parseCount(value) {
  if (!/^[1-9]\d*$/.test(value) || Number(value) > MAX_COUNT) {
    throw new UsageError(`--count must be a whole number from 1 to ${MAX_COUNT} (got ${value})`);
  }
  return Number(value);
}

function parseSizes(value) {
  const parts = value.split(',').map((part) => part.trim());
  for (const part of parts) {
    if (!/^[1-9]\d*$/.test(part) || Number(part) > MAX_PAYLOAD_BYTES) {
      throw new UsageError(`--sizes takes comma-separated byte counts from 1 to ${MAX_PAYLOAD_BYTES} (got ${value})`);
    }
  }
  return parts.map(Number);
}

// The Ed25519 identity helper lives with the M3-C1 relay tests (tests/m3-support.mjs). It is not on
// lane/rust-relay until that merge, so the import is deferred until a run actually starts.
async function loadIdentity() {
  try {
    return (await import('../tests/m3-support.mjs')).identity;
  } catch (err) {
    if (err?.code === 'ERR_MODULE_NOT_FOUND') {
      throw new Error('tests/m3-support.mjs is missing: merge lane/rust-relay-m3c (M3-C1) before running the bench');
    }
    throw err;
  }
}

// Throwaway keys. identity(seed) is the test helper's derivation, seeded with a random byte per run.
function freshIdentities(identity) {
  const hostSeed = crypto.randomInt(1, 256);
  let viewerSeed = crypto.randomInt(1, 256);
  while (viewerSeed === hostSeed) viewerSeed = crypto.randomInt(1, 256);
  return { host: identity(hostSeed), viewer: identity(viewerSeed) };
}

function lengthPrefixed(fields) {
  const parts = [];
  for (const field of fields) {
    const length = Buffer.alloc(4);
    length.writeUInt32BE(field.length, 0);
    parts.push(length, field);
  }
  return Buffer.concat(parts);
}

// Same encoding as the M3-C1 helper's relayProof. The origin is a parameter here, because the relay
// binds the proof to its PUBLIC_ORIGIN, which is not the test origin on a deployed relay.
function relayProof(who, role, nonce, origin) {
  const encoding = lengthPrefixed([
    RELAY_CONTEXT,
    Buffer.from(role, 'utf8'),
    Buffer.from(origin, 'utf8'),
    nonce,
    Buffer.from(who.id, 'utf8'),
    who.publicKey,
  ]);
  const hash = crypto.createHash('sha256').update(encoding).digest();
  return crypto.sign(null, hash, who.key).toString('base64url');
}

const isType = (type) => (item) =>
  item.json !== null && typeof item.json === 'object' && item.json.type === type;
const isCtl = (seq) => (item) => isType('ctl')(item) && item.json.seq === seq;
const nonceOf = (challenge) => Buffer.from(challenge.json.nonce, 'base64url');

// One WebSocket with a queue of text frames. take() consumes frames until one matches, and fails if
// the socket closes first. Each frame is stamped with its arrival time.
class Peer {
  #ws;
  #name;
  #queue = [];
  #waiters = [];
  #closed = null;

  constructor(ws, name) {
    this.#ws = ws;
    this.#name = name;
    ws.addEventListener('message', (event) => {
      if (typeof event.data !== 'string') return;
      let json = null;
      try {
        json = JSON.parse(event.data);
      } catch {
        json = null;
      }
      this.#queue.push({ text: event.data, json, at: performance.now() });
      this.#wake();
    });
    ws.addEventListener('close', (event) => {
      this.#closed = { code: event.code, reason: event.reason };
      this.#wake();
    });
  }

  #wake() {
    const waiters = this.#waiters;
    this.#waiters = [];
    for (const wake of waiters) wake();
  }

  async take(match, label, timeoutMs = STEP_TIMEOUT_MS) {
    const deadline = performance.now() + timeoutMs;
    for (;;) {
      while (this.#queue.length > 0) {
        const item = this.#queue.shift();
        if (match(item)) return item;
      }
      if (this.#closed) {
        throw new Error(
          `${this.#name}: socket closed ${this.#closed.code} "${this.#closed.reason}" while waiting for ${label}`,
        );
      }
      const left = deadline - performance.now();
      if (left <= 0) throw new Error(`${this.#name}: timed out waiting for ${label}`);
      await new Promise((resolve) => {
        const timer = setTimeout(resolve, left);
        this.#waiters.push(() => {
          clearTimeout(timer);
          resolve();
        });
      });
    }
  }

  sendJson(value) {
    this.sendText(JSON.stringify(value));
  }

  sendText(text) {
    this.#ws.send(text);
  }

  close() {
    try {
      this.#ws.close(1000);
    } catch {
      // Already closed.
    }
  }
}

function openPeer(url, name) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(url);
    const timer = setTimeout(() => {
      ws.close();
      reject(new Error(`${name}: no websocket open within ${OPEN_TIMEOUT_MS} ms`));
    }, OPEN_TIMEOUT_MS);
    ws.addEventListener(
      'open',
      () => {
        clearTimeout(timer);
        resolve(new Peer(ws, name));
      },
      { once: true },
    );
    ws.addEventListener(
      'error',
      () => {
        clearTimeout(timer);
        reject(new Error(`${name}: websocket upgrade refused or unreachable`));
      },
      { once: true },
    );
  });
}

// Host registration and admission, as registerHost() in the M3-C1 helper does it.
async function registerHost(bases, host, viewer, token, origin) {
  const hostPeer = await openPeer(`${bases.ws}/host-ws?pc=${host.id}`, 'host');
  try {
    hostPeer.sendJson({ type: 'hello', role: 'host', id: host.id, public: host.publicB64, token });
    const challenge = await hostPeer.take(isType('register-challenge'), 'register-challenge');
    hostPeer.sendJson({ type: 'register-proof', signature: relayProof(host, 'host', nonceOf(challenge), origin) });
    await hostPeer.take(isType('hello-ok'), 'hello-ok');
    hostPeer.sendJson({ type: 'admit', viewers: [viewer.id], remotePairing: false });
    hostPeer.sendText(SYNC_PING);
    await hostPeer.take(isType('pong'), 'pong');
    return hostPeer;
  } catch (err) {
    hostPeer.close();
    throw err;
  }
}

async function joinViewer(bases, host, viewer, origin) {
  const viewerPeer = await openPeer(`${bases.ws}/ws?pc=${host.id}`, 'viewer');
  try {
    viewerPeer.sendJson({ type: 'hello', role: 'viewer', id: viewer.id, public: viewer.publicB64, turn: false });
    const challenge = await viewerPeer.take(isType('register-challenge'), 'register-challenge');
    viewerPeer.sendJson({
      type: 'register-proof',
      signature: relayProof(viewer, 'viewer', nonceOf(challenge), origin),
    });
    await viewerPeer.take(isType('hello-ok'), 'hello-ok');
    return viewerPeer;
  } catch (err) {
    viewerPeer.close();
    throw err;
  }
}

// Nearest-rank percentile: the smallest sample with at least q percent of the samples at or below it.
function summarize(payloadBytes, dataChars, samples) {
  const sorted = [...samples].sort((a, b) => a - b);
  const at = (q) => round(sorted[Math.max(0, Math.ceil((q / 100) * sorted.length) - 1)]);
  return {
    payload_bytes: payloadBytes,
    data_chars: dataChars,
    samples: sorted.length,
    p50_ms: at(50),
    p95_ms: at(95),
    p99_ms: at(99),
    max_ms: round(sorted[sorted.length - 1]),
  };
}

async function measure(hostPeer, viewerPeer, sizes, count) {
  const results = [];
  let seq = 0;
  for (const payloadBytes of sizes) {
    const data = crypto.randomBytes(payloadBytes).toString('base64url');
    const samples = [];
    log(`${payloadBytes} bytes: ${count} round trips`);
    for (let i = 0; i < count; i += 1) {
      seq += 1;
      const text = JSON.stringify({ type: 'ctl', kind: CTL_KIND, seq, data });
      const sentAt = performance.now();
      viewerPeer.sendText(text);
      const atHost = await hostPeer.take(isCtl(seq), `ctl ${seq} at the host`);
      hostPeer.sendText(atHost.text);
      const back = await viewerPeer.take(isCtl(seq), `ctl ${seq} echoed to the viewer`);
      if (back.json.data !== data) throw new Error(`payload changed in transit (seq ${seq})`);
      samples.push(back.at - sentAt);
    }
    results.push(summarize(payloadBytes, data.length, samples));
  }
  return results;
}

async function run(opts) {
  const identity = await loadIdentity();
  const { host, viewer } = freshIdentities(identity);
  log(`registering a throwaway host on ${opts.bases.http}`);
  const hostPeer = await registerHost(opts.bases, host, viewer, opts.token, opts.origin);
  try {
    log('joining the admitted viewer');
    const viewerPeer = await joinViewer(opts.bases, host, viewer, opts.origin);
    try {
      return await measure(hostPeer, viewerPeer, opts.sizes, opts.count);
    } finally {
      viewerPeer.close();
    }
  } finally {
    hostPeer.close();
  }
}

async function main(argv, env) {
  let opts;
  try {
    opts = parseOptions(argv, env);
  } catch (err) {
    if (!(err instanceof UsageError)) throw err;
    process.stderr.write(`${err.message}\n${USAGE}\n`);
    return 2;
  }
  if (opts.help) {
    process.stdout.write(`${USAGE}\n`);
    return 0;
  }
  let results;
  try {
    results = await run(opts);
  } catch (err) {
    process.stderr.write(`bench failed: ${err.message}\n`);
    return 1;
  }
  const report = {
    relay: opts.bases.http,
    origin: opts.origin,
    channel: 'ctl',
    path: 'viewer to host and back, one message in flight',
    percentiles: 'nearest-rank',
    count: opts.count,
    node: process.version,
    measured_at: new Date().toISOString(),
    sizes: results,
  };
  process.stdout.write(`${JSON.stringify(report, null, 2)}\n`);
  return 0;
}

process.exitCode = await main(process.argv.slice(2), process.env);
