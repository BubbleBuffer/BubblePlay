// Media sockets on the native relay: presence, v4 frames, acks, keyframes and pings carried both ways on /media-ws and
// /host-media-ws, and a ticket the relay did not issue.
import { after, before, test } from "node:test";
import assert from "node:assert/strict";
import { encodeFrameV4 } from "../public/media-v4.js";
import {
  CASE_TIMEOUT_MS, admitViewer, closeSocket, loadIdentity, openDataSocket, openHost, startRelay,
} from "./harness.mjs";

let relay = null;
let PC = null;
let VIEWER = null;

before(async () => {
  PC = await loadIdentity("pc");
  VIEWER = await loadIdentity("viewer");
  relay = await startRelay();
});

after(async () => {
  await relay?.stop();
  relay = null;
});

test("media sockets: presence, v4 frames, acks, keyframes and pings flow both ways, and a leaving host shows present:false", { timeout: CASE_TIMEOUT_MS }, async () => {
  const host = await openHost(relay, PC, [VIEWER.id]);
  const viewer = await admitViewer(relay, VIEWER, PC.id);
  const epoch = viewer.hello.mediaEpoch;
  const media = await openDataSocket(relay, "/media-ws", PC.id, epoch, { type: "hello", role: "media-viewer", ticket: viewer.hello.ticket });
  const mediaHost = await openDataSocket(relay, "/host-media-ws", PC.id, epoch, { type: "hello", role: "media-host", ticket: host.ticket });
  try {
    assert.equal((await media.box.take(message => message.type === "hello-ok", "media hello-ok")).type, "hello-ok");
    assert.deepEqual(await media.box.take(message => message.type === "media-peer", "viewer media-peer"), { type: "media-peer", present: false });

    assert.equal((await mediaHost.box.take(message => message.type === "hello-ok", "media host hello-ok")).type, "hello-ok");
    assert.deepEqual(await mediaHost.box.take(message => message.type === "media-peer", "host media-peer"), { type: "media-peer", present: true });
    assert.deepEqual(await media.box.take(message => message.type === "media-peer", "viewer media-peer present"), { type: "media-peer", present: true });

    const packet = encodeFrameV4({ key: true, codec: 1, generation: 1, seq: 1, data: new Uint8Array([0, 0, 0, 1, 0x40]) });
    mediaHost.socket.send(packet);
    const frame = await media.box.take(message => message.binary, "v4 frame");
    assert.deepEqual(frame.binary, Buffer.from(packet));

    const ack = { type: "ack", generation: 1, seq: 1, recvTsMs: 5 };
    media.socket.send(JSON.stringify(ack));
    assert.deepEqual(await mediaHost.box.take(message => message.type === "ack", "ack"), ack);

    const keyframe = { type: "keyframe", reason: "decoder:lost" };
    media.socket.send(JSON.stringify(keyframe));
    assert.deepEqual(await mediaHost.box.take(message => message.type === "keyframe", "keyframe"), keyframe);

    media.socket.send(JSON.stringify({ type: "ping" }));
    assert.deepEqual(await media.box.take(message => message.type === "pong", "viewer pong"), { type: "pong" });
    mediaHost.socket.send(JSON.stringify({ type: "ping" }));
    assert.deepEqual(await mediaHost.box.take(message => message.type === "pong", "host pong"), { type: "pong" });

    await closeSocket(mediaHost.socket);
    assert.deepEqual(await media.box.take(message => message.type === "media-peer", "media-peer after host close"), { type: "media-peer", present: false });
  } finally {
    await closeSocket(mediaHost.socket);
    await closeSocket(media.socket);
    await closeSocket(viewer.socket);
    await closeSocket(host.socket);
  }
});

test("a media socket with a ticket the relay did not issue is closed 1008 with the reason not authorized", { timeout: CASE_TIMEOUT_MS }, async () => {
  const host = await openHost(relay, PC, [VIEWER.id]);
  const viewer = await admitViewer(relay, VIEWER, PC.id);
  const forged = await openDataSocket(relay, "/media-ws", PC.id, viewer.hello.mediaEpoch,
    { type: "hello", role: "media-viewer", ticket: "A".repeat(43) });
  try {
    const close = await forged.box.done;
    assert.equal(close.code, 1008);
    assert.equal(close.reason, "not authorized");
    assert.equal(forged.box.items.some(message => message.type === "hello-ok"), false);
  } finally {
    await closeSocket(forged.socket);
    await closeSocket(viewer.socket);
    await closeSocket(host.socket);
  }
});
