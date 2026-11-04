import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";
import { FileTransferClient } from "../public/file-client.js";
import { b64urlDecode, pcListUrl, registerProof, relayOrigin, viewerHello } from "../public/dk-relay.js";
// receiver.js now reports to monitor-previews.js; these vm contexts do not exercise it.
const noMonitorPreviews = { reset() {}, onFirstFrame() {}, onMonitorList() {}, onMessage() {} };

const source = readFileSync(new URL("../public/receiver.js", import.meta.url), "utf8").replaceAll("\r\n", "\n");
const index = readFileSync(new URL("../public/index.html", import.meta.url), "utf8");
const epoch = "8f7c62d194a04e3bb9e18cd604cf5a20";
const ticket = "test-only-secret-socket-ticket";
const contract = JSON.parse(readFileSync(new URL("../../fixtures/device-keys-contract.json", import.meta.url)));
const VECTOR = contract.relayProof.vectors.find(vector => vector.name === "viewer");

async function fixtureIdentity() {
  const entry = contract.ids.find(item => item.public === VECTOR.public);
  const pkcs8 = Buffer.concat([Buffer.from("302e020100300506032b657004220420", "hex"), Buffer.from(entry.seedHex, "hex")]);
  const privateKey = await crypto.subtle.importKey("pkcs8", pkcs8, { name: "Ed25519" }, false, ["sign"]);
  return { id: entry.id, publicRaw: b64urlDecode(entry.public, 32), privateKey };
}

function receiverFunction(name) {
  const match = source.match(new RegExp(`(?:async )?function ${name}\\([^]*?\\n\\}`));
  assert.ok(match, `receiver function ${name}`);
  return match[0];
}

function urlHarness(overrides = {}) {
  const context = vm.createContext({ monitorPreviews: noMonitorPreviews, URLSearchParams, proto: "wss:", location: { host: "relay.test" },
    sessionPc: "requested-pc", mediaEpoch: epoch, signalingPc: "resolved-pc", signalingTicket: ticket,
    authenticatedSession: { mediaReady: true }, ...overrides });
  vm.runInContext(["signalingSocketUrl", "mediaSocketUrl", "fileSocketUrl"].map(receiverFunction).join("\n"), context);
  return context;
}

test("socket URLs select the requested or admitted PC without exposing credentials", () => {
  const h = urlHarness();
  for (const pc of ["abcd-oiL0", "one two/+?&=\u96ea"]) {
    h.sessionPc = pc;
    assert.equal(h.signalingSocketUrl(), `wss://relay.test/ws?pc=${encodeURIComponent(pc)}`);
    h.signalingPc = pc;
    for (const kind of ["media", "file"]) {
      const url = h[`${kind}SocketUrl`]();
      assert.equal(url, `wss://relay.test/${kind}-ws?${new URLSearchParams({ epoch, pc })}`);
      assert.ok(!url.includes(ticket));
    }
  }
  for (const overrides of [{ mediaEpoch: null }, { signalingPc: null }, { signalingTicket: null },
    { authenticatedSession: null }, { authenticatedSession: { mediaReady: false } }]) {
    assert.equal(urlHarness(overrides).fileSocketUrl(), null);
  }
});

// Exercise receiver socket/callback wiring; dk-session tests own the cryptographic protocol.
function signalHarness(identity) {
  const output = [], endpoint = [], sockets = [], gateCalls = [], sessions = [], logged = [];
  class Socket {
    static OPEN = 1; static CONNECTING = 0;
    constructor(url) { this.url = url; this.readyState = 0; sockets.push(this); }
    send(text) { output.push(JSON.parse(text)); }
    close() { this.readyState = 3; }
  }
  const context = vm.createContext({ monitorPreviews: noMonitorPreviews, WebSocket: Socket, URL, URLSearchParams,
    performance: { now: () => 1 }, navigator: { onLine: true },
    ws: null, signalingGeneration: 0, signalingPc: null, signalingTicket: null,
    authenticatedSession: null, sessionPinnedPublic: new Uint8Array(32).fill(7),
    terminalSignalingFailure: false, reconnectTimer: null, signalingRegistered: false,
    hudState: {}, hud: { classList: { add() {} } },
    params: new URLSearchParams(), wantsTurn: () => false,
    sessionIdentity: identity, sessionPc: "pc-one", sessionTouched: false,
    touchPc: async id => { output.push(["touch", id]); },
    sessionGate: { hide: () => gateCalls.push(["hide"]), showRefusal: options => gateCalls.push(["refusal", options]) },
    viewerHello, registerProof, relayOrigin, pcListUrl,
    location: { href: `${VECTOR.origin}/?pc=pc-one`, assign: url => gateCalls.push(["assign", url]) },
    releaseInput: () => output.push(["release-input"]), cleanupPeer() {}, clearFirstFrameWatchdog() {},
    setFileTransferAvailable() {}, clearTimeout() {},
    signalingSocketUrl: () => "wss://relay.test/ws?pc=pc-one",
    diagnosticAttemptId: "attempt", clientBuild: "test", requestedTransport: "auto",
    codecSession: { codec: "h264" },
    BPLog: { log: (...args) => logged.push(args), setContext() {} },
    debugTrace: (...args) => output.push(args), recordSignalingError: (...args) => output.push(args),
    shouldReconnectAfterClose: () => false, restartPeer() {},
    setTimeout: () => 1, validateIceServers: () => [], renderAbout() {}, noteFileEpoch() {},
    sendStreamMode: reason => endpoint.push(["stream-mode", reason]), wssSelected: false,
    startPeer: generation => endpoint.push(["start-peer", generation]), peerGeneration: 0,
    requestWssMediaStart: () => endpoint.push(["start-wss"]),
    handleEndpointSignal: (_, generation, message) => endpoint.push(["received", generation, message]),
    handleCursorState: (bytes, route) => endpoint.push(["cursor", Array.from(bytes), route]),
    inputReady: false, pendingRtcTracks: [], dc: null, motionDc: null, onInputReady() {},
    invalidatePendingMonitorRequest() {}, lastRenderedDueMs: null, usingWssMedia: false,
    wssV4: { socketOpen: false, lastSeq: 0 }, wssPresentedSequence: 0,
    wssMediaReceivedPackets: 0, wssRecoveryState: "idle",
    createSignalingRecycleDiagnostic: value => value,
    createViewerSession(options) {
      const session = { options, controlReady: false, mediaReady: false, closed: false, received: [],
        async start() { options.send({ type: "auth-hello", test: true }); },
        sendControl(message) { if (!this.controlReady || this.closed) return false; endpoint.push(message); return true; },
        async receive(message) { this.received.push(message); },
        close() { this.closed = true; },
      };
      sessions.push(session);
      return session;
    },
  });
  vm.runInContext(["sendRelay", "sendControl", "viewerLog", "handleSignal", "reconnectSignaling", "recycleSignaling",
    "showNotAdmitted", "endAuthenticatedSession", "openAuthenticatedMedia", "updateAuthenticatedInputReady"]
    .map(receiverFunction).join("\n"), context);
  context.reconnectSignaling("startup");
  const socket = sockets[0];
  socket.readyState = Socket.OPEN; socket.bufferedAmount = 0;
  const relay = message => context.handleSignal(socket, 1, { data: JSON.stringify(message) });
  const helloOk = fields => relay({ type: "hello-ok", mediaEpoch: epoch, ...fields });
  const authenticate = () => { const session = sessions.at(-1); session.controlReady = true; session.options.onAuthenticated(); };
  const mediaReady = () => { const session = sessions.at(-1); session.mediaReady = true; session.options.onMediaReady(); };
  const close = async event => { socket.readyState = 3; socket.onclose(event); await Promise.resolve(); await Promise.resolve(); };
  return { context, socket, sockets, output, endpoint, logged, gateCalls, sessions, helloOk, relay, authenticate, mediaReady, close };
}

test("relay proof and admission start endpoint authentication without opening media or plaintext controls", async () => {
  const identity = await fixtureIdentity();
  const h = signalHarness(identity);
  h.socket.onopen();
  const hello = h.output.find(message => message.type === "hello");
  assert.deepEqual(hello, { type: "hello", role: "viewer", id: VECTOR.id, public: VECTOR.public });
  assert.deepEqual(Object.keys(hello), ["type", "role", "id", "public"]);
  assert.equal(h.context.sendControl({ type: "first-frame" }), false);
  await h.relay({ type: "register-challenge", nonce: VECTOR.nonce });
  assert.deepEqual(h.output.filter(message => message.type === "register-proof"),
    [{ type: "register-proof", signature: VECTOR.signature }]);
  await h.helloOk({ pc: "pc-one", ticket });
  const session = h.sessions[0];
  assert.equal(session.options.identity, identity);
  assert.equal(session.options.pcId, "pc-one");
  assert.equal(session.options.pinnedPublic, h.context.sessionPinnedPublic);
  assert.equal(session.options.epoch, epoch);
  assert.equal(h.context.mediaEpoch, epoch, "the relay epoch is an opaque string and must retain its exact bytes");
  assert.equal(session.options.relayOrigin, VECTOR.origin);
  assert.equal(session.options.transport, "webrtc");
  assert.deepEqual(h.gateCalls, []);
  assert.deepEqual(h.endpoint, []);
  assert.equal(h.context.sendControl({ type: "first-frame" }), false);
  h.authenticate();
  assert.ok(h.endpoint.some(entry => entry[0] === "start-peer"));
  assert.deepEqual(h.gateCalls, [], "control authentication alone cannot expose media");
  assert.equal(h.context.sendControl({ type: "first-frame" }), true);
  assert.equal(h.output.some(message => message.type === "first-frame"), false);
  h.mediaReady();
  assert.deepEqual(h.gateCalls, [["hide"]]);
  assert.ok(h.output.some(entry => entry[0] === "touch" && entry[1] === "pc-one"));
  await h.relay({ type: "register-challenge", nonce: VECTOR.nonce });
  assert.equal(h.output.filter(message => message.type === "register-proof").length, 1);
});

test("receiver sends only authentication envelopes to the session and ignores plaintext endpoint frames", async () => {
  const h = signalHarness(await fixtureIdentity());
  h.context.wssSelected = true;
  await h.helloOk({ pc: "pc-one", ticket });
  assert.equal(h.sessions[0].options.transport, "wss");
  for (const type of ["auth-host", "auth-bind", "ctl"]) await h.relay({ type, test: true });
  assert.deepEqual(h.sessions[0].received.map(message => message.type), ["auth-host", "auth-bind", "ctl"]);
  for (const type of ["answer", "host-capabilities", "desktop-state"]) await h.relay({ type, test: true });
  assert.deepEqual(h.endpoint, []);
  h.authenticate();
  assert.ok(h.endpoint.some(entry => entry[0] === "start-wss"));
  await h.sessions[0].options.onMessage({ type: "host-capabilities" });
  assert.ok(h.endpoint.some(entry => entry[0] === "received" && entry[2].type === "host-capabilities"));
});

test("relay epochs reach authentication and media URLs without normalization or numeric conversion", async () => {
  for (const value of [epoch, "42", "00042", "opaque epoch/+A"]) {
    const h = signalHarness(await fixtureIdentity());
    await h.helloOk({ pc: "pc-one", ticket, mediaEpoch: value });
    assert.equal(h.context.mediaEpoch, value);
    assert.equal(h.sessions[0].options.epoch, value);
    const urls = urlHarness({ mediaEpoch: h.context.mediaEpoch });
    for (const kind of ["media", "file"]) {
      assert.equal(new URL(urls[`${kind}SocketUrl`]()).searchParams.get("epoch"), value);
    }
  }
});

test("receiver waits for encrypted messages in wire order before handling socket closure", async () => {
  const h = signalHarness(await fixtureIdentity());
  await h.helloOk({ pc: "pc-one", ticket });
  const events = [];
  let finishFirst;
  const first = new Promise(resolve => { finishFirst = resolve; });
  h.sessions[0].receive = async message => {
    events.push(message.seq);
    if (message.seq === 1) await first;
  };
  h.socket.onmessage({ data: JSON.stringify({ type: "ctl", seq: 1 }) });
  h.socket.onmessage({ data: JSON.stringify({ type: "ctl", seq: 2 }) });
  h.socket.onclose({ code: 1008, reason: "" });
  await Promise.resolve();
  assert.deepEqual(events, [1]);
  assert.equal(h.sessions[0].closed, false);
  finishFirst();
  // Each event crosses both the receiver queue and its async handler.
  for (let i = 0; i < 12; i++) await Promise.resolve();
  assert.deepEqual(events, [1, 2]);
  assert.equal(h.sessions[0].closed, true);
  assert.equal(h.context.signalingTicket, null);
});

test("1008 before admission shows Not admitted and never reconnects", async () => {
  const h = signalHarness(await fixtureIdentity());
  h.socket.onopen();
  await h.relay({ type: "register-challenge", nonce: VECTOR.nonce });
  await h.close({ code: 1008, reason: "" });
  const refusal = h.gateCalls.find(call => call[0] === "refusal")?.[1];
  assert.equal(refusal.title, "Not admitted");
  assert.equal(refusal.body, "The relay did not let this browser through to pc-one. If it was unpaired on the PC, pair again.");
  refusal.onAction();
  assert.deepEqual(h.gateCalls.at(-1), ["assign", `${VECTOR.origin}/`]);
  assert.equal(h.context.terminalSignalingFailure, true);
  h.context.reconnectSignaling("ws-closed");
  assert.equal(h.sockets.length, 1);
});

test("1008 after admission closes authentication and releases input without showing relay refusal", async () => {
  const h = signalHarness(await fixtureIdentity());
  await h.helloOk({ pc: "pc-one", ticket });
  await h.close({ code: 1008, reason: "" });
  assert.equal(h.sessions[0].closed, true);
  assert.ok(h.output.some(entry => entry[0] === "release-input"));
  assert.equal(h.gateCalls.some(call => call[0] === "refusal"), false);
  assert.equal(h.context.terminalSignalingFailure, true);
  h.context.reconnectSignaling("ws-closed");
  assert.equal(h.sockets.length, 1);
});

test("duplicate admission and stale sockets cannot replace credentials or use old session callbacks", async () => {
  const h = signalHarness(await fixtureIdentity());
  await h.helloOk({ pc: "pc-one", ticket });
  await h.context.handleSignal({}, 0, { data: JSON.stringify({ type: "hello-ok", pc: "stale", ticket: "stale" }) });
  await h.helloOk({ pc: "pc-two", ticket: "replacement" });
  assert.equal(h.context.signalingPc, "pc-one");
  assert.equal(h.context.signalingTicket, ticket);
  assert.equal(h.sessions.length, 1);
  const session = h.sessions[0];
  h.context.recycleSignaling("peer-left");
  const before = h.endpoint.length;
  session.options.onAuthenticated(); session.options.onMediaReady();
  await session.options.onMessage({ type: "desktop-state" });
  session.options.onBinary("cursor", Uint8Array.of(1));
  session.options.onFailure({ title: "old session" });
  assert.equal(h.endpoint.length, before);
  assert.deepEqual(h.gateCalls, []);
  assert.equal(session.options.send({ type: "ctl" }), false);
});

test("missing, malformed or different-PC admission credentials fail closed", async () => {
  for (const fields of [{}, { pc: "pc-one" }, { pc: "pc-one", ticket: {} },
    { pc: 42, ticket }, { pc: "other-pc", ticket }]) {
    const h = signalHarness(await fixtureIdentity());
    await h.helloOk(fields);
    assert.equal(h.sessions.length, 0);
    assert.equal(h.context.signalingPc, null);
    assert.equal(h.context.signalingTicket, null);
    assert.equal(h.context.terminalSignalingFailure, true);
    assert.equal(h.gateCalls[0][1].title, "PC not reachable");
  }
});

test("signaling close and intentional recycle discard credentials", async () => {
  for (const recycle of [false, true]) {
    const h = signalHarness(await fixtureIdentity());
    await h.helloOk({ pc: "pc-one", ticket });
    if (recycle) h.context.recycleSignaling("peer-left");
    else await h.close({ code: 1008, reason: "policy" });
    assert.equal(h.context.signalingPc, null);
    assert.equal(h.context.signalingTicket, null);
    assert.equal(h.context.authenticatedSession, null);
    assert.equal(h.sessions[0].closed, true);
    assert.ok(!JSON.stringify(h.output).includes(ticket));
  }
});

test("admission traces and authenticated client logs exclude session credentials", async () => {
  const h = signalHarness(await fixtureIdentity());
  h.socket.onopen();
  await h.helloOk({ pc: "pc-one", ticket });
  h.authenticate();
  assert.ok(!h.endpoint.some(message => message.type === "client-log"), "no log message goes to the relay or PC");
  assert.ok(h.logged.some(entry => entry[2] === "signaling-authenticated"), "the event is logged locally");
  assert.ok(h.output.some(message => message[0] === "signaling-hello-ok"));
  assert.doesNotMatch(JSON.stringify([h.output, h.endpoint, h.logged]), /test-only-secret-socket-ticket/);
  assert.doesNotMatch(JSON.stringify(h.output.filter(message => Array.isArray(message))), /pc-one/);
});

test("media hello requires authenticated media and a current ticket; obsolete sockets cannot send", () => {
  const sent = [], sockets = [];
  class Socket {
    constructor(url) { this.url = url; sockets.push(this); }
    send(text) { sent.push(text); }
  }
  const h = vm.createContext({ monitorPreviews: noMonitorPreviews, WebSocket: Socket, usingWssMedia: true, mediaEpoch: epoch,
    wssV4: {}, signalingTicket: null, authenticatedSession: null,
    mediaSocketUrl: () => `wss://relay.test/media-ws?epoch=${epoch}&pc=pc-one`,
    performance: { now: () => 1 } });
  vm.runInContext(receiverFunction("openWssV4Socket"), h);
  h.openWssV4Socket(); assert.equal(sockets.length, 0);
  h.signalingTicket = ticket;
  h.openWssV4Socket(); assert.equal(sockets.length, 0);
  h.authenticatedSession = { mediaReady: false };
  h.openWssV4Socket(); assert.equal(sockets.length, 0);
  h.authenticatedSession.mediaReady = true;
  h.openWssV4Socket(); sockets[0].onopen();
  assert.equal(sent[0], JSON.stringify({ type: "hello", role: "media-viewer", ticket }));
  h.signalingTicket = "replacement";
  sockets[0].onopen(); assert.equal(sent.length, 1);
  h.openWssV4Socket(); sockets[1].onopen();
  assert.equal(sent[1], JSON.stringify({ type: "hello", role: "media-viewer", ticket: "replacement" }));
  sockets[0].onopen(); assert.equal(sent.length, 2);
  h.authenticatedSession.mediaReady = false;
  sockets[1].onopen(); assert.equal(sent.length, 2);
  for (const socket of sockets) assert.ok(!socket.url.includes(ticket));
});

test("file hello reads the current ticket callback without exposing it in snapshots", () => {
  const sent = [];
  class Socket {
    constructor(url) { this.url = url; }
    send(text) { sent.push(text); }
    close() {}
  }
  let currentTicket = ticket;
  const client = new FileTransferClient({ socketUrl: () => "wss://relay.test/file-ws", WebSocketImpl: Socket,
    socketTicket: () => currentTicket });
  try {
    for (const value of [ticket, "replacement"]) {
      client.openSocket(); currentTicket = value; client.ws.onopen();
      assert.equal(sent.at(-1), JSON.stringify({ type: "hello", role: "file-viewer", ticket: value }));
      assert.ok(!client.ws.url.includes(value));
      assert.ok(!Object.values(client).includes(value));
      assert.ok(!JSON.stringify(client.transfers).includes(value));
      const previous = client.ws;
      client.closeSocket(); previous.onopen();
    }
  } finally { client.reset(); }
  assert.equal(sent.length, 2);
  assert.match(source, /new FileTransferClient\(\{ socketUrl: fileSocketUrl, socketTicket: \(\) => signalingTicket \}\)/);
});

test("diagnostic snapshots, reports, telemetry and HUD do not read or spread credentials", () => {
  const paths = [
    source.slice(source.indexOf('setInterval(() => {\n  if (cursorReconciler'), source.indexOf("// ---- signaling")),
    source.slice(source.indexOf("if (diagnosticsEnabled) {"), source.indexOf("let previousInboundStats")),
    receiverFunction("viewerLog"), receiverFunction("sendClientProfile"),
    receiverFunction("sendClientQos"), receiverFunction("diagnosticsReport"),
  ];
  for (const path of paths) {
    assert.ok(path.length > 0);
    assert.doesNotMatch(path, /signalingTicket|signalingPc|\.\.\.msg|\.\.\.message|\.\.\.hello/);
  }
  const bootstrap = index.slice(index.indexOf("(() => {"), index.indexOf("})();") + 5);
  assert.match(bootstrap, /window\.BPLog\?\.info\("trace"/);
  assert.doesNotMatch(bootstrap, /WebSocket|ticket|signalingPc|hello-ok|fetch\(/);
});
