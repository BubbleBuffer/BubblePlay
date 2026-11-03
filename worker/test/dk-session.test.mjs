import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createHash, createPrivateKey, hkdfSync, sign } from "node:crypto";
import vm from "node:vm";
import { createViewerSession, browserDeviceName, sessionRefusal } from "../public/dk-session.js";
import { createCtlChannel } from "../public/dk-crypto.js";

const { handshake: hs } = JSON.parse(readFileSync(new URL("../../fixtures/device-keys-contract.json", import.meta.url)));
const inputContract = JSON.parse(readFileSync(new URL("../../fixtures/input-contract.json", import.meta.url)));
const source = readFileSync(new URL("../public/receiver.js", import.meta.url), "utf8");
const bytes = value => new Uint8Array(Buffer.from(value, "base64url"));
const hex = value => new Uint8Array(Buffer.from(value, "hex"));
const localSdp = `v=0\r\na=fingerprint:${hs.dtls.viewer.localFingerprint}\r\n`;
const remoteSdp = `v=0\r\na=fingerprint:${hs.dtls.viewer.remoteFingerprint}\r\n`;
const caps = ["control-aead-v1", "dtls-bind-v1"];
const viewerCaps = [...caps, "client-logs-v1"];
const flush = async () => { for (let i = 0; i < 12; i++) await new Promise(resolve => setImmediate(resolve)); };

async function harness(options = {}) {
  const { viaReceiver = false, ...sessionOverrides } = options;
  const timers = new Map();
  let nextTimer = 0;
  const events = { sent: [], failures: [], authenticated: 0, ready: 0, messages: [], binary: [] };
  const privateKey = await crypto.subtle.importKey("pkcs8", hex("302e020100300506032b657004220420" + hs.viewer.seedHex), "Ed25519", false, ["sign"]);
  const key = text => crypto.subtle.importKey("raw", hex(text), "AES-GCM", false, ["encrypt", "decrypt"]);
  const pcCtl = createCtlChannel({ sendKey: await key(hs.controlKeys.pcToViewerHex), recvKey: await key(hs.controlKeys.viewerToPcHex) });
  const sessionOptions = {
    identity: { id: hs.viewer.id, publicRaw: bytes(hs.viewer.public), privateKey },
    pcId: hs.pc.id, pinnedPublic: bytes(hs.pc.public), relayOrigin: hs.relayOrigin, epoch: hs.sessionEpoch,
    name: "Firefox on Linux", helloOptions: { nonce: bytes(hs.viewer.nonce), ephemeralSeed: hex(hs.viewer.ephemeralPrivateHex) },
    send: message => { events.sent.push(message); return true; },
    onFailure: (refusal, reason) => events.failures.push({ ...refusal, reason }),
    onAuthenticated: () => events.authenticated++, onMediaReady: () => events.ready++,
    onMessage: message => events.messages.push(message), onBinary: (kind, data) => events.binary.push({ kind, data }),
    schedule: (fn, delay) => { const id = ++nextTimer; timers.set(id, { fn, delay }); return id; },
    cancel: id => timers.delete(id), ...sessionOverrides,
  };
  let session;
  if (viaReceiver) {
    const socket = {};
    const context = vm.createContext({ ws: socket, signalingGeneration: 1, signalingRegistered: false,
      sessionPc: hs.pc.id, sessionIdentity: sessionOptions.identity, sessionPinnedPublic: sessionOptions.pinnedPublic,
      authenticatedSession: null, signalingPc: null, signalingTicket: null, Date, JSON,
      mediaEpoch: null, wssSelected: sessionOptions.transport === "wss", hudState: {},
      location: { href: hs.relayOrigin }, validateIceServers: value => value, relayOrigin: () => hs.relayOrigin,
      debugTrace() {}, renderAbout() {}, noteFileEpoch() {},
      endAuthenticatedSession: refusal => events.failures.push(refusal),
      // Observe real session traffic while the actual receiver handler supplies
      // the epoch from hello-ok. Test hooks replace only I/O and observation.
      createViewerSession: config => (session = createViewerSession({ ...config, ...sessionOptions, epoch: config.epoch })),
      event: { data: JSON.stringify({ type: "hello-ok", pc: hs.pc.id, ticket: "opaque-ticket", mediaEpoch: sessionOptions.epoch }) },
    });
    const handleSignal = source.slice(source.indexOf("async function handleSignal("), source.indexOf("\nasync function handleEndpointSignal("));
    await vm.runInContext(handleSignal + "\nhandleSignal(ws, 1, event);", context);
    assert.equal(context.mediaEpoch, sessionOptions.epoch);
  } else {
    session = createViewerSession(sessionOptions);
    await session.start();
  }
  return { session, events, timers, pcCtl,
    async authenticate(pcCaps = caps) {
      await session.receive(hs.messages.authHost);
      const envelope = events.sent.find(message => message.type === "ctl");
      const viewerCaps = await pcCtl.openJson(envelope);
      await session.receive(await pcCtl.sealJson({ type: "caps", caps: pcCaps, extra: "ignored" }));
      return viewerCaps;
    },
  };
}

test("fixture handshake sends viewer caps first, and PC caps unlock control but not WebRTC media/input", async () => {
  const h = await harness();
  assert.deepEqual(h.events.sent, [hs.messages.authHello]);
  assert.equal(h.session.sendControl({ type: "offer", sdp: localSdp }), false);
  assert.equal(h.session.sendInput(new Uint8Array([1, 2])), false);
  assert.deepEqual(await h.authenticate(), { type: "caps", caps: viewerCaps, name: "Firefox on Linux" });
  assert.deepEqual(h.events.sent[1], hs.messages.authFinish);
  assert.equal(h.events.sent[2].seq, 0);
  assert.equal(h.events.authenticated, 1);
  assert.equal(h.session.controlReady, true);
  assert.equal(h.session.mediaReady, false);
  assert.equal(h.events.ready, 0);
  assert.equal(h.session.sendInput(new Uint8Array([1, 2])), false);
  assert.equal(h.session.sendControl({ type: "offer", sdp: localSdp }), true);
  // Sealing runs on WebCrypto's thread pool; wait for the message instead of a fixed number of ticks.
  for (let i = 0; i < 500 && h.events.sent.length < 4; i++) await new Promise(resolve => setTimeout(resolve, 10));
  assert.deepEqual(await h.pcCtl.openJson(h.events.sent[3]), { type: "offer", sdp: localSdp });
  h.session.close();
});

test("auth-host timeout is exactly ten seconds and a late reply cannot resume", async () => {
  const h = await harness();
  const timer = [...h.timers.values()][0];
  assert.equal(timer.delay, 10_000);
  timer.fn();
  assert.deepEqual(h.events.failures, [{ reason: "auth-timeout", title: "PC not reachable", body: "This PC does not support device keys or is not reachable." }]);
  assert.equal(h.timers.size, 0);
  await h.session.receive(hs.messages.authHost);
  assert.equal(h.events.sent.length, 1);
  assert.equal(h.session.controlReady, false);
});

test("an auth-host received before the deadline cancels the timer before verification yields", async () => {
  const h = await harness();
  const work = h.session.receive(hs.messages.authHost);
  await Promise.resolve();
  assert.equal(h.timers.size, 0, "the no-reply deadline cannot fire during verification");
  await work;
  assert.equal(h.timers.size, 1, "caps discovery gets its own deadline after verification");
  h.session.close();
});

test("missing pin and non-string, empty, overlong or non-ASCII epochs fail before auth-hello", async () => {
  for (const options of [{ pinnedPublic: null }, { epoch: 9007199254740992 },
    { epoch: "" }, { epoch: "x".repeat(65) }, { epoch: "session\n" }, { epoch: "\u007f" }, { epoch: "\u00e9" }]) {
    const h = await harness(options);
    assert.equal(h.events.sent.length, 0);
    assert.equal(h.events.failures.length, 1);
  }
});

test("opaque ASCII epochs preserve exact transcript bytes, including the relay's hex mediaEpoch", async () => {
  for (const epoch of ["8f7c62d194a04e3bb9e18cd604cf5a20", " opaque:relay-epoch ", "~".repeat(64)]) {
    const h = await harness({ epoch, transport: "wss", viaReceiver: true });
    assert.deepEqual(h.events.sent, [hs.messages.authHello]);
    // Independent Node encoding/signing verifies the runtime did not parse,
    // trim, canonicalise or reinterpret the relay's opaque epoch string.
    const fields = ["bubbleplay-auth-v1", hs.relayOrigin, epoch, hs.viewer.id,
      bytes(hs.viewer.public), hs.pc.id, bytes(hs.pc.public), bytes(hs.viewer.nonce),
      bytes(hs.pc.nonce), bytes(hs.viewer.ephemeral), bytes(hs.pc.ephemeral)];
    const encoded = fields.flatMap(field => {
      const value = Buffer.from(field);
      const length = Buffer.alloc(4);
      length.writeUInt32BE(value.length);
      return [length, value];
    });
    const H = createHash("sha256").update(Buffer.concat(encoded)).digest();
    const privateKey = seed => createPrivateKey({ key: Buffer.from("302e020100300506032b657004220420" + seed, "hex"), format: "der", type: "pkcs8" });
    const pcSignature = sign(null, Buffer.concat([Buffer.from("bubbleplay-auth-host-v1"), H]), privateKey(hs.pc.seedHex));
    await h.session.receive({ ...hs.messages.authHost, signature: pcSignature.toString("base64url") });
    assert.deepEqual(h.events.failures, []);
    assert.deepEqual(h.events.sent[1], { type: "auth-finish", signature: sign(null,
      Buffer.concat([Buffer.from("bubbleplay-auth-viewer-v1"), H, pcSignature]), privateKey(hs.viewer.seedHex)).toString("base64url") });
    const key = async info => crypto.subtle.importKey("raw", hkdfSync("sha256", Buffer.from(hs.sharedSecretHex, "hex"), H, info, 32), "AES-GCM", false, ["encrypt", "decrypt"]);
    const pcCtl = createCtlChannel({ sendKey: await key("bubbleplay-ctl-v1 pc-to-viewer"), recvKey: await key("bubbleplay-ctl-v1 viewer-to-pc") });
    assert.deepEqual(await pcCtl.openJson(h.events.sent[2]), { type: "caps", caps: viewerCaps, name: "Firefox on Linux" });
    await h.session.receive(await pcCtl.sealJson({ type: "caps", caps }));
    assert.equal(h.session.controlReady, true);
    assert.equal(h.session.mediaReady, true);
    h.session.close();
  }
});

test("changed pin and bad PC signature refuse before auth-finish or media", async () => {
  for (const [options, host, title] of [
    [{ pinnedPublic: bytes(hs.viewer.public) }, hs.messages.authHost, "PC key changed"],
    [{}, { ...hs.messages.authHost, signature: hs.messages.authFinish.signature }, "Session ended"],
  ]) {
    const h = await harness(options);
    await h.session.receive(host);
    assert.equal(h.events.failures[0].title, title);
    assert.equal(h.events.sent.length, 1);
    assert.equal(h.events.authenticated, 0);
    assert.equal(h.session.mediaReady, false);
  }
});

test("PC first ctl must be caps, and mandatory caps cannot be downgraded", async () => {
  for (const pcCaps of [[], ["control-aead-v1"], ["dtls-bind-v1"]]) {
    const h = await harness();
    await h.authenticate(pcCaps);
    assert.equal(h.events.authenticated, 0);
    assert.equal(h.events.failures[0].title, "PC needs an update");
    assert.equal(h.events.failures[0].body.includes(pcCaps.includes("control-aead-v1") ? "dtls-bind-v1" : "control-aead-v1"), true);
  }
  const h = await harness();
  await h.session.receive(hs.messages.authHost);
  await h.session.receive(await h.pcCtl.sealJson({ type: "clipboard", text: "unready" }));
  assert.equal(h.events.failures.length, 1);
  assert.deepEqual(h.events.messages, []);
});

test("PC caps gate also expires; an unsupported peer never leaves a permanent busy screen", async () => {
  const h = await harness();
  await h.session.receive(hs.messages.authHost);
  assert.equal(h.events.sent[2].kind, "caps");
  [...h.timers.values()][0].fn();
  assert.equal(h.events.failures[0].title, "PC needs an update");
  assert.equal(h.session.controlReady, false);
});

test("both DTLS binds are required; an early PC bind waits for actual connected SDP", async () => {
  const h = await harness();
  await h.authenticate();
  await h.session.receive(hs.messages.authBindPc);
  assert.equal(h.session.mediaReady, false);
  await h.session.bindPeer({ localSdp, remoteSdp });
  assert.deepEqual(h.events.sent.at(-1), hs.messages.authBind);
  assert.equal(h.session.mediaReady, true);
  assert.equal(h.session.rtcReady, true);
  assert.equal(h.events.ready, 1);
  await h.session.bindPeer({ localSdp, remoteSdp });
  assert.equal(h.events.sent.filter(message => message.type === "auth-bind").length, 1);
  h.session.close();
});

test("viewer bind alone leaves input gated; valid PC bind opens it", async () => {
  const h = await harness();
  await h.authenticate();
  await h.session.bindPeer({ localSdp, remoteSdp });
  assert.equal(h.session.mediaReady, false);
  assert.equal(h.session.sendInput(new Uint8Array([1, 2])), false);
  await h.session.receive(hs.messages.authBindPc);
  assert.equal(h.session.mediaReady, true);
  assert.equal(h.events.ready, 1);
  assert.equal(h.events.failures.length, 0);
  h.session.close();
});

test("wrong signer order, changed SDP and conflicting fingerprints close the session", async () => {
  for (const [local, remote, bind] of [
    [localSdp, remoteSdp, hs.messages.authBind],
    [localSdp.replace("A0:A1", "00:01"), remoteSdp, hs.messages.authBindPc],
    [localSdp + "a=fingerprint:sha-256 00:01\r\n", remoteSdp, hs.messages.authBindPc],
  ]) {
    const h = await harness();
    await h.authenticate();
    await h.session.receive(bind);
    await h.session.bindPeer({ localSdp: local, remoteSdp: remote });
    assert.equal(h.events.failures[0].reason, "bind-failed");
    assert.equal(h.events.failures[0].body, "The PC's connection could not be verified, so the session was closed.");
    assert.equal(h.session.mediaReady, false);
    assert.equal(h.events.ready, 0);
  }
});

test("decision 23: an SDP without a sha-256 fingerprint sends dtls-bind-required and ends the session", async () => {
  for (const [local, remote] of [
    ["v=0\r\na=fingerprint:sha-1 00:11:22\r\n", remoteSdp],
    [localSdp, "v=0\r\n"],
  ]) {
    const h = await harness();
    await h.authenticate();
    await h.session.bindPeer({ localSdp: local, remoteSdp: remote });
    assert.equal(h.events.sent.some(message => message.type === "auth-bind"), false);
    const error = await h.pcCtl.openJson(h.events.sent.at(-1));
    assert.equal(error.type, "error");
    assert.equal(error.code, "dtls-bind-required");
    assert.equal(h.events.failures[0].reason, "bind-failed");
    assert.equal(h.session.mediaReady, false);
  }
});

test("lowercase SDP fingerprints still produce the fixture's auth-bind", async () => {
  const h = await harness();
  await h.authenticate();
  const lower = sdp => sdp.replace(/(a=fingerprint:)sha-256 ([0-9A-F:]+)/, (_, prefix, hex) => `${prefix}SHA-256 ${hex.toLowerCase()}`);
  await h.session.bindPeer({ localSdp: lower(localSdp), remoteSdp: lower(remoteSdp) });
  assert.deepEqual(h.events.sent.at(-1), hs.messages.authBind);
  assert.equal(h.events.failures.length, 0);
  h.session.close();
});

test("bind timeout ends the session, and a new peer cannot reuse an old readiness grant", async () => {
  const h = await harness();
  await h.authenticate();
  await h.session.bindPeer({ localSdp, remoteSdp });
  const timer = [...h.timers.values()][0];
  assert.equal(timer.delay, 10_000);
  timer.fn();
  assert.equal(h.events.failures[0].reason, "bind-failed");
  const next = await harness();
  await next.authenticate();
  await next.session.bindPeer({ localSdp, remoteSdp });
  await next.session.receive(hs.messages.authBindPc);
  next.session.resetPeer();
  assert.equal(next.session.mediaReady, false);
  assert.equal(next.session.sendInput(new Uint8Array([1])), false);
  next.session.close();
});

test("WSS requires control-aead only, starts without bind and wraps exact binary input bytes", async () => {
  const h = await harness({ transport: "wss" });
  assert.deepEqual(await h.authenticate(["control-aead-v1"]), { type: "caps", caps: viewerCaps, name: "Firefox on Linux" });
  assert.equal(h.session.mediaReady, true);
  assert.equal(h.session.rtcReady, false);
  assert.equal(h.events.ready, 1);
  const input = new Uint8Array([0, 1, 2, 3]);
  assert.equal(h.session.sendInput(input), true);
  input.fill(255);
  // Sealing is async WebCrypto; wait for the envelope rather than a fixed number of turns.
  for (let i = 0; i < 50 && h.events.sent.at(-1)?.kind !== "input"; i++) await flush();
  const envelope = h.events.sent.at(-1);
  assert.equal(envelope.kind, "input");
  assert.equal(envelope.seq, 1);
  assert.deepEqual(await h.pcCtl.open(envelope), new Uint8Array([0, 1, 2, 3]));
  h.session.close();
});

test("media-control, offer and fixture binary input share the same ordered ctl sequence after caps", async () => {
  const h = await harness({ transport: "wss" });
  await h.authenticate();
  const media = { type: "media-control", action: "mode", protocol: 4, epoch: hs.sessionEpoch, mode: "work" };
  const offer = { type: "offer", sdp: localSdp };
  const input = new Uint8Array(inputContract.packets.find(packet => packet.name === "arrow-left-down").bytes);
  assert.equal(h.session.sendControl(media), true);
  assert.equal(h.session.sendControl(offer), true);
  assert.equal(h.session.sendInput(input), true);
  for (let i = 0; i < 100 && h.events.sent.length < 6; i++) await new Promise(resolve => setTimeout(resolve, 5));
  const envelopes = h.events.sent.filter(message => message.type === "ctl");
  assert.deepEqual(envelopes.map(message => [message.kind, message.seq]), [["caps", 0], ["media-control", 1], ["offer", 2], ["input", 3]]);
  assert.equal(h.events.sent.slice(2).every(message => message.type === "ctl"), true);
  assert.deepEqual(await h.pcCtl.openJson(envelopes[1]), media);
  assert.deepEqual(await h.pcCtl.openJson(envelopes[2]), offer);
  assert.deepEqual(await h.pcCtl.open(envelopes[3]), input);
  h.session.close();
});

test("raw cursor and input kinds use open; other kinds require JSON type equality", async () => {
  const h = await harness({ transport: "wss" });
  await h.authenticate();
  const raw = new Uint8Array([0, 0, 255, 7]);
  await h.session.receive(await h.pcCtl.seal("cursor", raw));
  await h.session.receive(await h.pcCtl.seal("input", raw));
  assert.deepEqual(h.events.binary, [{ kind: "cursor", data: raw }, { kind: "input", data: raw }]);
  await h.session.receive(await h.pcCtl.seal("clipboard", new TextEncoder().encode('{"type":"quality"}')));
  assert.equal(h.events.failures.length, 1);
  assert.deepEqual(h.events.messages, []);
});

test("replay, skipped sequence, bad tag and wrong direction terminate and reject later traffic", async () => {
  for (const fault of ["replay", "gap", "tag", "direction"]) {
    const h = await harness({ transport: "wss" });
    await h.authenticate();
    let envelope = await h.pcCtl.sealJson({ type: "clipboard", text: "hello" });
    if (fault === "replay") {
      await h.session.receive(envelope);
    } else if (fault === "gap") envelope = { ...envelope, seq: envelope.seq + 1 };
    else if (fault === "tag") envelope = { ...envelope, data: "AA" + envelope.data.slice(2) };
    else envelope = h.events.sent.find(message => message.type === "ctl");
    await h.session.receive(envelope);
    assert.equal(h.events.failures.length, 1, fault);
    assert.equal(h.session.controlReady, false);
    assert.equal(h.session.sendControl({ type: "ping" }), false);
    await h.session.receive(await h.pcCtl.sealJson({ type: "quality", value: 1 }));
    assert.equal(h.events.failures.length, 1);
    assert.equal(h.events.messages.some(message => message.type === "quality"), false);
  }
});

test("encrypted PC refusal codes are shown before caps and stop the session", async () => {
  for (const code of ["dtls-bind-required", "cap-missing"]) {
    const h = await harness();
    await h.session.receive(hs.messages.authHost);
    await h.session.receive(await h.pcCtl.sealJson({ type: "error", code, message: "<script>" + "x".repeat(300) }));
    assert.equal(h.events.failures[0].title, "PC refused the session");
    assert.equal(h.events.failures[0].body, sessionRefusal(code).body + " " + ("<script>" + "x".repeat(300)).slice(0, 200));
    assert.equal(h.session.controlReady, false);
    assert.equal(h.events.authenticated, 0);
  }
});

test("closing during auth work never emits a finish or grants readiness", async () => {
  const h = await harness();
  const work = h.session.receive(hs.messages.authHost);
  await Promise.resolve();
  h.session.close();
  await work;
  assert.equal(h.events.sent.length, 1);
  assert.equal(h.events.authenticated, 0);
  assert.equal(h.events.ready, 0);
  assert.equal(h.events.failures.length, 0);
});

test("browser label uses brand/platform with a safe fallback", () => {
  assert.equal(browserDeviceName({ userAgentData: { brands: [{ brand: "Microsoft Edge" }], platform: "Windows" } }), "Edge on Windows");
  assert.equal(browserDeviceName({ userAgent: "Mozilla/5.0 (Linux; Android 16) Chrome/140.0" }), "Chrome on Android");
  assert.equal(browserDeviceName({ userAgent: "Mozilla/5.0 (iPhone) AppleWebKit Safari/604.1" }), "Safari on iOS");
  assert.equal(browserDeviceName({}), "Browser");
});

test("receiver drops plaintext endpoint frames and preserves mediaEpoch string in authentication", async () => {
  const received = [], configs = [], errors = [];
  const socket = {};
  const context = vm.createContext({ ws: socket, signalingGeneration: 7, signalingRegistered: false,
    sessionPc: hs.pc.id, sessionIdentity: {}, sessionPinnedPublic: bytes(hs.pc.public),
    authenticatedSession: null, signalingPc: null, signalingTicket: null,
    sessionGate: {}, Date, JSON, currentIceServers: null, mediaEpoch: null, wssSelected: false,
    hudState: {}, console: { info() {} }, location: { href: hs.relayOrigin }, validateIceServers: value => value,
    debugTrace() {}, noteFileEpoch() {}, renderAbout() {}, relayOrigin: () => hs.relayOrigin,
    viewerLog() {}, sendStreamMode() {}, startPeer() {}, peerGeneration: 0,
    endAuthenticatedSession: refusal => errors.push(refusal),
    createViewerSession: config => { configs.push(config); return { async start() {}, receive: message => received.push(message) }; },
  });
  const fn = source.slice(source.indexOf("async function handleSignal("), source.indexOf("\nasync function handleEndpointSignal("));
  vm.runInContext(fn, context);
  const deliver = value => { context.event = { data: typeof value === "string" ? value : JSON.stringify(value) }; return vm.runInContext("handleSignal(ws, 7, event)", context); };
  const relayEpoch = "8f7c62d194a04e3bb9e18cd604cf5a20";
  await deliver({ type: "hello-ok", pc: hs.pc.id, ticket: "opaque-ticket", mediaEpoch: relayEpoch });
  assert.equal(context.mediaEpoch, relayEpoch);
  assert.equal(configs[0].epoch, relayEpoch);
  for (const type of ["answer", "ice", "host-capabilities", "clipboard", "desktop-state"]) await deliver({ type });
  context.event = { data: new ArrayBuffer(12) };
  await vm.runInContext("handleSignal(ws, 7, event)", context);
  assert.equal(received.length, 0);
  await deliver({ type: "ctl", kind: "clipboard", seq: 0, data: "AA" });
  assert.equal(received.length, 1);
  assert.equal(errors.length, 0);
});

test("receiver cannot send DataChannel input or WSS media hello before authentication", () => {
  const sent = [];
  const context = vm.createContext({ authenticatedSession: { mediaReady: false, rtcReady: false, sendInput: () => { sent.push("ctl-input"); return true; } },
    desktopInputState: "enabled", desktopAvailable: true, desktopAwaitingFrame: false,
    usingWssMedia: false, signalingRegistered: true, WebSocket: { OPEN: 1 }, ws: { readyState: 1, bufferedAmount: 0 },
    mediaEpoch: hs.sessionEpoch, signalingTicket: "ticket", wssV4: {},
    data: new Uint8Array([1]), channel: { readyState: "open", send: () => sent.push("raw-input") },
  });
  const inputFn = source.slice(source.indexOf("function sendInput("), source.indexOf("\nlet motionFrame"));
  vm.runInContext(inputFn, context);
  assert.equal(vm.runInContext("sendInput(data, channel)", context), false);
  assert.deepEqual(sent, []);
  context.authenticatedSession.mediaReady = true;
  assert.equal(vm.runInContext("sendInput(data, channel)", context), false, "a WSS readiness grant cannot authorize an unbound DataChannel");
  context.authenticatedSession.rtcReady = true;
  assert.equal(vm.runInContext("sendInput(data, channel)", context), true);
  assert.deepEqual(sent, ["raw-input"]);
  context.usingWssMedia = true;
  assert.equal(vm.runInContext("sendInput(data, channel)", context), true);
  assert.deepEqual(sent, ["raw-input", "ctl-input"]);
  context.signalingTicket = null;
  const socketFn = source.slice(source.indexOf("function openWssV4Socket("), source.indexOf("\nfunction onWssV4Packet("));
  vm.runInContext(socketFn + "\nopenWssV4Socket();", context);
  assert.equal(context.wssV4.socket, undefined);
});

test("receiver disables and holds RTC audio/video tracks until the verified media gate opens", () => {
  const rendered = [], stopped = [], persisted = [];
  const candidate = {};
  const track = kind => ({ kind, enabled: true, readyState: "live", stop() { stopped.push(kind); this.readyState = "ended"; } });
  const video = track("video"), audio = track("audio");
  const context = vm.createContext({ candidate, pc: candidate, generation: 4, peerGeneration: 4,
    authenticatedSession: { mediaReady: false }, usingWssMedia: false, wssSelected: false, pendingRtcTracks: [],
    sessionGate: { hide: () => rendered.push("gate-hidden") }, sessionTouched: false, sessionPc: hs.pc.id,
    touchPc: id => { persisted.push(id); return Promise.resolve(); },
    updateAuthenticatedInputReady: () => rendered.push("input-update"),
    onTrack: event => rendered.push(event.track.kind),
    isCurrentPeer: generation => generation === 4,
    videoEvent: { track: video }, audioEvent: { track: audio },
  });
  const trackHandler = source.slice(source.indexOf("  candidate.ontrack ="), source.indexOf("  candidate.onconnectionstatechange ="));
  const openMedia = source.slice(source.indexOf("function openAuthenticatedMedia("), source.indexOf("\nfunction maybeBindRtcPeer("));
  vm.runInContext(trackHandler + "\n" + openMedia + "\ncandidate.ontrack(videoEvent); candidate.ontrack(audioEvent); openAuthenticatedMedia();", context);
  assert.equal(video.enabled, false);
  assert.equal(audio.enabled, false);
  assert.equal(context.pendingRtcTracks.length, 2);
  assert.deepEqual(rendered, []);
  assert.deepEqual(persisted, []);
  context.authenticatedSession.mediaReady = true;
  vm.runInContext("openAuthenticatedMedia();", context);
  assert.equal(video.enabled, true);
  assert.equal(audio.enabled, true);
  assert.equal(context.pendingRtcTracks.length, 0);
  assert.deepEqual(rendered, ["gate-hidden", "video", "audio", "input-update"]);
  assert.deepEqual(persisted, [hs.pc.id]);
  assert.deepEqual(stopped, []);
});

test("receiver binds the peer only once DTLS is connected, with its actual local and remote SDP", () => {
  const bindings = [];
  const candidate = { connectionState: "connecting", localDescription: { sdp: localSdp }, remoteDescription: { sdp: remoteSdp } };
  const context = vm.createContext({ pc: candidate, candidate, peerGeneration: 3,
    isCurrentPeer: generation => generation === 3, usingWssMedia: false, wssSelected: false,
    authenticatedSession: { bindPeer: descriptions => bindings.push(descriptions) },
  });
  const fn = source.slice(source.indexOf("function maybeBindRtcPeer("), source.indexOf("\nfunction restartPeer("));
  vm.runInContext(fn + "\nmaybeBindRtcPeer();", context);
  assert.equal(bindings.length, 0);
  candidate.connectionState = "connected";
  vm.runInContext("maybeBindRtcPeer();", context);
  assert.equal(bindings[0].localSdp, localSdp);
  assert.equal(bindings[0].remoteSdp, remoteSdp);
  context.usingWssMedia = true;
  vm.runInContext("maybeBindRtcPeer();", context);
  assert.equal(bindings.length, 1);
});

test("receiver failure closes authentication, media, pending tracks and input with no reconnect", () => {
  const calls = [];
  const context = vm.createContext({ terminalSignalingFailure: false,
    authenticatedSession: { close: () => calls.push("auth-close") }, reconnectTimer: 5,
    ws: { close: () => calls.push("socket-close") }, signalingGeneration: 3, signalingRegistered: true,
    signalingPc: hs.pc.id, signalingTicket: "ticket", clearTimeout: () => calls.push("reconnect-cancel"),
    clearFirstFrameWatchdog: () => calls.push("watchdog-clear"),
    cleanupPeer: () => calls.push("peer-cleanup"), noteFileEpoch: value => calls.push(["file-epoch", value]),
    setFileTransferAvailable: value => calls.push(["files", value]), hudState: {},
    hud: { classList: { add() {} } },
    sessionGate: { showRefusal: value => calls.push(["refusal", value.title]) },
  });
  const failFn = source.slice(source.indexOf("function endAuthenticatedSession("), source.indexOf("\nfunction updateAuthenticatedInputReady("));
  vm.runInContext(failFn + '\nendAuthenticatedSession({title:"Session ended",body:"integrity"});', context);
  assert.equal(context.authenticatedSession, null);
  assert.equal(context.ws, null);
  assert.equal(context.signalingRegistered, false);
  assert.equal(context.signalingTicket, null);
  assert.equal(context.terminalSignalingFailure, true);
  assert.deepEqual(calls, ["auth-close", "reconnect-cancel", "watchdog-clear", "peer-cleanup", ["file-epoch", null], ["files", false], "socket-close", ["refusal", "Session ended"]]);
  // Exercise the actual cleanup prefix that the failure path calls. Its order
  // closes stale tracks and sends release-all before removing the RTC lanes.
  const cleanupCalls = [];
  const cleanupContext = vm.createContext({ authenticatedSession: { resetPeer: () => cleanupCalls.push("peer-reset") },
    pendingRtcTracks: [{ event: { track: { stop: () => cleanupCalls.push("track-stop") } } }],
    fileDragPreview: { cancel() {} }, nativeFileDropPreviewAvailable: true,
    fileDropPresentedGeneration: 3, fileDropPresentedViewportRevision: 1, usingWssMedia: false,
    soundController: { detachSink() {} }, clearWssTimers() {}, closingPeer: false,
    releaseInput: () => cleanupCalls.push("release-input"),
  });
  const cleanupPrefix = source.slice(source.indexOf("function cleanupPeer("), source.indexOf("  resetKeyboardDraft();", source.indexOf("function cleanupPeer(")));
  vm.runInContext(cleanupPrefix + "\n}\ncleanupPeer();", cleanupContext);
  assert.deepEqual(cleanupCalls, ["track-stop", "release-input", "peer-reset"]);
  assert.equal(cleanupContext.pendingRtcTracks.length, 0);
});

test("receiver drains a final encrypted refusal before processing socket close or reconnect", async () => {
  const sockets = [], calls = [], timers = [];
  let release;
  const decrypting = new Promise(resolve => { release = resolve; });
  class Socket {
    static OPEN = 1; static CONNECTING = 0;
    constructor() { this.readyState = 0; sockets.push(this); }
  }
  const context = vm.createContext({ WebSocket: Socket, URL, Promise, performance: { now: () => 1 },
    authenticatedSession: null, terminalSignalingFailure: false, reconnectTimer: null,
    ws: null, signalingGeneration: 0, signalingRegistered: true, signalingPc: hs.pc.id, signalingTicket: "ticket",
    latestInputStatus: null, inputStatusReason: null, hudState: {}, hud: { classList: { add() {} } },
    signalingSocketUrl: () => "wss://test.invalid/ws", debugTrace() {}, viewerLog() {}, cleanupPeer: () => calls.push("cleanup"),
    recordSignalingError() {}, shouldReconnectAfterClose: (_code, terminal) => !terminal,
    restartPeer: () => calls.push("restart"),
    setTimeout: (fn, delay) => { timers.push({ fn, delay }); return timers.length; },
    handleSignal: async () => { calls.push("decrypt-start"); await decrypting; context.terminalSignalingFailure = true; calls.push("refusal"); },
  });
  const fn = source.slice(source.indexOf("function reconnectSignaling("), source.indexOf("\nfunction signalingSocketUrl("));
  vm.runInContext(fn + '\nreconnectSignaling("startup");', context);
  const socket = sockets[0];
  socket.readyState = 1;
  socket.onmessage({ data: '{"type":"ctl"}' });
  socket.readyState = 3;
  socket.onclose({ code: 1006 });
  await Promise.resolve();
  assert.deepEqual(calls, ["decrypt-start"]);
  assert.equal(timers.length, 0);
  release();
  await flush();
  assert.deepEqual(calls, ["decrypt-start", "refusal", "cleanup"]);
  assert.equal(timers.length, 0);
  assert.equal(sockets.length, 1);
});

test("WSS teardown releases through ctl and cannot authorize an unbound DataChannel", () => {
  const sent = [];
  let context;
  const lane = { readyState: "open", bufferedAmount: 0, send: () => sent.push("plaintext-dc") };
  context = vm.createContext({ authenticatedSession: { mediaReady: true, rtcReady: false,
    sendInput: () => { sent.push("ctl-input"); return true; }, resetPeer() {} },
    desktopInputState: "enabled", desktopAvailable: true, desktopAwaitingFrame: false,
    usingWssMedia: true, signalingRegistered: true, WebSocket: { OPEN: 1 }, ws: { readyState: 1, bufferedAmount: 0 },
    dc: lane, motionDc: lane, data: new Uint8Array([1]), pendingRtcTracks: [],
    fileDragPreview: { cancel() {} }, nativeFileDropPreviewAvailable: true,
    fileDropPresentedGeneration: 3, fileDropPresentedViewportRevision: 1, closingPeer: false,
    releaseInput: () => vm.runInContext("sendInput(data, dc)", context),
    stopWssMedia: () => { context.usingWssMedia = false; }, soundController: { detachSink() {} }, clearWssTimers() {},
  });
  const inputFns = source.slice(source.indexOf("function inputControlState("), source.indexOf("\nlet motionFrame"));
  const cleanupPrefix = source.slice(source.indexOf("function cleanupPeer("), source.indexOf("  resetKeyboardDraft();", source.indexOf("function cleanupPeer(")));
  vm.runInContext(inputFns + "\n" + cleanupPrefix + "\n}\ncleanupPeer();", context);
  assert.deepEqual(sent, ["ctl-input"]);
  assert.equal(vm.runInContext("sendInput(data, dc)", context), false);
  assert.equal(vm.runInContext("inputControlState()", context), "down");
});

test("each failure cause gets its own plain text and a log line with the cause", async () => {
  const bodies = new Set(["handshake-failed", "unexpected-message", "socket-ended", "start-failed", "bind-failed", "control-failed"]
    .map(reason => sessionRefusal(reason).body));
  assert.equal(bodies.size, 6, "six distinct causes, six distinct messages");
  assert.ok([...bodies].every(body => /\.$/.test(body) && !/undefined/.test(body)));
  const lines = [];
  const previous = globalThis.BPLog;
  globalThis.BPLog = { log: (level, target, msg, fields) => lines.push({ level, target, msg, fields }) };
  try {
    const h = await harness();
    await h.session.receive({ ...hs.messages.authHost, signature: hs.messages.authFinish.signature });
    assert.equal(h.events.failures[0].reason, "handshake-failed");
    assert.equal(h.events.failures[0].title, "Session ended");
    const line = lines.find(item => item.msg === "session ended");
    assert.equal(line.target, "dk-session");
    assert.equal(line.fields.reason, "handshake-failed");
    assert.ok(line.fields.cause, "the underlying error is logged");
    const pin = await harness({ pinnedPublic: bytes(hs.viewer.public) });
    await pin.session.receive(hs.messages.authHost);
    assert.equal(pin.events.failures[0].reason, "pc-key-changed");
    const stray = await harness();
    await stray.session.receive({ type: "ctl", kind: "x", seq: 0, data: "AA" });
    assert.equal(stray.events.failures[0].reason, "unexpected-message");
  } finally {
    globalThis.BPLog = previous;
  }
});

test("client-logs-v1 is advertised but optional; log messages are delivered only when both sides negotiated it", async () => {
  const off = await harness({ onLogMessage: () => assert.fail("log message without the capability") });
  assert.deepEqual((await off.authenticate()).caps, viewerCaps);
  assert.equal(off.session.logsNegotiated, false);
  await off.session.receive(await off.pcCtl.sealJson({ type: "logs-state", v: 1, cursors: {}, limits: {} }));
  assert.deepEqual(off.events.messages, [], "ignored, and not passed on as an endpoint message either");
  assert.equal(off.events.failures.length, 0);
  off.session.close();

  const got = [];
  const on = await harness({ onLogMessage: message => got.push(message) });
  await on.authenticate([...caps, "client-logs-v1"]);
  assert.equal(on.session.logsNegotiated, true);
  await on.session.receive(await on.pcCtl.sealJson({ type: "logs-state", v: 1, cursors: {}, limits: {} }));
  await on.session.receive(await on.pcCtl.sealJson({ type: "feedback-ack", v: 1, id: "r1", status: "ok", reason: "" }));
  assert.deepEqual(got.map(message => message.type), ["logs-state", "feedback-ack"]);
  assert.deepEqual(on.events.messages, []);
  on.session.close();

  const broken = await harness({ onLogMessage: () => { throw new Error("uploader bug"); } });
  await broken.authenticate([...caps, "client-logs-v1"]);
  await broken.session.receive(await broken.pcCtl.sealJson({ type: "logs-ack", v: 1, stream: "browser", id: 1, status: "ok" }));
  assert.equal(broken.events.failures.length, 0, "an upload failure never ends the session");
  assert.equal(broken.session.controlReady, true);
  broken.session.close();
});
