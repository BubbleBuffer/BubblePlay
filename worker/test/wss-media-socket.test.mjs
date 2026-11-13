import test from "node:test";
import assert from "node:assert/strict";
import { createWssMediaSocket, MEDIA_HEARTBEAT_TICK_MS } from "../public/wss-media-socket.js";
import { MEDIA_CONNECT_TIMEOUT_MS } from "../public/recovery.js";
import { V4_BACKOFF_MIN_MS, V4_BACKOFF_JITTER_FRACTION, V4_HEARTBEAT_MS } from "../public/media-v4.js";

function harness(over = {}) {
  const sockets = [], timers = [], intervals = [], events = [];
  let nowMs = 1;
  const state = { active: true, epoch: "e", ticket: "t1" };
  class Socket {
    constructor(url) { this.url = url; this.readyState = 0; this.sent = []; this.closed = null; sockets.push(this); }
    send(text) { this.sent.push(text); }
    close(code) { this.closed = code ?? true; this.readyState = 3; }
  }
  const media = createWssMediaSocket({
    active: () => state.active, epoch: () => state.epoch, ticket: () => state.ticket, url: () => "wss://relay.test/media-ws",
    now: () => nowMs, onPacket: p => events.push(["packet", p]), onOpen: o => events.push(["open", o.reconnect]),
    onPeer: p => events.push(["peer", p.present, p.joined]), onClosed: () => events.push(["closed"]),
    log: (level, event) => events.push(["log", event]), trace: () => {},
    makeSocket: url => new Socket(url),
    setTimeout: (fn, delay) => { timers.push({ fn, delay, live: true }); return timers.length - 1; },
    clearTimeout: id => { if (timers[id]) timers[id].live = false; },
    setInterval: fn => { intervals.push(fn); return intervals.length - 1; }, clearInterval() {},
    random: () => 0.5, ...over,
  });
  const message = (socket, data) => socket.onmessage({ data: typeof data === "string" ? data : data });
  return { media, sockets, timers, intervals, events, state, message, advance: ms => { nowMs += ms; } };
}

test("open needs an active session, an epoch and a current ticket; the hello carries the ticket only in the message", () => {
  const h = harness();
  h.state.ticket = null; h.media.open(); assert.equal(h.sockets.length, 0);
  h.state.ticket = "t1"; h.state.epoch = ""; h.media.open(); assert.equal(h.sockets.length, 0);
  h.state.epoch = "e"; h.media.open();
  h.sockets[0].onopen();
  assert.deepEqual(h.sockets[0].sent, [JSON.stringify({ type: "hello", role: "media-viewer", ticket: "t1" })]);
  assert.ok(!h.sockets[0].url.includes("t1"));
  h.state.ticket = "t2";
  h.sockets[0].onopen();
  assert.equal(h.sockets[0].sent.length, 1, "a socket of an obsolete ticket cannot send");
});

test("a socket that never reaches hello-ok is abandoned after the connect timeout and retried on the backoff", () => {
  const h = harness();
  h.media.open();
  const timeout = h.timers.find(t => t.delay === MEDIA_CONNECT_TIMEOUT_MS);
  assert.ok(timeout);
  const first = h.sockets[0];
  timeout.fn();
  assert.ok(first.closed);
  assert.equal(first.onopen, null);
  const retry = h.timers.at(-1);
  assert.equal(retry.delay, Math.round(V4_BACKOFF_MIN_MS * (1 + V4_BACKOFF_JITTER_FRACTION * 0)), "jitter at the middle of its range is zero");
  assert.equal(h.media.reconnecting, true);
  retry.fn();
  assert.equal(h.sockets.length, 2);
  assert.equal(h.media.reconnects, 1);
});

test("hello-ok opens the session, reports reconnects, and a close schedules a reconnect", () => {
  const h = harness();
  h.media.open();
  h.message(h.sockets[0], JSON.stringify({ type: "hello-ok" }));
  assert.equal(h.media.isOpen, true);
  h.message(h.sockets[0], JSON.stringify({ type: "media-peer", present: true }));
  h.message(h.sockets[0], JSON.stringify({ type: "media-peer", present: true }));
  h.message(h.sockets[0], new ArrayBuffer(4));
  h.sockets[0].onclose({ code: 1006 });
  assert.equal(h.media.isOpen, false);
  h.timers.at(-1).fn();
  h.message(h.sockets[1], JSON.stringify({ type: "hello-ok" }));
  assert.deepEqual(h.events.filter(e => e[0] !== "log").map(e => e.join(":")),
    ["open:false", "peer:true:true", "peer:true:false", "packet:[object ArrayBuffer]", "closed", "open:true"]);
  assert.ok(h.events.some(e => e[1] === "wss-v4-reconnected"));
});

test("heartbeat pings on its own clock and closes a socket that stays silent", () => {
  const h = harness();
  h.media.start();
  h.message(h.sockets[0], JSON.stringify({ type: "hello-ok" }));
  h.sockets[0].onopen();
  h.sockets[0].readyState = 1;
  const tick = h.intervals[0];
  h.advance(V4_HEARTBEAT_MS); tick();
  assert.ok(h.sockets[0].sent.includes('{"type":"ping"}'));
  h.advance(10 * V4_HEARTBEAT_MS); tick();
  assert.ok(h.sockets[0].closed, "no inbound traffic after the ping: closed");
  assert.ok(h.events.some(e => e[1] === "wss-v4-silent-socket"));
  assert.ok(MEDIA_HEARTBEAT_TICK_MS < V4_HEARTBEAT_MS);
});
