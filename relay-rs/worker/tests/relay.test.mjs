// Local workerd tests for the relay Durable Object. Each run starts `wrangler dev --local` from
// the crate root (after `worker-build --release`) on a free loopback port, with throwaway config
// values, and stops every process it started. Every case name carries the `m3 ` prefix; the GET /
// case is replaced by the gate's silent hold (M3-C2). The `m3 ` cases port the native e2e behaviours
// from relay-rs/server/tests.
import { after, before, test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  ADMIN,
  CASE_TIMEOUT_MS,
  CLIENT_IP,
  NTFY_TOKEN,
  NTFY_TOPIC,
  RESTART_TIMEOUT_MS,
  SYNC_PING,
  TOKEN,
  approveBrowser,
  b64,
  decide,
  gateCookieOf,
  heldNavigation,
  browserJoin,
  dataSocket,
  freshClientIp,
  httpRequest,
  identity,
  openOk,
  openSocket,
  proveViewer,
  registerHost,
  startNtfy,
  startRelay,
  viewerIdentity,
  viewerJoin,
} from './m3-support.mjs';

// The shared instance holds an unapproved navigation this long instead of 30 s, so a dropped
// navigation does not take a whole case timeout.
const GATE_HOLD_MS = 4_000;

const crateRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

let relay = null;
let ntfy = null;
let persistDir = '';

before(async () => {
  persistDir = mkdtempSync(path.join(tmpdir(), 'bp-relay-m3-'));
  ntfy = await startNtfy();
  relay = await startRelay({ persistDir, ntfy, gateHoldMs: GATE_HOLD_MS });
});

after(async () => {
  if (relay) await relay.stop();
  if (ntfy) await ntfy.stop();
  relay = null;
  ntfy = null;
  if (persistDir) rmSync(persistDir, { recursive: true, force: true });
});

// Restarts and the TURN case run on their own instance and persist directory, so the shared one is
// untouched. `options` goes to startRelay (ntfy, turn).
async function withRelay(dir, fn, options = {}) {
  const instance = await startRelay({ persistDir: dir, ...options });
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

// M3-C2 replaces the M3-A 404: with the gate wired, a top-level navigation without a cookie is
// held open with no page, and with no decision it is dropped when the hold ends: the connection
// fails with no content.
test('m3 GET / without a cookie is held, then dropped with no content', { timeout: CASE_TIMEOUT_MS }, async () => {
  const error = await heldNavigation(relay.port, { path: '/', ip: freshClientIp() }).then(
    ({ response }) => assert.fail(`expected a dropped connection, got ${response.status}`),
    (err) => err,
  );
  assert.ok(error.ms >= GATE_HOLD_MS - 500, `held for ${error.ms} ms`);
});

test(
  'm3 a text hello sent to /ws closes the socket with 1008 and an empty reason',
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
  await proveViewer(viewer, identity(203), { origin: relay.origin });
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

test('m3 admitted viewer is announced to the host as peer-joined with its media epoch', { timeout: CASE_TIMEOUT_MS }, async () => {
  const host = identity(90);
  const { socket: hostSocket } = await registerHost(relay.port, host);
  const { ok } = await viewerJoin(relay.port, host.id);
  const joined = await hostSocket.nextJson('peer-joined', 10_000);
  assert.deepEqual(joined, { type: 'peer-joined', mediaEpoch: ok.mediaEpoch, pairing: false });
});

// Browser origin: a browser Origin needs a valid gate cookie. Without one the gate refuses: a wrong
// origin is 404, and the public origin is accepted then closed 1008.

test('m3 a wrong Origin on /ws is refused with 404', { timeout: CASE_TIMEOUT_MS }, async () => {
  const { status, socket } = await openSocket(relay.port, `/ws?pc=${identity(11).id}`, {
    origin: 'https://evil.test',
  });
  assert.equal(status, 404);
  assert.equal(socket, null);
});

test('m3 an Origin without a valid gate cookie is accepted then closed 1008 with an empty reason', { timeout: CASE_TIMEOUT_MS }, async () => {
  // No cookie, then a cookie the gate does not hold. The upgrade succeeds so the browser sees a 1008.
  for (const cookie of [undefined, '__Host-gate=not-a-cookie']) {
    const { status, socket } = await openSocket(relay.port, `/ws?pc=${identity(44).id}`, {
      origin: relay.origin,
      cookie,
    });
    assert.equal(status, 101);
    assert.deepEqual(await socket.expectClose(10_000), { code: 1008, reason: '' });
  }
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
  await proveViewer(viewer, stranger, { origin: relay.origin, pair: admit });
  const ok = await viewer.nextJson('hello-ok', 10_000);
  assert.equal(ok.pairing, true);
  assert.equal(ok.ticket, undefined);
  // The host hears of the admission first, so its peer-joined comes before the auth-hello.
  const joined = await hostSocket.nextJson('peer-joined', 10_000);
  assert.equal(joined.pairing, true);

  viewer.sendJson({ type: 'offer', sdp: 'v=0' });
  viewer.sendJson(authHello(stranger));
  const first = JSON.parse(await hostSocket.nextText(10_000));
  assert.equal(first.type, 'auth-hello', 'the offer must not reach the host');

  hostSocket.sendJson(authHost(host));
  const back = await viewer.nextJson('auth-host', 10_000);
  assert.equal(back.type, 'auth-host');
});

test('m3 the old media and file role names are refused with invalid hello', { timeout: CASE_TIMEOUT_MS }, async () => {
  const host = identity(72);
  await registerHost(relay.port, host);
  const { socket: viewer, ok } = await viewerJoin(relay.port, host.id);
  for (const [target, role] of [['/media-ws', 'media-client'], ['/file-ws', 'file-client']]) {
    const socket = await openOk(relay.port, `${target}?pc=${host.id}&epoch=${ok.mediaEpoch}`);
    socket.sendJson({ type: 'hello', role, ticket: ok.ticket });
    assert.deepEqual(await socket.expectClose(10_000), { code: 1008, reason: 'invalid hello' });
  }
  viewer.close();
});

test('m3 a viewer offer never reaches an admitted host, and the ctl after it does', { timeout: CASE_TIMEOUT_MS }, async () => {
  const host = identity(73);
  const { socket: hostSocket } = await registerHost(relay.port, host);
  const { socket: viewer } = await viewerJoin(relay.port, host.id);
  await hostSocket.nextJson('peer-joined', 10_000);

  viewer.sendJson({ type: 'offer', sdp: 'v=0' });
  viewer.sendJson({ type: 'ctl', kind: 'input', seq: 7, data: '' });
  const seen = [];
  for (;;) {
    const msg = JSON.parse(await hostSocket.nextText(10_000));
    seen.push(msg.type);
    if (msg.type === 'ctl') {
      assert.equal(msg.seq, 7);
      break;
    }
  }
  assert.ok(!seen.includes('offer'), `the offer must not reach the host: ${seen.join(', ')}`);
  viewer.close();
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
  viewerMedia.sendJson({ type: 'hello', role: 'media-viewer', ticket: bogus });
  assert.deepEqual(await viewerMedia.expectClose(10_000), { code: 1008, reason: '' });
  viewer.close();
});

test('m3 an admitted viewer media ticket opens a working media socket', { timeout: CASE_TIMEOUT_MS }, async () => {
  const host = identity(48);
  await registerHost(relay.port, host);
  const { ok } = await viewerJoin(relay.port, host.id);
  const media = await openOk(relay.port, `/media-ws?pc=${host.id}&epoch=${ok.mediaEpoch}`);
  media.sendJson({ type: 'hello', role: 'media-viewer', ticket: ok.ticket });
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
  const viewerMedia = await dataSocket(relay.port, '/media-ws', host.id, epoch, 'media-viewer', viewerOk.ticket);

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
  const fileClient = await dataSocket(relay.port, '/file-ws', host.id, epoch, 'file-viewer', viewerOk.ticket);

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
  const staleMedia = await dataSocket(relay.port, '/media-ws', host.id, epoch1, 'media-viewer', first.ok.ticket);

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

// M3-C2 HTTP surfaces. Native: e2e_gate_and_data.rs, e2e_m2.rs and e2e_registration.rs; the static
// and healthz rules come from relay-rs/server/src/assets.rs and app.rs.

test('m3 a held viewer opens one ntfy approval naming only the client prefix, and a deny drops it', { timeout: CASE_TIMEOUT_MS }, async () => {
  const ip = freshClientIp();
  const prefix = `${ip.split('.').slice(0, 3).join('.')}.0/24`;
  const before = ntfy.count();
  const held = heldNavigation(relay.port, { path: '/', ip });
  held.catch(() => {});
  const publish = await ntfy.waitFor(
    before,
    (p) => p.body?.message === `Browser navigation awaiting approval from client prefix ${prefix}`,
  );
  assert.equal(publish.headers.authorization, `Bearer ${NTFY_TOKEN}`);
  assert.equal(publish.body.topic, NTFY_TOPIC);
  assert.equal(publish.body.actions[0].label, 'Approve');
  assert.equal(publish.body.actions[1].label, 'Deny');
  const sent = JSON.stringify(publish.body);
  assert.ok(!sent.includes(ip), 'ntfy names the prefix, not the client address');

  // A second navigation from the prefix joins the same approval: no second publish.
  const joined = heldNavigation(relay.port, { path: '/', ip });
  joined.catch(() => {});
  const deny = publish.body.actions[1].headers.Authorization.replace(/^Bearer /, '');
  await decide(relay.port, deny, 'deny', ip);
  for (const pending of [held, joined]) {
    const error = await pending.then(
      ({ response }) => assert.fail(`expected a dropped connection, got ${response.status}`),
      (err) => err,
    );
    assert.ok(error.ms < GATE_HOLD_MS, `the deny ends the hold early (${error.ms} ms)`);
  }
  assert.equal(ntfy.count() - before, 1, 'the joined navigation opened no second approval');
});

test('m3 an approval after the hold is claimed once by the next navigation', { timeout: CASE_TIMEOUT_MS }, async () => {
  const ip = freshClientIp();
  const before = ntfy.count();
  const first = heldNavigation(relay.port, { path: '/', ip });
  first.catch(() => {});
  const prefix = `${ip.split('.').slice(0, 3).join('.')}.0/24`;
  const publish = await ntfy.waitFor(
    before,
    (p) => p.body?.message === `Browser navigation awaiting approval from client prefix ${prefix}`,
  );
  await first.then(
    ({ response }) => assert.fail(`expected a dropped connection, got ${response.status}`),
    () => {},
  );
  const approve = publish.body.actions[0].headers.Authorization.replace(/^Bearer /, '');
  await decide(relay.port, approve, 'approve', ip);
  const { response, ms } = await heldNavigation(relay.port, { path: '/?a=1', ip });
  assert.equal(response.status, 302);
  assert.ok(ms < GATE_HOLD_MS, 'the claim is answered at once');
  assert.equal(response.headers.location, `${relay.origin}/?a=1`);
  assert.ok(gateCookieOf(response), 'the reload claims the cookie');
  await heldNavigation(relay.port, { path: '/', ip }).then(
    ({ response: again }) => assert.fail(`the approval was claimed twice (${again.status})`),
    () => {},
  );
});

test('m3 approval grants the gate cookie, and the page is served with it', { timeout: CASE_TIMEOUT_MS }, async () => {
  const cookie = await approveBrowser(relay, ntfy);
  assert.match(cookie, /^__Host-gate=/);
  const page = await httpRequest(relay.port, { path: '/', headers: { Cookie: cookie } });
  assert.equal(page.status, 200);
  assert.match(page.body.toString('utf8'), /^<!doctype html>/i);

  const bare = await httpRequest(relay.port, { path: '/', headers: { 'Sec-Fetch-Dest': 'empty' } });
  assert.equal(bare.status, 403, 'a subresource without the cookie gets 403 and is not held');
});

test('m3 a WebSocket with the gate cookie gets hello-ok with a media epoch', { timeout: CASE_TIMEOUT_MS }, async () => {
  const host = identity(80);
  await registerHost(relay.port, host);
  const cookie = await approveBrowser(relay, ntfy);
  const { ok } = await browserJoin(relay.port, host.id, cookie);
  assert.equal(ok.type, 'hello-ok');
  assert.equal(ok.mediaEpoch.length, 32, 'the epoch is 16 bytes in hex');
  assert.ok(typeof ok.ticket === 'string' && ok.ticket.length > 0);
  assert.equal(ok.iceServers.length, 1, 'STUN only without TURN');
});

test('m3 a browser viewer with a gate cookie is admitted, and its media ticket opens a media socket', { timeout: CASE_TIMEOUT_MS }, async () => {
  const host = identity(85);
  await registerHost(relay.port, host);
  const cookie = await approveBrowser(relay, ntfy);
  const { ok } = await browserJoin(relay.port, host.id, cookie);
  const media = await openOk(relay.port, `/media-ws?pc=${host.id}&epoch=${ok.mediaEpoch}`);
  media.sendJson({ type: 'hello', role: 'media-viewer', ticket: ok.ticket });
  assert.equal((await media.nextJson('hello-ok', 10_000)).type, 'hello-ok');
  media.close();
});

test('m3 POST /__gate/forget needs an Origin, and then drops the gate cookie', { timeout: CASE_TIMEOUT_MS }, async () => {
  const cookie = await approveBrowser(relay, ntfy);
  const bare = await httpRequest(relay.port, { method: 'POST', path: '/__gate/forget', headers: { Cookie: cookie } });
  assert.equal(bare.status, 404, 'forget without an Origin is refused');

  const forget = await httpRequest(relay.port, {
    method: 'POST',
    path: '/__gate/forget',
    headers: { Cookie: cookie, Origin: relay.origin },
  });
  assert.equal(forget.status, 204);
  assert.match(forget.headers['set-cookie'].join(';'), /__Host-gate=; Max-Age=0/);

  const after = await httpRequest(relay.port, { path: '/', headers: { Cookie: cookie, 'Sec-Fetch-Dest': 'empty' } });
  assert.equal(after.status, 403, 'the dropped cookie no longer opens the site');
});

test('m3 an unknown /__gate path is 404, with or without a cookie', { timeout: CASE_TIMEOUT_MS }, async () => {
  const cookie = await approveBrowser(relay, ntfy);
  for (const headers of [{}, { Cookie: cookie }]) {
    const res = await httpRequest(relay.port, { path: '/__gate/x', headers: { 'Sec-Fetch-Dest': 'document', ...headers } });
    assert.equal(res.status, 404);
  }
  const wrongMethod = await httpRequest(relay.port, { path: '/__gate/decision', headers: { Cookie: cookie } });
  assert.equal(wrongMethod.status, 404, 'the decision route takes POST only');
});

// Admin: list and delete (e2e_registration.rs), and the listing's count and no viewer IDs (e2e_m2.rs).

function adminGet(port, bearer) {
  return httpRequest(port, {
    path: '/__gate/manage/pcs',
    headers: bearer === undefined ? {} : { Authorization: `Bearer ${bearer}` },
  });
}

test('m3 admin list and delete close the host, and a wrong bearer is 404', { timeout: CASE_TIMEOUT_MS }, async () => {
  const host = identity(81);
  const { socket: hostSocket } = await registerHost(relay.port, host);
  assert.equal((await adminGet(relay.port, undefined)).status, 404);
  assert.equal((await adminGet(relay.port, 'wrong')).status, 404);

  const listed = await adminGet(relay.port, ADMIN);
  assert.equal(listed.status, 200);
  const row = JSON.parse(listed.body.toString('utf8')).find((r) => r.id === host.id);
  assert.ok(row, 'the registered PC is listed');
  assert.equal(row.online, true);

  const deleted = await httpRequest(relay.port, {
    method: 'DELETE',
    path: `/__gate/manage/pcs/${host.id}`,
    headers: { Authorization: `Bearer ${ADMIN}` },
  });
  assert.equal(deleted.status, 204);
  assert.deepEqual(await hostSocket.expectClose(10_000), { code: 1008, reason: 'peer removed' });

  const after = JSON.parse((await adminGet(relay.port, ADMIN)).body.toString('utf8'));
  assert.ok(after.every((r) => r.id !== host.id), 'the deleted PC is gone from the listing');
  const again = await httpRequest(relay.port, {
    method: 'DELETE',
    path: `/__gate/manage/pcs/${host.id}`,
    headers: { Authorization: `Bearer ${ADMIN}` },
  });
  assert.equal(again.status, 404);
});

test('m3 the admin listing shows the admitted count and no viewer IDs', { timeout: CASE_TIMEOUT_MS }, async () => {
  const host = identity(82);
  await registerHost(relay.port, host);
  const listed = await adminGet(relay.port, ADMIN);
  const row = JSON.parse(listed.body.toString('utf8')).find((r) => r.id === host.id);
  assert.equal(row.admitted, 1);
  assert.ok(!listed.body.toString('utf8').includes(viewerIdentity().id), 'the listing has counts, not viewer IDs');
});

test('m3 an unknown admin route is 404 even with the admin bearer', { timeout: CASE_TIMEOUT_MS }, async () => {
  const res = await httpRequest(relay.port, {
    path: '/__gate/manage/nope',
    headers: { Authorization: `Bearer ${ADMIN}` },
  });
  assert.equal(res.status, 404);
});

// healthz (e2e_healthz_loopback_200_bad_host_404_head_empty). Cloudflare has no loopback, so the
// admin bearer stands in for it. The request carries no CF-Connecting-IP, as the probe does.

test('m3 healthz answers ok with the admin bearer, and HEAD has no body', { timeout: CASE_TIMEOUT_MS }, async () => {
  const headers = { Authorization: `Bearer ${ADMIN}` };
  const get = await httpRequest(relay.port, { path: '/healthz', headers, ip: null });
  assert.equal(get.status, 200);
  assert.equal(get.body.toString('utf8'), 'ok');
  const head = await httpRequest(relay.port, { method: 'HEAD', path: '/healthz', headers, ip: null });
  assert.equal(head.status, 200);
  assert.equal(head.body.length, 0);
});

test('m3 healthz without the admin bearer, or with a foreign Host, is 404', { timeout: CASE_TIMEOUT_MS }, async () => {
  const none = await httpRequest(relay.port, { path: '/healthz', ip: null });
  assert.equal(none.status, 404);
  const wrong = await httpRequest(relay.port, { path: '/healthz', ip: null, headers: { Authorization: 'Bearer wrong' } });
  assert.equal(wrong.status, 404);
  const badHost = await httpRequest(relay.port, {
    path: '/healthz',
    ip: null,
    headers: { Authorization: `Bearer ${ADMIN}`, Host: 'evil.test' },
  });
  assert.equal(badHost.status, 404);
});

// Static assets (assets.rs): the gate runs first, then the JS Worker's public directory is served
// through the ASSETS binding. Traversal, dotfiles and backslashes are refused before the fetch.

test('m3 static files are served with the cookie, and HEAD has no body', { timeout: CASE_TIMEOUT_MS }, async () => {
  const cookie = await approveBrowser(relay, ntfy);
  const index = await httpRequest(relay.port, { path: '/', headers: { Cookie: cookie } });
  assert.equal(index.status, 200);
  assert.match(index.headers['content-type'] ?? '', /text\/html/);
  const css = await httpRequest(relay.port, { path: '/bubbleplay.css', headers: { Cookie: cookie } });
  assert.equal(css.status, 200);
  const head = await httpRequest(relay.port, { method: 'HEAD', path: '/', headers: { Cookie: cookie } });
  assert.equal(head.status, 200);
  assert.equal(head.body.length, 0);
});

test('m3 static traversal, dotfile and backslash paths are 404, and POST is 404', { timeout: CASE_TIMEOUT_MS }, async () => {
  const cookie = await approveBrowser(relay, ntfy);
  for (const target of ['/..%2f..%2fwrangler.toml', '/%2e%2e%2fwrangler.toml', '/.env', '/.git/config', '/a%5cb']) {
    const res = await httpRequest(relay.port, { path: target, headers: { Cookie: cookie } });
    assert.equal(res.status, 404, target);
  }
  const post = await httpRequest(relay.port, { method: 'POST', path: '/', headers: { Cookie: cookie } });
  assert.equal(post.status, 404);
});

// /pair is the browser page too (native: e2e_pair_* in e2e_gate_and_data.rs). The pairing code
// rides in the fragment, which the page reads itself, so the Worker only maps the path.

test('m3 GET /pair without a cookie is held and dropped like /', { timeout: CASE_TIMEOUT_MS }, async () => {
  const error = await heldNavigation(relay.port, { path: '/pair', ip: freshClientIp() }).then(
    ({ response }) => assert.fail(`expected a dropped connection, got ${response.status}`),
    (err) => err,
  );
  assert.ok(error.ms >= GATE_HOLD_MS - 500, `held for ${error.ms} ms`);
});

test('m3 GET and HEAD /pair with a cookie serve index.html, as / does', { timeout: CASE_TIMEOUT_MS }, async () => {
  const cookie = await approveBrowser(relay, ntfy);
  const root = await httpRequest(relay.port, { path: '/', headers: { Cookie: cookie } });
  const pair = await httpRequest(relay.port, { path: '/pair', headers: { Cookie: cookie } });
  assert.equal(pair.status, 200);
  assert.match(pair.headers['content-type'] ?? '', /text\/html/);
  assert.deepEqual(pair.body, root.body);
  const head = await httpRequest(relay.port, { method: 'HEAD', path: '/pair', headers: { Cookie: cookie } });
  assert.equal(head.status, 200);
  assert.equal(head.body.length, 0);
});

test('m3 /pair/x with a cookie is still 404, as before /pair was served', { timeout: CASE_TIMEOUT_MS }, async () => {
  const cookie = await approveBrowser(relay, ntfy);
  const below = await httpRequest(relay.port, { path: '/pair/x', headers: { Cookie: cookie } });
  assert.equal(below.status, 404);
});

// Inert stub classes (src/stubs.rs) exist only so an in-place deploy keeps the JS relay's classes.
// Nothing routes to them: the local config binds only RELAY, and the routes keep their answers.

test('m3 the local config binds only RELAY, so no request can reach a stub class', () => {
  const toml = readFileSync(path.join(crateRoot, 'wrangler.toml'), 'utf8');
  const classes = [...toml.matchAll(/class_name\s*=\s*"([^"]+)"/g)].map((match) => match[1]);
  assert.deepEqual(classes, ['Relay']);
});

test('m3 stub-named paths keep the relay 404 with an empty body, and / still serves the page', { timeout: CASE_TIMEOUT_MS }, async () => {
  const cookie = await approveBrowser(relay, ntfy);
  for (const target of ['/room', '/media-room', '/approval-gate', '/agent-room']) {
    const res = await httpRequest(relay.port, { path: target, headers: { Cookie: cookie } });
    assert.equal(res.status, 404, target);
    assert.equal(res.body.length, 0, target);
  }
  const index = await httpRequest(relay.port, { path: '/', headers: { Cookie: cookie } });
  assert.equal(index.status, 200);
  assert.match(index.headers['content-type'] ?? '', /text\/html/);
});

// TURN (e2e_ws_with_turn_adds_credentialed_ice_server): a TURN instance, so the TURN config is set.

test('m3 a WebSocket hello with turn gets a credentialed TURN ICE server', { timeout: RESTART_TIMEOUT_MS }, async () => {
  const dir = mkdtempSync(path.join(tmpdir(), 'bp-relay-m3-turn-'));
  try {
    await withRelay(
      dir,
      async (instance) => {
        const host = identity(83);
        await registerHost(instance.port, host);
        const cookie = await approveBrowser(instance, ntfy);
        const on = await browserJoin(instance.port, host.id, cookie, { turn: true });
        assert.equal(on.ok.iceServers.length, 2, 'STUN plus TURN when the viewer asks for TURN');
        assert.equal(typeof on.ok.iceServers[1].credential, 'string');
        const off = await browserJoin(instance.port, host.id, cookie, { turn: false });
        assert.equal(off.ok.iceServers.length, 1, 'STUN only when the viewer does not ask');
      },
      { ntfy, turn: true },
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// Restart with the gate cookie (e2e_restart_keeps_pcs_and_gate_cookie).

test('m3 a restart keeps the gate cookie', { timeout: RESTART_TIMEOUT_MS }, async () => {
  const dir = mkdtempSync(path.join(tmpdir(), 'bp-relay-m3-cookie-'));
  const host = identity(84);
  let cookie = '';
  try {
    await withRelay(
      dir,
      async (instance) => {
        await registerHost(instance.port, host);
        cookie = await approveBrowser(instance, ntfy);
      },
      { ntfy },
    );
    await withRelay(
      dir,
      async (instance) => {
        const page = await httpRequest(instance.port, { path: '/', headers: { Cookie: cookie } });
        assert.equal(page.status, 200, 'the cookie is still valid after the restart');
        assert.match(page.body.toString('utf8'), /^<!doctype html>/i);
      },
      { ntfy },
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
