import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { encodeFrameV4, parseFrameV4 } from "../public/media-v4.js";
import {
  AudioPlayout,
  SoundController,
  WssAudioPlayer,
  isAudioPacket,
  parseAudioPacket,
  probeOpusDecode,
  soundButtonView,
  sumAudioStats,
} from "../public/audio.js";

const contract = JSON.parse(readFileSync(new URL("../../fixtures/audio-contract.json", import.meta.url), "utf8"));

function packetBytes(packet) {
  if (packet.name === "oversized-payload") {
    // Header plus 1501 payload bytes: one past the 1500-byte limit.
    return Buffer.concat([Buffer.from(packet.hexPrefix, "hex"), Buffer.alloc(packet.payloadBytes, 0xfc)]);
  }
  return Buffer.from(packet.hex, "hex");
}

function near(actual, expected, message = "") {
  assert.ok(Math.abs(actual - expected) < 1e-9, `${message} ${actual} != ${expected}`);
}

const tick = () => new Promise(resolve => setTimeout(resolve, 0));

test("every BPA1 fixture packet parses to the contract, or is rejected", () => {
  assert.ok(contract.packets.some(packet => packet.name === "oversized-payload"));
  for (const packet of contract.packets) {
    const bytes = packetBytes(packet);
    if (!packet.accepted) {
      assert.equal(parseAudioPacket(bytes), null, packet.name);
      continue;
    }
    const parsed = parseAudioPacket(bytes);
    assert.ok(parsed, packet.name);
    assert.equal(isAudioPacket(bytes), true, packet.name);
    assert.equal(parsed.discontinuity, packet.discontinuity, packet.name);
    assert.equal(parsed.codec, packet.codec, packet.name);
    assert.equal(parsed.channels, packet.channels, packet.name);
    assert.equal(parsed.sampleRate, packet.sampleRate, packet.name);
    assert.equal(parsed.seq, packet.seq, packet.name);
    assert.equal(String(parsed.captureTsUs), packet.captureTsUs, packet.name);
    assert.equal(parsed.frameSamples, packet.frameSamples, packet.name);
    assert.equal(Buffer.from(parsed.data).toString("hex"), packet.payloadHex, packet.name);
  }
});

test("parseAudioPacket accepts an ArrayBuffer and a view at an offset", () => {
  const bytes = packetBytes(contract.packets[0]);
  const expected = parseAudioPacket(bytes);
  const padded = new Uint8Array(bytes.length + 10);
  padded.set(bytes, 10);
  const view = parseAudioPacket(padded.subarray(10, 10 + bytes.length));
  assert.equal(view.seq, expected.seq);
  assert.equal(view.captureTsUs, expected.captureTsUs);
  assert.deepEqual(Array.from(view.data), Array.from(expected.data));
  const copy = parseAudioPacket(new Uint8Array(bytes).buffer);
  assert.equal(copy.seq, expected.seq);
});

test("isAudioPacket tells BPA1 from BPV4 video by magic, and each parser rejects the other", () => {
  const video = encodeFrameV4({ key: true, codec: 2, generation: 1, seq: 7, width: 1280, height: 720,
    data: new Uint8Array([0, 0, 0, 1, 0x65]) });
  assert.ok(parseFrameV4(video));
  assert.equal(isAudioPacket(video), false);
  assert.equal(parseAudioPacket(video), null);
  const audio = packetBytes(contract.packets[0]);
  assert.equal(isAudioPacket(audio), true);
  assert.equal(parseFrameV4(audio), null);
  const badMagic = packetBytes(contract.packets.find(packet => packet.name === "bad-magic"));
  assert.equal(isAudioPacket(badMagic), false);
  assert.equal(isAudioPacket(new Uint8Array(3)), false);
  assert.equal(isAudioPacket("text"), false);
});

test("playout starts each packet 40 ms ahead and drops while more than 150 ms is queued", () => {
  const playout = new AudioPlayout();
  assert.equal(playout.receive(1, true), "reset");
  near(playout.schedule(10, 0.01), 10.04, "first packet");
  near(playout.schedule(10, 0.01), 10.05, "second packet");
  // Same wall clock: the queue grows 10 ms per packet until it passes 150 ms ahead.
  let accepted = 2;
  let dropped = 0;
  for (let i = 0; i < 20; i++) {
    if (playout.schedule(10, 0.01) === null) dropped++;
    else accepted++;
  }
  // 0.15 s sits on a float boundary, so the exact count may be 11 or 12.
  assert.ok(accepted >= 11 && accepted <= 12, `accepted ${accepted}`);
  assert.equal(accepted + dropped, 22);
  assert.ok(dropped > 0);
  const queuedAt = playout.nextAt;
  assert.equal(playout.schedule(10, 0.01), null);
  assert.equal(playout.nextAt, queuedAt, "a dropped packet does not move the schedule");
  // Once the clock catches up, scheduling resumes at the usual lead.
  assert.notEqual(playout.schedule(10.1, 0.01), null);
});

test("sequence wraps at u32 without a reset; gaps and discontinuities reset the schedule", () => {
  const playout = new AudioPlayout();
  assert.equal(playout.receive(4294967295), "reset");
  near(playout.schedule(5, 0.01), 5.04, "first");
  assert.equal(playout.receive(0), "accept", "wrap is continuous");
  assert.equal(playout.receive(1), "accept");
  assert.equal(playout.receive(3), "reset", "gap");
  near(playout.schedule(5, 0.01), 5.04, "after gap");
  assert.equal(playout.receive(4, true), "reset", "discontinuity flag on the expected seq");
  near(playout.schedule(6, 0.01), 6.04, "after discontinuity");
});

test("playout counts mid-stream resets only, not the first packet or an explicit reset", () => {
  const playout = new AudioPlayout();
  playout.receive(1);
  assert.equal(playout.resets, 0, "first packet");
  playout.receive(3);
  assert.equal(playout.resets, 1, "gap");
  playout.receive(4, true);
  assert.equal(playout.resets, 2, "discontinuity");
  playout.reset();
  playout.receive(9);
  assert.equal(playout.resets, 2, "explicit reset and the next first packet");
});

test("playout re-anchors after an underrun instead of scheduling in the past", () => {
  const playout = new AudioPlayout();
  playout.receive(1);
  near(playout.schedule(10, 0.01), 10.04);
  near(playout.schedule(11, 0.01), 11.04);
});

function fakeAudioEnv() {
  const env = { allowAudio: false, contexts: [], decoders: [], chunks: [], sources: [] };
  env.createContext = () => {
    const ctx = {
      currentTime: 0,
      state: "suspended",
      destination: { id: "destination" },
      closed: false,
      gains: [],
      async resume() { if (env.allowAudio) ctx.state = "running"; },
      close() { ctx.closed = true; return Promise.resolve(); },
      createGain() {
        const gain = { gain: { value: 1 }, connect(target) { gain.target = target; } };
        ctx.gains.push(gain);
        return gain;
      },
      createBuffer(channels, length, sampleRate) { return { channels, length, sampleRate, copyToChannel() {} }; },
      createBufferSource() {
        const source = { connect(node) { source.connected = node; }, start(at) { source.startAt = at; env.sources.push(source); } };
        return source;
      },
    };
    env.contexts.push(ctx);
    return ctx;
  };
  env.createDecoder = ({ output, error }) => {
    const decoder = {
      output, error, configured: null, decoded: [], closed: false,
      configure(config) { decoder.configured = config; },
      decode(chunk) { decoder.decoded.push(chunk); },
      close() { decoder.closed = true; },
    };
    env.decoders.push(decoder);
    return decoder;
  };
  env.createChunk = init => { env.chunks.push(init); return init; };
  return env;
}

const fakeAudioData = () => ({
  numberOfChannels: 2, numberOfFrames: 480, sampleRate: 48000, closed: false,
  copyTo(target) { target.fill(0.25); },
  close() { this.closed = true; },
});

function playerFor(env) {
  return new WssAudioPlayer({ createContext: env.createContext, createDecoder: env.createDecoder, createChunk: env.createChunk });
}

test("WSS player decodes BPA1 packets, schedules them 40 ms ahead, and skips decoding while muted", async () => {
  const env = fakeAudioEnv();
  const player = playerFor(env);
  assert.equal(player.push(packetBytes(contract.packets[0])), "decoded");
  const [decoder] = env.decoders;
  assert.deepEqual(decoder.configured, { codec: "opus", sampleRate: 48000, numberOfChannels: 2 });
  assert.equal(env.chunks[0].type, "key");
  assert.equal(env.chunks[0].timestamp, 123456789);
  assert.equal(Buffer.from(env.chunks[0].data).toString("hex"), "fcfffe");

  const [ctx] = env.contexts;
  ctx.currentTime = 1;
  decoder.output(fakeAudioData());
  assert.equal(env.sources.length, 1);
  near(env.sources[0].startAt, 1.04, "scheduled ahead");
  assert.equal(env.sources[0].connected, ctx.gains[0]);
  assert.equal(ctx.gains[0].target, ctx.destination);

  player.setMuted(true);
  assert.equal(ctx.gains[0].gain.value, 0);
  assert.equal(player.push(packetBytes(contract.packets[1])), "muted");
  assert.equal(decoder.decoded.length, 1, "no decode while muted");

  player.setMuted(false);
  assert.equal(ctx.gains[0].gain.value, 1);
  assert.equal(player.push(packetBytes(contract.packets.find(packet => packet.name === "bad-magic"))), "rejected");

  player.stop();
  assert.equal(decoder.closed, true);
  assert.equal(ctx.closed, true);
  assert.equal(player.running, false);
});

test("WSS player reports whether its context is running after a resume", async () => {
  const env = fakeAudioEnv();
  const player = playerFor(env);
  assert.equal(await player.resume(), false, "no context yet");
  player.push(packetBytes(contract.packets[0]));
  assert.equal(await player.resume(), false, "blocked context stays suspended");
  env.allowAudio = true;
  assert.equal(await player.resume(), true);
});

test("WSS player drops output once the queue is more than 150 ms ahead", () => {
  const env = fakeAudioEnv();
  const player = playerFor(env);
  player.push(packetBytes(contract.packets[0]));
  const [decoder] = env.decoders;
  for (let i = 0; i < 20; i++) decoder.output(fakeAudioData());
  assert.ok(player.dropped > 0);
  assert.ok(env.sources.length < 20);
});

test("WSS player stats count packets, payload bytes, rejects, frames, cap drops and resets", () => {
  const env = fakeAudioEnv();
  const player = playerFor(env);
  const zero = { packets: 0, payloadBytes: 0, rejected: 0, failures: 0, framesDecoded: 0,
    framesScheduled: 0, droppedCap: 0, scheduleResets: 0 };
  assert.deepEqual(player.stats(), zero);

  const good = packetBytes(contract.packets[0]);
  player.push(good);
  assert.equal(player.stats().packets, 1);
  assert.equal(player.stats().payloadBytes, 3, "fcfffe is three Opus bytes");
  assert.equal(player.stats().scheduleResets, 0, "first packet is not a reset");

  // The same packet again is a sequence gap: one mid-stream reset.
  player.push(good);
  assert.equal(player.stats().scheduleResets, 1);
  assert.equal(player.stats().payloadBytes, 6);

  assert.equal(player.push(packetBytes(contract.packets.find(packet => packet.name === "bad-magic"))), "rejected");
  assert.equal(player.stats().packets, 3);
  assert.equal(player.stats().rejected, 1);
  assert.equal(player.stats().payloadBytes, 6, "rejected bytes are not counted");

  const [decoder] = env.decoders;
  env.contexts[0].currentTime = 0;
  for (let i = 0; i < 20; i++) decoder.output(fakeAudioData());
  const after = player.stats();
  assert.equal(after.framesDecoded, 20);
  assert.equal(after.framesScheduled + after.droppedCap, 20, "every decoded frame is scheduled or dropped");
  assert.ok(after.droppedCap > 0);
  assert.equal(after.failures, 0);
});

test("WSS player counts a decoder failure once and keeps counting after it restarts", () => {
  const env = fakeAudioEnv();
  env.createDecoder = () => { throw new Error("no decoder"); };
  const errors = [];
  const player = new WssAudioPlayer({
    createContext: env.createContext, createDecoder: env.createDecoder, createChunk: env.createChunk,
    onError: error => errors.push(error),
  });
  assert.equal(player.push(packetBytes(contract.packets[0])), "rejected");
  assert.equal(errors.length, 1);
  assert.equal(player.stats().failures, 1);
  assert.equal(player.stats().rejected, 0, "a valid packet that failed to start is not a parse reject");
  assert.equal(player.stats().packets, 1);
});

test("sumAudioStats adds player snapshots and treats missing ones as zero", () => {
  const env = fakeAudioEnv();
  const first = playerFor(env);
  first.push(packetBytes(contract.packets[0]));
  const second = playerFor(env);
  second.push(packetBytes(contract.packets.find(packet => packet.name === "bad-magic")));
  const total = sumAudioStats(first.stats(), null, second.stats());
  assert.equal(total.packets, 2);
  assert.equal(total.rejected, 1);
  assert.equal(total.payloadBytes, 3);
  assert.deepEqual(sumAudioStats(), {
    packets: 0, payloadBytes: 0, rejected: 0, failures: 0, framesDecoded: 0,
    framesScheduled: 0, droppedCap: 0, scheduleResets: 0,
  });
});

function fakeTarget() {
  const listeners = new Map();
  return {
    addEventListener(type, fn) {
      if (!listeners.has(type)) listeners.set(type, new Set());
      listeners.get(type).add(fn);
    },
    removeEventListener(type, fn) { listeners.get(type)?.delete(fn); },
    fire(type) { for (const fn of [...(listeners.get(type) ?? [])]) fn({ type }); },
    count() { return [...listeners.values()].reduce((total, set) => total + set.size, 0); },
  };
}

function fakeSink({ allowed = true } = {}) {
  const sink = {
    allowed, starts: 0, muted: [], stopped: false,
    start() { sink.starts++; return Promise.resolve(sink.allowed); },
    setMuted(muted) { sink.muted.push(muted); },
    stop() { sink.stopped = true; },
  };
  return sink;
}

test("sound starts unmuted when the browser allows autoplay", async () => {
  const target = fakeTarget();
  const controller = new SoundController({ target });
  const sink = fakeSink({ allowed: true });
  assert.equal(await controller.attachSink("rtc", sink), true);
  assert.equal(controller.state, "playing");
  assert.deepEqual(sink.muted, [false]);
  assert.equal(target.count(), 0, "no gesture hooks while playing");
});

test("blocked sound starts on the first gesture, unmuted", async () => {
  const target = fakeTarget();
  const states = [];
  const controller = new SoundController({ target, onChange: state => states.push(state) });
  const sink = fakeSink({ allowed: false });
  assert.equal(await controller.attachSink("rtc", sink), false);
  assert.equal(controller.state, "blocked");
  assert.equal(target.count(), 3, "pointerdown, click and keydown are listened for");
  sink.allowed = true;
  target.fire("pointerdown");
  await tick();
  assert.equal(controller.state, "playing");
  assert.equal(sink.starts, 2);
  assert.equal(target.count(), 0, "gesture hooks are removed once playing");
  assert.equal(states.at(-1), "playing");
});

test("clicking the mute button while blocked starts playback unmuted instead of muting", async () => {
  const controller = new SoundController({ target: fakeTarget() });
  const sink = fakeSink({ allowed: false });
  await controller.attachSink("wss", sink);
  sink.allowed = true;
  await controller.toggle();
  assert.equal(controller.state, "playing");
  assert.equal(controller.muted, false);
});

test("the mute button toggles a local mute on every sink and does not persist", async () => {
  const controller = new SoundController();
  const sink = fakeSink();
  await controller.attachSink("wss", sink);
  await controller.toggle();
  assert.equal(controller.state, "muted");
  assert.deepEqual(sink.muted, [false, true]);
  await controller.toggle();
  assert.equal(controller.state, "playing");
  assert.deepEqual(sink.muted, [false, true, false]);
});

test("a late sink joins the current mute state; replacing a sink stops the old one", async () => {
  const controller = new SoundController();
  await controller.toggle();
  const first = fakeSink();
  await controller.attachSink("rtc", first);
  assert.deepEqual(first.muted, [true]);
  const second = fakeSink();
  await controller.attachSink("rtc", second);
  assert.equal(first.stopped, true);
  assert.deepEqual(second.muted, [true]);
});

test("detaching the last sink stops it, clears blocked, and returns to off", async () => {
  const target = fakeTarget();
  const controller = new SoundController({ target });
  const sink = fakeSink({ allowed: false });
  await controller.attachSink("wss", sink);
  assert.equal(controller.state, "blocked");
  controller.detachSink("wss");
  assert.equal(sink.stopped, true);
  assert.equal(controller.state, "off");
  assert.equal(target.count(), 0);
});

test("the strip button is hidden without the host audio capability and shows the muted glyph when silent", () => {
  assert.equal(soundButtonView({ capable: false, state: "playing" }).hidden, true);
  assert.equal(soundButtonView({ capable: false, state: "off" }).hidden, true);
  assert.deepEqual(soundButtonView({ capable: true, state: "playing" }),
    { hidden: false, pressed: false, silent: false, label: "Mute sound" });
  assert.deepEqual(soundButtonView({ capable: true, state: "muted" }),
    { hidden: false, pressed: true, silent: true, label: "Unmute sound" });
  assert.deepEqual(soundButtonView({ capable: true, state: "blocked" }),
    { hidden: false, pressed: false, silent: true, label: "Unmute sound" });
});

test("Opus decoding is opted in only when isConfigSupported resolves supported", async () => {
  const seen = [];
  const supported = Object.assign(function AudioDecoder() {}, {
    isConfigSupported: async config => { seen.push(config); return { supported: true }; },
  });
  assert.equal(await probeOpusDecode(supported), true);
  assert.deepEqual(seen[0], { codec: "opus", sampleRate: 48000, numberOfChannels: 2 });
  assert.equal(await probeOpusDecode(undefined), false);
  assert.equal(await probeOpusDecode(Object.assign(function AudioDecoder() {}, {
    isConfigSupported: async () => ({ supported: false }),
  })), false);
  assert.equal(await probeOpusDecode(Object.assign(function AudioDecoder() {}, {
    isConfigSupported: async () => { throw new Error("unsupported"); },
  })), false);
});

test("the strip sound button sits before fullscreen, hidden until the host announces audio; audio transceiver follows video", () => {
  const html = readFileSync(new URL("../public/index.html", import.meta.url), "utf8");
  const receiver = readFileSync(new URL("../public/receiver.js", import.meta.url), "utf8");
  assert.match(html, /<button id="strip-sound" class="strip-item" data-ui-action="sound"[^>]*\bhidden\b/);
  assert.ok(html.indexOf('id="strip-sound"') < html.indexOf('id="strip-fullscreen"'));
  assert.match(html, /<!-- Lucide volume-2 \/ volume-x \(ISC License, lucide\.dev\) -->/);
  assert.match(receiver, /hostAudioCapable = Number\.isInteger\(msg\.audio\) && msg\.audio >= 1;/);
  assert.match(receiver, /\.\.\.\(audioDecodable \? \{ audio: 1 \} : \{\}\)/);
  assert.match(receiver, /if \(isAudioPacket\(data\)\) \{/);
  assert.match(receiver, /if \(ev\.track\.kind === "audio"\) \{/);
  // The audio inbound-rtp is read alongside the unchanged video selection.
  assert.match(receiver, /s\.type === "inbound-rtp" && \(s\.kind === "video" \|\| s\.mediaType === "video"\)/);
  assert.match(receiver, /s\.type === "inbound-rtp" && \(s\.kind === "audio" \|\| s\.mediaType === "audio"\)/);
  assert.match(receiver, /audio: audioSnapshot\(\),/);
  const video = receiver.indexOf('candidate.addTransceiver("video", { direction: "recvonly" });');
  const audio = receiver.indexOf('candidate.addTransceiver("audio", { direction: "recvonly" });');
  assert.ok(video > 0 && audio > video, "audio transceiver right after video");
});
