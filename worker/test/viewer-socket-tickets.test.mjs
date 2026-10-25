import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";
import { FileTransferClient } from "../public/file-client.js";
import { b64urlDecode, pcListUrl, registerProof, relayOrigin, viewerHello } from "../public/dk-relay.js";

const source = readFileSync(new URL("../public/receiver.js", import.meta.url), "utf8").replaceAll("\r\n", "\n");
const index = readFileSync(new URL("../public/index.html", import.meta.url), "utf8");
const epoch = "a".repeat(32);
const ticket = "test-only-secret-socket-ticket";
const contract = JSON.parse(readFileSync(new URL("../../fixtures/device-keys-contract.json", import.meta.url)));
const VECTOR = contract.relayProof.vectors.find(vector => vector.name === "viewer");

// The fixture viewer identity (test-only seed), shaped as dk-app.js hands it to startSession.
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
  const context = vm.createContext({ URLSearchParams, proto: "wss:", location: { host: "relay.test" },
    sessionPc: null, labRun: null, labCompatibilityId: "compat-v1",
    mediaEpoch: epoch, signalingPc: null, signalingTicket: ticket, ...overrides });
  vm.runInContext(["signalingSocketUrl", "mediaSocketUrl", "fileSocketUrl"].map(receiverFunction).join("\n"), context);
  return context;
}

test("signaling passes the session PC through encodeURIComponent", () => {
  const h = urlHarness();
  assert.equal(h.signalingSocketUrl(), "wss://relay.test/ws");
  // IDs are opaque here: dk-app.js passes the stored canonical ID.
  for (const pc of ["abcd-oiL0", "one two/+?&=雪"]) {
    h.sessionPc = pc;
    assert.equal(h.signalingSocketUrl(), `wss://relay.test/ws?pc=${encodeURIComponent(pc)}`);
    h.labRun = "run /+";
    assert.equal(h.signalingSocketUrl(), `wss://relay.test/lab-ws?run=run%20%2F%2B&compat=compat-v1&pc=${encodeURIComponent(pc)}`);
    h.labRun = null;
  }
  h.labRun = "run"; h.sessionPc = null;
  assert.equal(h.signalingSocketUrl(), "wss://relay.test/lab-ws?run=run&compat=compat-v1");
});

test("media and file URLs append resolved PC after epoch, including lab routes", () => {
  const h = urlHarness({ sessionPc: "requested-pc" });
  for (const lab of [false, true]) {
    h.labRun = lab ? "run" : null;
    const tail = lab ? "&run=run&compat=compat-v1" : "";
    for (const pc of [null, "resolved-pc /+雪"]) {
      h.signalingPc = pc;
      const selector = pc ? `&${new URLSearchParams({ pc })}` : "";
      for (const kind of ["media", "file"]) {
        const url = h[`${kind}SocketUrl`]();
        assert.equal(url, `wss://relay.test/${lab ? "lab-" : ""}${kind}-ws?epoch=${epoch}${selector}${tail}`);
        assert.doesNotMatch(url, /ticket|requested-pc/);
        assert.ok(!url.includes(ticket));
      }
    }
  }
  h.mediaEpoch = null;
  assert.equal(h.fileSocketUrl(), null);
});

function signalHarness(identity) {
  const output = [];
  const sockets = [];
  const gateCalls = [];
  class Socket {
    static OPEN = 1; static CONNECTING = 0;
    constructor(url) { this.url = url; this.readyState = 0; sockets.push(this); }
    send(text) { output.push(JSON.parse(text)); }
    close() { this.readyState = 3; }
  }
  const context = vm.createContext({ WebSocket: Socket, URL, URLSearchParams,
    ArrayBuffer, performance: { now: () => 1 }, navigator: { onLine: true },
    ws: null, signalingGeneration: 0, signalingPc: null, signalingTicket: null,
    terminalSignalingFailure: false, reconnectTimer: null, signalingRegistered: false,
    hudState: {}, hud: { classList: { add() {} } }, labRun: null,
    params: new URLSearchParams(), wantsTurn: () => false,
    sessionIdentity: identity, sessionPc: "pc-one", sessionTouched: false,
    touchPc: async id => { output.push(["touch", id]); },
    sessionGate: { hide: () => gateCalls.push(["hide"]), showRefusal: options => gateCalls.push(["refusal", options]) },
    viewerHello, registerProof, relayOrigin, pcListUrl,
    location: { href: `${VECTOR.origin}/?pc=pc-one`, assign: url => gateCalls.push(["assign", url]) },
    releaseInput: () => output.push(["release-input"]),
    signalingSocketUrl: () => "wss://relay.test/ws",
    diagnosticAttemptId: "attempt", clientBuild: "test", requestedTransport: "auto",
    codecSession: { codec: "h264" }, clientLogSequence: 0,
    debugTrace: (...args) => output.push(args), recordSignalingError: (...args) => output.push(args),
    shouldReconnectAfterClose: () => false, restartPeer() {},
    setTimeout: () => 1, validateIceServers: () => [], renderAbout() {}, noteFileEpoch() {},
    sendStreamMode() {}, wssSelected: false, startPeer() {}, peerGeneration: 0,
    invalidatePendingMonitorRequest() {}, lastRenderedDueMs: null, usingWssMedia: false,
    wssV4: { socketOpen: false, lastSeq: 0 }, wssPresentedSequence: 0,
    wssMediaReceivedPackets: 0, wssRecoveryState: "idle",
    createSignalingRecycleDiagnostic: value => value,
  });
  vm.runInContext(["sendRelay", "sendControl", "sendClientLog", "handleSignal", "reconnectSignaling", "recycleSignaling", "showNotAdmitted"]
    .map(receiverFunction).join("\n"), context);
  context.reconnectSignaling("startup");
  const socket = sockets[0];
  socket.readyState = Socket.OPEN; socket.bufferedAmount = 0;
  const relay = message => context.handleSignal(socket, 1, { data: JSON.stringify(message) });
  const helloOk = fields => relay({ type: "hello-ok", mediaEpoch: epoch, ...fields });
  return { context, socket, sockets, output, gateCalls, helloOk, relay };
}

test("viewer hello has the exact keys and register-challenge gets the fixture relay proof", async () => {
  const h = signalHarness(await fixtureIdentity());
  h.socket.onopen();
  const hello = h.output.find(message => message.type === "hello");
  assert.deepEqual(hello, { type: "hello", role: "viewer", id: VECTOR.id, public: VECTOR.public });
  assert.deepEqual(Object.keys(hello), ["type", "role", "id", "public"]);
  // Nothing but relay messages go out before hello-ok.
  assert.equal(h.context.sendControl({ type: "first-frame" }), false);
  await h.relay({ type: "register-challenge", nonce: VECTOR.nonce });
  assert.deepEqual(h.output.filter(message => message.type === "register-proof"),
    [{ type: "register-proof", signature: VECTOR.signature }]);
  assert.deepEqual(h.gateCalls, []);
  await h.helloOk({ pc: "pc-one", ticket });
  assert.deepEqual(h.gateCalls, [["hide"]]);
  assert.ok(h.output.some(entry => entry[0] === "touch" && entry[1] === "pc-one"));
  assert.equal(h.context.sendControl({ type: "first-frame" }), true);
  // A challenge after admission gets no second proof.
  await h.relay({ type: "register-challenge", nonce: VECTOR.nonce });
  assert.equal(h.output.filter(message => message.type === "register-proof").length, 1);
});

test("1008 before hello-ok shows Not admitted and never reconnects", async () => {
  const h = signalHarness(await fixtureIdentity());
  h.socket.onopen();
  await h.relay({ type: "register-challenge", nonce: VECTOR.nonce });
  h.socket.onclose({ code: 1008, reason: "" });
  const refusal = h.gateCalls.find(call => call[0] === "refusal")?.[1];
  assert.equal(refusal.title, "Not admitted");
  assert.equal(refusal.body, "The relay did not let this browser through to pc-one. If it was unpaired on the PC, pair again.");
  refusal.onAction();
  assert.deepEqual(h.gateCalls.at(-1), ["assign", `${VECTOR.origin}/`]);
  assert.equal(h.context.terminalSignalingFailure, true);
  h.context.reconnectSignaling("ws-closed");
  assert.equal(h.sockets.length, 1);
});

test("1008 after hello-ok releases input, shows no refusal and does not reconnect", async () => {
  const h = signalHarness(await fixtureIdentity());
  await h.helloOk({ pc: "pc-one", ticket });
  h.socket.onclose({ code: 1008, reason: "" });
  assert.ok(h.output.some(entry => entry[0] === "release-input"));
  assert.equal(h.gateCalls.some(call => call[0] === "refusal"), false);
  assert.equal(h.context.terminalSignalingFailure, true);
  h.context.reconnectSignaling("ws-closed");
  assert.equal(h.sockets.length, 1);
});

test("hello-ok replaces credentials, omitted fields clear them, and stale sockets cannot overwrite them", async () => {
  const h = signalHarness(await fixtureIdentity());
  await h.helloOk({ pc: "pc-one", ticket });
  assert.equal(h.context.signalingPc, "pc-one");
  assert.equal(h.context.signalingTicket, ticket);
  await h.context.handleSignal({}, 0, { data: JSON.stringify({ type: "hello-ok", pc: "stale", ticket: "stale" }) });
  assert.equal(h.context.signalingTicket, ticket);
  await h.helloOk({ pc: "pc-two", ticket: "replacement" });
  assert.equal(h.context.signalingPc, "pc-two");
  assert.equal(h.context.signalingTicket, "replacement");
  await h.helloOk({});
  assert.equal(h.context.signalingPc, null);
  assert.equal(h.context.signalingTicket, null);
  await h.helloOk({ pc: 42, ticket: {} });
  assert.equal(h.context.signalingPc, null);
  assert.equal(h.context.signalingTicket, null);
});

test("signaling close and intentional recycle both discard credentials", async () => {
  for (const recycle of [false, true]) {
    const h = signalHarness(await fixtureIdentity());
    await h.helloOk({ pc: "pc-one", ticket });
    if (recycle) h.context.recycleSignaling("peer-left");
    else h.socket.onclose({ code: 1008, reason: "policy" });
    assert.equal(h.context.signalingPc, null);
    assert.equal(h.context.signalingTicket, null);
    assert.ok(!JSON.stringify(h.output).includes(ticket));
  }
});

test("signaling hello and hello-ok traces/logs never include session credentials", async () => {
  const h = signalHarness(await fixtureIdentity());
  await h.helloOk({ pc: "resolved", ticket });
  h.socket.onopen();
  const hello = h.output.find(message => message.type === "hello");
  assert.deepEqual(hello, { type: "hello", role: "viewer", id: VECTOR.id, public: VECTOR.public });
  assert.ok(h.output.some(message => message.type === "client-log"));
  assert.ok(h.output.some(message => message[0] === "signaling-hello-ok"));
  assert.doesNotMatch(JSON.stringify(h.output), /ticket|resolved/);
});

test("media hello adds only a known ticket and ignores obsolete sockets", () => {
  const sent = [];
  const sockets = [];
  class Socket {
    constructor(url) { this.url = url; sockets.push(this); }
    send(text) { sent.push(text); }
  }
  const h = vm.createContext({ WebSocket: Socket, usingWssMedia: true, mediaEpoch: epoch,
    wssV4: {}, signalingTicket: null, mediaSocketUrl: () => "wss://relay.test/media-ws",
    performance: { now: () => 1 } });
  vm.runInContext(receiverFunction("openWssV4Socket"), h);
  for (const value of [null, ticket]) {
    h.signalingTicket = value;
    h.openWssV4Socket();
    sockets.at(-1).onopen();
    assert.equal(sent.at(-1), JSON.stringify({ type: "hello", role: "media-viewer", ...(value ? { ticket: value } : {}) }));
    assert.ok(!sockets.at(-1).url.includes(ticket));
  }
  sockets[0].onopen();
  assert.equal(sent.length, 2);
});

test("file hello uses the current ticket callback without storing a ticket in snapshots", () => {
  const sent = [];
  class Socket {
    constructor(url) { this.url = url; }
    send(text) { sent.push(text); }
    close() {}
  }
  let currentTicket = null;
  for (const withCallback of [false, true]) {
    const client = new FileTransferClient({ socketUrl: () => "wss://relay.test/file-ws", WebSocketImpl: Socket,
      ...(withCallback ? { socketTicket: () => currentTicket } : {}) });
    try {
      for (const value of [null, ticket, null]) {
        currentTicket = value;
        client.openSocket();
        client.ws.onopen();
        assert.equal(sent.at(-1), JSON.stringify({ type: "hello", role: "file-viewer", ...(withCallback && value ? { ticket: value } : {}) }));
        assert.ok(!client.ws.url.includes(ticket));
        assert.ok(!Object.values(client).includes(ticket));
        assert.ok(!JSON.stringify(client.transfers).includes(ticket));
        const previous = client.ws;
        client.closeSocket(); previous.onopen();
      }
    } finally { client.reset(); }
  }
  assert.equal(sent.length, 6);
  assert.match(source, /new FileTransferClient\(\{ socketUrl: fileSocketUrl, socketTicket: \(\) => signalingTicket \}\)/);
});

test("diagnostic snapshots, reports, telemetry and HUD do not read or spread credentials", () => {
  // These are the browser output paths; client_diag lives on Android.
  const paths = [
    source.slice(source.indexOf('setInterval(() => {\n  if (cursorReconciler'), source.indexOf("// ---- signaling")),
    source.slice(source.indexOf("setInterval(() => sendClientLog"), source.indexOf("function reconnectSignaling")),
    source.slice(source.indexOf("if (diagnosticsEnabled) {"), source.indexOf("let previousInboundStats")),
    receiverFunction("sendClientLog"), receiverFunction("sendClientProfile"),
    receiverFunction("sendClientQos"), receiverFunction("diagnosticsReport"),
  ];
  for (const path of paths) {
    assert.ok(path.length > 0);
    assert.doesNotMatch(path, /signalingTicket|signalingPc|\.\.\.msg|\.\.\.message|\.\.\.hello/);
  }
  // The inline /__diag/client sender takes explicit trace details; it does not
  // intercept socket traffic or read credentials from the receiver module.
  const bootstrap = index.slice(index.indexOf("(() => {"), index.indexOf("})();") + 5);
  assert.match(bootstrap, /fetch\("\/__diag\/client"/);
  assert.doesNotMatch(bootstrap, /WebSocket|ticket|signalingPc|hello-ok/);
});
