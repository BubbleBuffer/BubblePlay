// Helpers for the M3-C1 Node tests under local workerd. A raw WebSocket client (the global
// WebSocket cannot send Origin or CF-Connecting-IP), device keys and relay proofs with the same
// encodings as bp-device-key, and a wrangler dev lifecycle that can stop and start again on the
// same --persist-to directory. Every value here is throwaway and reaches only local workerd.
import { spawn } from 'node:child_process';
import crypto from 'node:crypto';
import { existsSync } from 'node:fs';
import net from 'node:net';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const crateRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const wranglerBin = path.join(crateRoot, 'node_modules', '.bin', 'wrangler');

export const ORIGIN = 'https://relay.test';
export const TOKEN = 'm3-token-local';
export const ADMIN = 'm3-admin-local';
export const CLIENT_IP = '203.0.113.7';
export const VIEWER_SEED = 200;
export const CASE_TIMEOUT_MS = 30_000;
export const RESTART_TIMEOUT_MS = 240_000;

const READY_TIMEOUT_MS = 120_000;
const PKCS8_ED25519_PREFIX = '302e020100300506032b657004220420';
const RELAY_CONTEXT = Buffer.from('bubbleplay-relay-v1', 'utf8');
const ID_ALPHABET = '0123456789ABCDEFGHJKMNPQRSTVWXYZ';

// The runtime answers exactly `{"type":"ping"}` without waking the object. This spelling reaches
// the object, so its pong comes after the frames sent before it have been applied.
export const SYNC_PING = '{ "type": "ping" }';

export const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// Canonical hyphenated device ID for a raw public key, derived as bp-device-key derives it.
function deviceIdOf(publicKey) {
  const digest = crypto.createHash('sha256').update(publicKey).digest().subarray(0, 10);
  let pending = 0;
  let bits = 0;
  let encoded = '';
  for (const byte of digest) {
    pending = (pending << 8) | byte;
    bits += 8;
    while (bits >= 5) {
      bits -= 5;
      encoded += ID_ALPHABET[(pending >>> bits) & 31];
    }
    pending &= (1 << bits) - 1;
  }
  return [0, 4, 8, 12].map((start) => encoded.slice(start, start + 4)).join('-');
}

// The Ed25519 identity for a seed byte, as the native tests build it: 32 copies of the byte.
export function identity(seed) {
  const key = crypto.createPrivateKey({
    key: Buffer.concat([Buffer.from(PKCS8_ED25519_PREFIX, 'hex'), Buffer.alloc(32, seed)]),
    format: 'der',
    type: 'pkcs8',
  });
  const publicKey = Buffer.from(crypto.createPublicKey(key).export({ format: 'jwk' }).x, 'base64url');
  return { key, publicKey, id: deviceIdOf(publicKey), publicB64: publicKey.toString('base64url') };
}

export const viewerIdentity = () => identity(VIEWER_SEED);
export const b64 = (bytes) => Buffer.from(bytes).toString('base64url');

// uint32 big-endian length before each field, as bp-device-key's relay_proof_encoding writes it.
function lengthPrefixed(fields) {
  const parts = [];
  for (const field of fields) {
    const length = Buffer.alloc(4);
    length.writeUInt32BE(field.length, 0);
    parts.push(length, field);
  }
  return Buffer.concat(parts);
}

// The relay proof: Ed25519 over the SHA-256 of the length-prefixed fields, for the test origin.
export function relayProof(who, role, nonce) {
  const encoding = lengthPrefixed([
    RELAY_CONTEXT,
    Buffer.from(role, 'utf8'),
    Buffer.from(ORIGIN, 'utf8'),
    nonce,
    Buffer.from(who.id, 'utf8'),
    who.publicKey,
  ]);
  const hash = crypto.createHash('sha256').update(encoding).digest();
  return crypto.sign(null, hash, who.key).toString('base64url');
}

const OP_TEXT = 0x1;
const OP_BINARY = 0x2;
const OP_CLOSE = 0x8;
const OP_PING = 0x9;
const OP_PONG = 0xa;

// A single unfragmented client frame, masked as RFC 6455 requires.
function encodeFrame(opcode, payload) {
  const mask = crypto.randomBytes(4);
  let header;
  if (payload.length < 126) {
    header = Buffer.from([0x80 | opcode, 0x80 | payload.length]);
  } else if (payload.length < 65536) {
    header = Buffer.alloc(4);
    header[0] = 0x80 | opcode;
    header[1] = 0x80 | 126;
    header.writeUInt16BE(payload.length, 2);
  } else {
    header = Buffer.alloc(10);
    header[0] = 0x80 | opcode;
    header[1] = 0x80 | 127;
    header.writeBigUInt64BE(BigInt(payload.length), 2);
  }
  const body = Buffer.alloc(payload.length);
  for (let i = 0; i < payload.length; i += 1) body[i] = payload[i] ^ mask[i % 4];
  return Buffer.concat([header, mask, body]);
}

// The JSON `type` of a text frame, or undefined when it is not a JSON object.
function typeOf(text) {
  try {
    const value = JSON.parse(text);
    return value && typeof value === 'object' ? value.type : undefined;
  } catch {
    return undefined;
  }
}

// One upgraded WebSocket. Frames arrive in order into a queue; `next` consumes them.
class RawSocket {
  constructor(sock) {
    this.sock = sock;
    this.buffer = Buffer.alloc(0);
    this.queue = [];
    this.waiters = [];
    sock.on('data', (chunk) => {
      this.buffer = Buffer.concat([this.buffer, chunk]);
      this.parse();
    });
    sock.on('error', () => {});
    sock.on('close', () => {
      this.push({ kind: 'eof', code: 1006, reason: '' });
    });
  }

  parse() {
    for (;;) {
      if (this.buffer.length < 2) return;
      const opcode = this.buffer[0] & 0x0f;
      let length = this.buffer[1] & 0x7f;
      let offset = 2;
      if (length === 126) {
        if (this.buffer.length < 4) return;
        length = this.buffer.readUInt16BE(2);
        offset = 4;
      } else if (length === 127) {
        if (this.buffer.length < 10) return;
        length = Number(this.buffer.readBigUInt64BE(2));
        offset = 10;
      }
      if (this.buffer.length < offset + length) return;
      const payload = Buffer.from(this.buffer.subarray(offset, offset + length));
      this.buffer = this.buffer.subarray(offset + length);
      this.frame(opcode, payload);
    }
  }

  frame(opcode, payload) {
    if (opcode === OP_TEXT) {
      this.push({ kind: 'text', text: payload.toString('utf8') });
    } else if (opcode === OP_BINARY) {
      this.push({ kind: 'binary', data: payload });
    } else if (opcode === OP_CLOSE) {
      const code = payload.length >= 2 ? payload.readUInt16BE(0) : 1005;
      const reason = payload.length > 2 ? payload.subarray(2).toString('utf8') : '';
      this.push({ kind: 'close', code, reason });
    } else if (opcode === OP_PING) {
      this.sock.write(encodeFrame(OP_PONG, payload));
    }
  }

  push(message) {
    this.queue.push(message);
    const waiters = this.waiters;
    this.waiters = [];
    for (const wake of waiters) wake();
  }

  // Consumes frames until one matches. A close or EOF that is not the match fails the wait.
  async next(match, label, timeoutMs = 10_000) {
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      while (this.queue.length > 0) {
        const message = this.queue.shift();
        if (match(message)) return message;
        if (message.kind === 'close' || message.kind === 'eof') {
          throw new Error(
            `socket closed ${message.code} ${JSON.stringify(message.reason)} while waiting for ${label}`,
          );
        }
      }
      const left = deadline - Date.now();
      if (left <= 0) throw new Error(`timed out waiting for ${label}`);
      await new Promise((resolve) => {
        const timer = setTimeout(resolve, left);
        this.waiters.push(() => {
          clearTimeout(timer);
          resolve();
        });
      });
    }
  }

  async nextText(timeoutMs) {
    const message = await this.next((m) => m.kind === 'text', 'a text frame', timeoutMs);
    return message.text;
  }

  async nextJson(type, timeoutMs) {
    const message = await this.next(
      (m) => m.kind === 'text' && typeOf(m.text) === type,
      `a text frame of type ${type}`,
      timeoutMs,
    );
    return JSON.parse(message.text);
  }

  async nextBinary(timeoutMs) {
    const message = await this.next((m) => m.kind === 'binary', 'a binary frame', timeoutMs);
    return message.data;
  }

  async expectClose(timeoutMs) {
    const message = await this.next(
      (m) => m.kind === 'close' || m.kind === 'eof',
      'a close frame',
      timeoutMs,
    );
    return { code: message.code, reason: message.reason };
  }

  // Fails if any frame or close arrives within `ms`.
  async quiet(ms) {
    await sleep(ms);
    if (this.queue.length > 0) {
      throw new Error(`expected no frames, got ${JSON.stringify(this.queue[0])}`);
    }
  }

  sendJson(value) {
    this.sendRawText(JSON.stringify(value));
  }

  sendRawText(text) {
    this.sock.write(encodeFrame(OP_TEXT, Buffer.from(text, 'utf8')));
  }

  sendBinary(bytes) {
    this.sock.write(encodeFrame(OP_BINARY, Buffer.from(bytes)));
  }

  close() {
    this.sock.destroy();
  }
}

// Opens a WebSocket upgrade. Resolves { status: 101, socket } when accepted, or
// { status, socket: null } for an HTTP refusal. Headers are set exactly as given.
export function openSocket(port, target, { origin, cookie, ip = CLIENT_IP } = {}) {
  return new Promise((resolve, reject) => {
    const sock = net.connect({ host: '127.0.0.1', port });
    const lines = [
      `GET ${target} HTTP/1.1`,
      `Host: 127.0.0.1:${port}`,
      'Upgrade: websocket',
      'Connection: Upgrade',
      `Sec-WebSocket-Key: ${crypto.randomBytes(16).toString('base64')}`,
      'Sec-WebSocket-Version: 13',
    ];
    if (origin !== undefined) lines.push(`Origin: ${origin}`);
    if (cookie !== undefined) lines.push(`Cookie: ${cookie}`);
    if (ip) lines.push(`CF-Connecting-IP: ${ip}`);
    sock.on('error', reject);
    sock.setTimeout(15_000, () => {
      sock.destroy();
      reject(new Error(`no upgrade response for ${target}`));
    });
    sock.write(`${lines.join('\r\n')}\r\n\r\n`);
    let head = Buffer.alloc(0);
    const onData = (chunk) => {
      head = Buffer.concat([head, chunk]);
      const end = head.indexOf('\r\n\r\n');
      if (end === -1) return;
      sock.off('data', onData);
      sock.setTimeout(0);
      const status = Number(head.subarray(0, end).toString('latin1').split(' ')[1]);
      if (status !== 101) {
        sock.destroy();
        resolve({ status, socket: null });
        return;
      }
      const raw = new RawSocket(sock);
      const rest = head.subarray(end + 4);
      if (rest.length > 0) {
        raw.buffer = Buffer.from(rest);
        raw.parse();
      }
      resolve({ status, socket: raw });
    };
    sock.on('data', onData);
  });
}

export async function openOk(port, target, options) {
  const { status, socket } = await openSocket(port, target, options);
  if (status !== 101) throw new Error(`upgrade ${target} refused with ${status}`);
  return socket;
}

// Sends the signed hello and answers the register-challenge. Does not wait for hello-ok.
export async function proveViewer(socket, who, pair) {
  const hello = {
    type: 'hello',
    role: 'viewer',
    id: who.id,
    public: who.publicB64,
    turn: false,
  };
  if (pair !== undefined) hello.pair = b64(pair);
  socket.sendJson(hello);
  const challenge = await socket.nextJson('register-challenge', 10_000);
  const nonce = Buffer.from(challenge.nonce, 'base64url');
  socket.sendJson({ type: 'register-proof', signature: relayProof(who, 'viewer', nonce) });
}

// A native viewer on /ws (no Origin, so no gate cookie): signed hello, then its hello-ok.
export async function viewerJoin(port, pc, who = viewerIdentity()) {
  const socket = await openOk(port, `/ws?pc=${pc}`);
  await proveViewer(socket, who);
  const ok = await socket.nextJson('hello-ok', 10_000);
  return { socket, ok };
}

// Registers a host on /host-ws. With `admit` it also admits the fixed viewer identity, as a real
// host publishes its list. The sync ping returns only after everything sent before it is applied.
// `token` defaults to the throwaway relay token; pass null for a PC that is already registered.
export async function registerHost(port, who, { token = TOKEN, admit = true } = {}) {
  const socket = await openOk(port, `/host-ws?pc=${who.id}`);
  const hello = { type: 'hello', role: 'host', id: who.id, public: who.publicB64 };
  if (token) hello.token = token;
  socket.sendJson(hello);
  const challenge = await socket.nextJson('register-challenge', 10_000);
  const nonce = Buffer.from(challenge.nonce, 'base64url');
  socket.sendJson({ type: 'register-proof', signature: relayProof(who, 'host', nonce) });
  const ok = await socket.nextJson('hello-ok', 10_000);
  if (admit) {
    socket.sendJson({
      type: 'admit',
      viewers: [viewerIdentity().id],
      remotePairing: false,
    });
  }
  socket.sendRawText(SYNC_PING);
  await socket.nextJson('pong', 10_000);
  return { socket, ok };
}

// A media or file data socket: upgrade with the epoch, then the data hello with the ticket.
// Native viewers connect with no Origin, so the viewer side needs no cookie.
export async function dataSocket(port, target, pc, epoch, role, ticket) {
  const socket = await openOk(port, `${target}?pc=${pc}&epoch=${epoch}`);
  socket.sendJson({ type: 'hello', role, ticket });
  const ok = await socket.nextJson('hello-ok', 10_000);
  return { socket, ok };
}

const live = new Set();

function signalGroup(pid, sig) {
  try {
    process.kill(process.platform === 'win32' ? pid : -pid, sig);
  } catch {
    // Already gone.
  }
}

// Synchronous last resort for exits that skip the test hooks.
process.on('exit', () => {
  for (const child of live) signalGroup(child.pid, 'SIGKILL');
});

function freePort() {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.on('error', reject);
    server.listen(0, '127.0.0.1', () => {
      const { port } = server.address();
      server.close(() => resolve(port));
    });
  });
}

async function stopChild(child) {
  live.delete(child);
  if (child.exitCode !== null || child.signalCode !== null) return;
  signalGroup(child.pid, 'SIGTERM');
  const exited = new Promise((resolve) => child.once('exit', resolve));
  await Promise.race([exited, sleep(3000)]);
  if (child.exitCode === null && child.signalCode === null) signalGroup(child.pid, 'SIGKILL');
}

// Starts `wrangler dev --local` on a free port with the throwaway config as --var values and the
// given --persist-to directory. Resolves when the Worker answers. A config rejection (500) fails
// the start, so the cause shows up in the log instead of in a test.
export async function startRelay({ persistDir, maxConnections = 256 }) {
  if (!existsSync(path.join(crateRoot, 'build', 'worker', 'shim.mjs'))) {
    throw new Error('build/worker/shim.mjs is missing: run worker-build --release first');
  }
  if (!existsSync(wranglerBin)) {
    throw new Error('node_modules/.bin/wrangler is missing: install the pinned wrangler first');
  }
  const port = await freePort();
  const base = `http://127.0.0.1:${port}`;
  let output = '';
  const child = spawn(
    wranglerBin,
    [
      'dev',
      '--local',
      '--ip',
      '127.0.0.1',
      '--port',
      String(port),
      '--persist-to',
      persistDir,
      '--show-interactive-dev-session=false',
      '--var',
      `PUBLIC_ORIGIN:${ORIGIN}`,
      '--var',
      `RELAY_TOKEN:${TOKEN}`,
      '--var',
      `ADMIN_TOKEN:${ADMIN}`,
      '--var',
      `MAX_CONNECTIONS:${maxConnections}`,
    ],
    {
      cwd: crateRoot,
      detached: process.platform !== 'win32',
      env: { ...process.env, CI: '1', WRANGLER_SEND_METRICS: 'false' },
      stdio: ['ignore', 'pipe', 'pipe'],
    },
  );
  live.add(child);
  child.stdout.on('data', (d) => {
    output += d;
  });
  child.stderr.on('data', (d) => {
    output += d;
  });
  const stop = () => stopChild(child);
  const deadline = Date.now() + READY_TIMEOUT_MS;
  let lastErr = 'no response';
  try {
    for (;;) {
      if (child.exitCode !== null) {
        throw new Error(`wrangler exited with ${child.exitCode}\n${output.slice(-4000)}`);
      }
      if (Date.now() > deadline) {
        throw new Error(`wrangler not ready in ${READY_TIMEOUT_MS} ms (${lastErr})\n${output.slice(-4000)}`);
      }
      try {
        const res = await fetch(`${base}/`, { signal: AbortSignal.timeout(2000) });
        await res.arrayBuffer();
        if (res.status === 500) {
          throw new Error(`relay configuration rejected (500)\n${output.slice(-4000)}`);
        }
        break;
      } catch (err) {
        if (String(err).includes('configuration rejected')) throw err;
        lastErr = String(err);
      }
      await sleep(500);
    }
  } catch (err) {
    await stop();
    throw err;
  }
  return { port, base, stop, log: () => output };
}
