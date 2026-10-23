// Local workerd tests for the relay Durable Object. Each run starts `wrangler dev --local` from
// the crate root (after `worker-build --release`) on a free loopback port, with throwaway config
// values, and stops every process it started. The two M3-A cases keep their names. The `m3 `
// cases port the native e2e behaviours from relay-rs/server/tests.
import { after, before, test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import {
  CASE_TIMEOUT_MS,
  CLIENT_IP,
  ORIGIN,
  RESTART_TIMEOUT_MS,
  SYNC_PING,
  TOKEN,
  b64,
  dataSocket,
  identity,
  openOk,
  openSocket,
  proveViewer,
  registerHost,
  startRelay,
  viewerJoin,
} from './m3-support.mjs';

let relay = null;
let persistDir = '';

before(async () => {
  persistDir = mkdtempSync(path.join(tmpdir(), 'bp-relay-m3-'));
  relay = await startRelay({ persistDir });
});

after(async () => {
  if (relay) await relay.stop();
  relay = null;
  if (persistDir) rmSync(persistDir, { recursive: true, force: true });
});

// Restarts run on their own instance and persist directory, so the shared one is untouched.
async function withRelay(dir, fn) {
  const instance = await startRelay({ persistDir: dir });
  try {
    return await fn(instance);
  } finally {
    await instance.stop();
  }
}

function filePacket(payload) {
  return Buffer.concat([
    Buffer.from('BPF1', 'latin1'),
    Buffer.from([3, 0, 0, 0]),
    Buffer.alloc(8),
    payload,
  ]);
}

function authHello(who) {
  return {
    type: 'auth-hello',
    id: who.id,
    public: who.publicB64,
    nonce: b64(Buffer.alloc(32, 9)),
    ephemeral: b64(Buffer.alloc(32, 8)),
    caps: ['input'],
  };
}

function authHost(host) {
  return {
    type: 'auth-host',
    id: host.id,
    public: b64(Buffer.alloc(32, 1)),
    nonce: b64(Buffer.alloc(32, 9)),
    ephemeral: b64(Buffer.alloc(32, 8)),
    signature: b64(Buffer.alloc(64, 5)),
  };
}

// M3-A cases (names kept from the stub).

test('GET / returns 404 with an empty body', { timeout: CASE_TIMEOUT_MS }, async () => {
  const res = await fetch(`${relay.base}/`);
  assert.equal(res.status, 404);
  assert.equal(await res.text(), '');
});

test(
  'a text hello sent to /ws closes the socket with 1008 and an empty reason',
  { timeout: CASE_TIMEOUT_MS },
  async () => {
    const socket = await openOk(relay.port, `/ws?pc=${identity(70).id}`);
    socket.sendJson({ type: 'hello', role: 'viewer' });
    assert.deepEqual(await socket.expectClose(10_000), { code: 1008, reason: '' });
    socket.close();
  },
);

// Registration: native e2e_registration.rs.

test('m3 host hello registers the PC and returns hello-ok with a ticket', { timeout: CASE_TIMEOUT_MS }, async () => {
  const host = identity(1);
  const { socket, ok } = await registerHost(relay.port, host);
  assert.equal(ok.type, 'hello-ok');
  assert.equal(ok.pc, host.id);
  assert.ok(typeof ok.ticket === 'string' && ok.ticket.length > 0);
  socket.close();
});

test('m3 host re-registration with the same key is accepted without a token', { timeout: CASE_TIMEOUT_MS }, async () => {
  const host = identity(2);
  const first = await registerHost(relay.port, host);
  const second = await registerHost(relay.port, host, { token: null, admit: false });
  assert.equal(second.ok.pc, host.id);
  first.socket.close();
  second.socket.close();
});

test('m3 host with a public key that does not match its id is refused 1008 invalid hello', { timeout: CASE_TIMEOUT_MS }, async () => {
  const claimed = identity(3);
  const other = identity(4);
  const socket = await openOk(relay.port, `/host-ws?pc=${claimed.id}`);
  socket.sendJson({
    type: 'hello',
    role: 'host',
    id: claimed.id,
    public: other.publicB64,
    token: TOKEN,
  });
  assert.deepEqual(await socket.expectClose(10_000), { code: 1008, reason: 'invalid hello' });
});

test('m3 unknown PC without a token is refused 1008 invalid hello', { timeout: CASE_TIMEOUT_MS }, async () => {
  const host = identity(5);
  const socket = await openOk(relay.port, `/host-ws?pc=${host.id}`);
  socket.sendJson({ type: 'hello', role: 'host', id: host.id, public: host.publicB64 });
  assert.deepEqual(await socket.expectClose(10_000), { code: 1008, reason: 'invalid hello' });
});

test('m3 unknown PC with a wrong token is refused 1008 invalid hello', { timeout: CASE_TIMEOUT_MS }, async () => {
  const host = identity(6);
  const socket = await openOk(relay.port, `/host-ws?pc=${host.id}`);
  socket.sendJson({
    type: 'hello',
    role: 'host',
    id: host.id,
    public: host.publicB64,
    token: 'not-the-token',
  });
  assert.deepEqual(await socket.expectClose(10_000), { code: 1008, reason: 'invalid hello' });
});

test('m3 version probe answers hello-ok then closes 1000, and a wrong token is refused', { timeout: CASE_TIMEOUT_MS }, async () => {
  const pc = identity(7).id;
  const probe = await openOk(relay.port, `/host-version-ws?pc=${pc}`);
  probe.sendJson({ type: 'hello', role: 'version-probe', token: TOKEN });
  await probe.nextJson('hello-ok', 10_000);
  assert.deepEqual(await probe.expectClose(10_000), { code: 1000, reason: 'version-probe-complete' });

  const bad = await openOk(relay.port, `/host-version-ws?pc=${pc}`);
  bad.sendJson({ type: 'hello', role: 'version-probe', token: 'wrong' });
  assert.deepEqual(await bad.expectClose(10_000), { code: 1008, reason: 'invalid hello' });
});

// Admission: native e2e_gate_and_data.rs and e2e_m2.rs.

test('m3 native viewer without Origin upgrades and a bare hello is closed 1008 with an empty reason', { timeout: CASE_TIMEOUT_MS }, async () => {
  const socket = await openOk(relay.port, `/ws?pc=${identity(40).id}`);
  socket.sendJson({ type: 'hello', role: 'viewer' });
  assert.deepEqual(await socket.expectClose(10_000), { code: 1008, reason: '' });
});

test('m3 unlisted viewer is closed 1008 with an empty reason and the host stays connected', { timeout: CASE_TIMEOUT_MS }, async () => {
  const host = identity(42);
  const { socket: hostSocket } = await registerHost(relay.port, host);
  const viewer = await openOk(relay.port, `/ws?pc=${host.id}`);
  await proveViewer(viewer, identity(203));
  assert.deepEqual(await viewer.expectClose(10_000), { code: 1008, reason: '' });
  await hostSocket.quiet(500);
  hostSocket.sendRawText(SYNC_PING);
  await hostSocket.nextJson('pong', 10_000);
});

test('m3 admitted viewer gets hello-ok with a media epoch and a ticket', { timeout: CASE_TIMEOUT_MS }, async () => {
  const host = identity(43);
  await registerHost(relay.port, host);
  const { ok } = await viewerJoin(relay.port, host.id);
  assert.equal(ok.type, 'hello-ok');
  assert.ok(typeof ok.mediaEpoch === 'string' && ok.mediaEpoch.length > 0);
  assert.ok(typeof ok.ticket === 'string' && ok.ticket.length > 0);
});

// Browser origin: a browser Origin needs a valid gate cookie, which arrives in M3-C2. Until then
// the gate verdict is the only check on these routes.

test('m3 a wrong Origin on /ws is refused with 404', { timeout: CASE_TIMEOUT_MS }, async () => {
  const { status, socket } = await openSocket(relay.port, `/ws?pc=${identity(11).id}`, {
    origin: 'https://evil.test',
  });
  assert.equal(status, 404);
  assert.equal(socket, null);
});

test('m3 an Origin without a valid gate cookie is accepted then closed 1008 with an empty reason', { timeout: CASE_TIMEOUT_MS }, async () => {
  const { status, socket } = await openSocket(relay.port, `/ws?pc=${identity(44).id}`, {
    origin: ORIGIN,
  });
  assert.equal(status, 101);
  assert.deepEqual(await socket.expectClose(10_000), { code: 1008, reason: '' });
});

// Client IP: the relay reads CF-Connecting-IP only and refuses a request without one with 404 (the
// production edge always sends it). CI run 39878845827 showed that local workerd fills the header
// in when the client sends none, so the missing-IP 404 cannot be reached under wrangler dev.

test('m3 CF-Connecting-IP probe: local workerd fills in a missing client IP and a supplied one is accepted', { timeout: CASE_TIMEOUT_MS }, async () => {
  const pc = identity(45).id;
  const missing = await openSocket(relay.port, `/ws?pc=${pc}`, { ip: null });
  assert.equal(missing.status, 101);
  missing.socket.close();
  const present = await openSocket(relay.port, `/ws?pc=${pc}`, { ip: CLIENT_IP });
  assert.equal(present.status, 101);
  present.socket.close();
});

// Pairing: native e2e_m2.rs m2_pairing_over_real_sockets_forwards_auth_and_drops_offers.

test('m3 pairing viewer auth messages are forwarded to the host and its offers are dropped', { timeout: CASE_TIMEOUT_MS }, async () => {
  const host = identity(46);
  const { socket: hostSocket } = await registerHost(relay.port, host);
  const admit = Buffer.alloc(32, 9);
  hostSocket.sendJson({ type: 'pair-open', admit: b64(admit) });
  hostSocket.sendRawText(SYNC_PING);
  await hostSocket.nextJson('pong', 10_000);

  const stranger = identity(203);
  const viewer = await openOk(relay.port, `/ws?pc=${host.id}`);
  await proveViewer(viewer, stranger, admit);
  const ok = await viewer.nextJson('hello-ok', 10_000);
  assert.equal(ok.pairing, true);
  assert.equal(ok.ticket, undefined);

  viewer.sendJson({ type: 'offer', sdp: 'v=0' });
  viewer.sendJson(authHello(stranger));
  const first = JSON.parse(await hostSocket.nextText(10_000));
  assert.equal(first.type, 'auth-hello', 'the offer must not reach the host');

  hostSocket.sendJson(authHost(host));
  const back = await viewer.nextJson('auth-host', 10_000);
  assert.equal(back.type, 'auth-host');
});

// Ticket sockets: native e2e_gate_and_data.rs and e2e_m2.rs.

test('m3 a wrong ticket closes both media sockets 1008 with an empty reason', { timeout: CASE_TIMEOUT_MS }, async () => {
  const host = identity(47);
  await registerHost(relay.port, host);
  const { socket: viewer, ok } = await viewerJoin(relay.port, host.id);
  const bogus = b64(Buffer.alloc(32, 1));

  const hostMedia = await openOk(relay.port, `/host-media-ws?pc=${host.id}&epoch=${ok.mediaEpoch}`);
  hostMedia.sendJson({ type: 'hello', role: 'media-host', ticket: bogus });
  assert.deepEqual(await hostMedia.expectClose(10_000), { code: 1008, reason: '' });

  const viewerMedia = await openOk(relay.port, `/media-ws?pc=${host.id}&epoch=${ok.mediaEpoch}`);
  viewerMedia.sendJson({ type: 'hello', role: 'media-client', ticket: bogus });
  assert.deepEqual(await viewerMedia.expectClose(10_000), { code: 1008, reason: '' });
  viewer.close();
});

test('m3 an admitted viewer media ticket opens a working media socket', { timeout: CASE_TIMEOUT_MS }, async () => {
  const host = identity(48);
  await registerHost(relay.port, host);
  const { ok } = await viewerJoin(relay.port, host.id);
  const media = await openOk(relay.port, `/media-ws?pc=${host.id}&epoch=${ok.mediaEpoch}`);
  media.sendJson({ type: 'hello', role: 'media-client', ticket: ok.ticket });
  const mediaOk = await media.nextJson('hello-ok', 10_000);
  assert.equal(mediaOk.type, 'hello-ok');
  assert.equal(typeof mediaOk.registeredAtMs, 'number');
});

// Forwarding: native e2e_gate_and_data.rs and e2e_m2.rs.

test('m3 media frames forward host to viewer and keyframe requests forward viewer to host', { timeout: CASE_TIMEOUT_MS }, async () => {
  const host = identity(49);
  const { ok: hostOk } = await registerHost(relay.port, host);
  const { socket: viewer, ok: viewerOk } = await viewerJoin(relay.port, host.id);
  const epoch = viewerOk.mediaEpoch;
  const hostMedia = await dataSocket(relay.port, '/host-media-ws', host.id, epoch, 'media-host', hostOk.ticket);
  const viewerMedia = await dataSocket(relay.port, '/media-ws', host.id, epoch, 'media-client', viewerOk.ticket);

  hostMedia.socket.sendBinary(Buffer.alloc(64, 9));
  assert.deepEqual(await viewerMedia.socket.nextBinary(10_000), Buffer.alloc(64, 9));

  viewerMedia.socket.sendJson({ type: 'keyframe', reason: 'test' });
  const keyframe = await hostMedia.socket.nextJson('keyframe', 10_000);
  assert.equal(keyframe.reason, 'test');
  viewer.close();
});

test('m3 file socket forwards both ways and answers the exact ping text', { timeout: CASE_TIMEOUT_MS }, async () => {
  const host = identity(50);
  const { ok: hostOk } = await registerHost(relay.port, host);
  const { socket: viewer, ok: viewerOk } = await viewerJoin(relay.port, host.id);
  const epoch = viewerOk.mediaEpoch;
  const fileHost = await dataSocket(relay.port, '/host-file-ws', host.id, epoch, 'file-host', hostOk.ticket);
  const fileClient = await dataSocket(relay.port, '/file-ws', host.id, epoch, 'file-client', viewerOk.ticket);

  const packet = filePacket(Buffer.from('hello', 'utf8'));
  fileHost.socket.sendBinary(packet);
  assert.deepEqual(await fileClient.socket.nextBinary(10_000), packet);
  fileClient.socket.sendBinary(packet);
  assert.deepEqual(await fileHost.socket.nextBinary(10_000), packet);

  fileClient.socket.sendRawText('{"type":"ping"}');
  assert.equal(await fileClient.socket.nextText(10_000), '{"type":"pong"}');
  viewer.close();
});

test('m3 a new viewer epoch evicts stale media and refuses the old epoch with 404', { timeout: CASE_TIMEOUT_MS }, async () => {
  const host = identity(51);
  const { ok: hostOk } = await registerHost(relay.port, host);
  const first = await viewerJoin(relay.port, host.id);
  const epoch1 = first.ok.mediaEpoch;
  await dataSocket(relay.port, '/host-media-ws', host.id, epoch1, 'media-host', hostOk.ticket);
  const staleMedia = await dataSocket(relay.port, '/media-ws', host.id, epoch1, 'media-client', first.ok.ticket);

  const second = await viewerJoin(relay.port, host.id);
  assert.notEqual(second.ok.mediaEpoch, epoch1);

  assert.deepEqual(await first.socket.expectClose(10_000), { code: 1008, reason: 'peer replaced' });
  assert.deepEqual(await staleMedia.socket.expectClose(10_000), { code: 1001, reason: 'stale epoch' });

  const refused = await openSocket(relay.port, `/media-ws?pc=${host.id}&epoch=${epoch1}`);
  assert.equal(refused.status, 404, 'an old epoch no longer upgrades');
});

test('m3 ctl frames round-trip between the host and an admitted viewer', { timeout: CASE_TIMEOUT_MS }, async () => {
  const host = identity(52);
  const { socket: hostSocket } = await registerHost(relay.port, host);
  const { socket: viewer } = await viewerJoin(relay.port, host.id);

  viewer.sendJson({ type: 'ctl', kind: 'input', seq: 1, data: '' });
  const fromViewer = await hostSocket.nextJson('ctl', 10_000);
  assert.equal(fromViewer.seq, 1);

  hostSocket.sendJson({ type: 'ctl', kind: 'input', seq: 2, data: '' });
  const fromHost = await viewer.nextJson('ctl', 10_000);
  assert.equal(fromHost.seq, 2);
});

// Persistence: each case restarts wrangler on the same --persist-to directory.

test('m3 a restart keeps the registered PC and its admitted list', { timeout: RESTART_TIMEOUT_MS }, async () => {
  const dir = mkdtempSync(path.join(tmpdir(), 'bp-relay-m3-restart-'));
  const host = identity(60);
  try {
    await withRelay(dir, async (instance) => {
      const { socket } = await registerHost(instance.port, host);
      socket.close();
    });
    await withRelay(dir, async (instance) => {
      // The viewer was admitted before the restart. This host re-registers without admitting it again.
      const { socket } = await registerHost(instance.port, host, { token: null, admit: false });
      const { ok } = await viewerJoin(instance.port, host.id);
      assert.equal(ok.type, 'hello-ok', 'the admitted viewer still gets hello-ok after the restart');
      socket.close();
    });
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test('m3 after a restart a host re-registers with its key and a different key is refused', { timeout: RESTART_TIMEOUT_MS }, async () => {
  const dir = mkdtempSync(path.join(tmpdir(), 'bp-relay-m3-rereg-'));
  const host = identity(61);
  try {
    await withRelay(dir, async (instance) => {
      const { socket } = await registerHost(instance.port, host);
      socket.close();
    });
    await withRelay(dir, async (instance) => {
      const again = await registerHost(instance.port, host, { token: null, admit: false });
      assert.equal(again.ok.pc, host.id);
      again.socket.close();

      const other = identity(62);
      const refused = await openOk(instance.port, `/host-ws?pc=${host.id}`);
      refused.sendJson({
        type: 'hello',
        role: 'host',
        id: host.id,
        public: other.publicB64,
        token: TOKEN,
      });
      assert.deepEqual(await refused.expectClose(10_000), { code: 1008, reason: 'invalid hello' });
    });
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// Keepalive: the runtime answers the exact text frame without waking the object.

test('m3 the exact text {"type":"ping"} is answered with the exact text {"type":"pong"}', { timeout: CASE_TIMEOUT_MS }, async () => {
  const host = identity(53);
  const { socket } = await registerHost(relay.port, host);
  socket.sendRawText('{"type":"ping"}');
  assert.equal(await socket.nextText(10_000), '{"type":"pong"}');
});
