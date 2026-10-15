import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";
import { FileTransferClient } from "../public/file-client.js";

const source = readFileSync(new URL("../public/receiver.js", import.meta.url), "utf8").replaceAll("\r\n", "\n");
const index = readFileSync(new URL("../public/index.html", import.meta.url), "utf8");
const epoch = "a".repeat(32);
const ticket = "test-only-secret-socket-ticket";

function receiverFunction(name) {
  const match = source.match(new RegExp(`(?:async )?function ${name}\\([^]*?\\n\\}`));
  assert.ok(match, `receiver function ${name}`);
  return match[0];
}

function urlHarness(overrides = {}) {
  const context = vm.createContext({ URLSearchParams, proto: "wss:", location: { host: "relay.test" },
    params: new URLSearchParams(), labRun: null, labCompatibilityId: "compat-v1",
    mediaEpoch: epoch, signalingPc: null, signalingTicket: ticket, ...overrides });
  vm.runInContext(["signalingSocketUrl", "mediaSocketUrl", "fileSocketUrl"].map(receiverFunction).join("\n"), context);
  return context;
}

test("signaling passes the trimmed page PC selector through encodeURIComponent", () => {
  const h = urlHarness();
  for (const query of ["", "?pc=", "?pc=%20%20", "?other=pc"]) {
    h.params = new URLSearchParams(query);
    assert.equal(h.signalingSocketUrl(), "wss://relay.test/ws");
  }
  // IDs are opaque here: aliases, punctuation and spaces are relay concerns.
  for (const pc of ["abcd-oiL0", "one two/+?&=雪"]) {
    h.params = new URLSearchParams({ pc: `  ${pc}  ` });
    assert.equal(h.signalingSocketUrl(), `wss://relay.test/ws?pc=${encodeURIComponent(pc)}`);
    h.labRun = "run /+";
    assert.equal(h.signalingSocketUrl(), `wss://relay.test/lab-ws?run=run%20%2F%2B&compat=compat-v1&pc=${encodeURIComponent(pc)}`);
    h.labRun = null;
  }
  h.labRun = "run"; h.params = new URLSearchParams();
  assert.equal(h.signalingSocketUrl(), "wss://relay.test/lab-ws?run=run&compat=compat-v1");
});

test("media and file URLs append resolved PC after epoch, including lab routes", () => {
  const h = urlHarness({ params: new URLSearchParams({ pc: "requested-pc" }) });
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

function signalHarness() {
  const output = [];
  const sockets = [];
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
  vm.runInContext(["sendSignal", "sendClientLog", "handleSignal", "reconnectSignaling", "recycleSignaling"].map(receiverFunction).join("\n"), context);
  context.reconnectSignaling("startup");
  const socket = sockets[0];
  socket.readyState = Socket.OPEN; socket.bufferedAmount = 0;
  const helloOk = fields => context.handleSignal(socket, 1, { data: JSON.stringify({ type: "hello-ok", mediaEpoch: epoch, ...fields }) });
  return { context, socket, output, helloOk };
}

test("hello-ok replaces credentials, omitted fields clear them, and stale sockets cannot overwrite them", async () => {
  const h = signalHarness();
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
    const h = signalHarness();
    await h.helloOk({ pc: "pc-one", ticket });
    if (recycle) h.context.recycleSignaling("peer-left");
    else h.socket.onclose({ code: 1008, reason: "policy" });
    assert.equal(h.context.signalingPc, null);
    assert.equal(h.context.signalingTicket, null);
    assert.ok(!JSON.stringify(h.output).includes(ticket));
  }
});

test("signaling hello and hello-ok traces/logs never include session credentials", async () => {
  const h = signalHarness();
  await h.helloOk({ pc: "resolved", ticket });
  h.socket.onopen();
  const hello = h.output.find(message => message.type === "hello");
  assert.deepEqual(hello, { type: "hello", role: "client" });
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
    assert.equal(sent.at(-1), JSON.stringify({ type: "hello", role: "media-client", ...(value ? { ticket: value } : {}) }));
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
        assert.equal(sent.at(-1), JSON.stringify({ type: "hello", role: "file-client", ...(withCallback && value ? { ticket: value } : {}) }));
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
