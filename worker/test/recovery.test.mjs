import test from "node:test";
import assert from "node:assert/strict";
import { Room } from "../src/index.js";
import {
  shouldRecoverAfterNetworkRouteProbe,
  shouldReconnectAfterClose,
  shouldRecycleSignalingForPeerState,
} from "../public/recovery.js";

function socket(role, { replaced = false, readyState = 1 } = {}) {
  return { readyState, deserializeAttachment: () => ({ role, replaced }) };
}

test("relay selects the current open peer rather than a replaced or closing socket", () => {
  const replaced = socket("host", { replaced: true });
  const closing = socket("host", { readyState: 2 });
  const current = socket("host");
  const room = new Room({ getWebSockets: () => [replaced, closing, current] }, {});
  assert.equal(room.findPeer("host"), current);
});

test("policy rejection is terminal but abnormal websocket closure reconnects", () => {
  assert.equal(shouldReconnectAfterClose(1008, false), false);
  assert.equal(shouldReconnectAfterClose(1006, false), true);
  assert.equal(shouldReconnectAfterClose(1006, true), false);
});

test("a failed peer transport recycles signaling so both ends get a fresh epoch", () => {
  assert.equal(shouldRecycleSignalingForPeerState("failed"), true);
  assert.equal(shouldRecycleSignalingForPeerState("connected"), false);
  assert.equal(shouldRecycleSignalingForPeerState("closed"), false);
});

test("Network Information estimate changes do not tear down a healthy WSS cohort", () => {
  const before = {
    signalingGeneration: 7,
    receivedPackets: 5_018,
    reconstructed: 403,
    presented: 403,
  };
  const healthy = {
    ...before,
    online: true,
    signalingRegistered: true,
    signalingSocketOpen: true,
    usingWssMedia: true,
    openLanes: 16,
    dataShards: 15,
    recoveryState: "healthy",
    renderedAgeMs: 203,
  };
  assert.equal(shouldRecoverAfterNetworkRouteProbe(before, healthy), false);
  assert.equal(shouldRecoverAfterNetworkRouteProbe(before, {
    ...healthy,
    receivedPackets: before.receivedPackets + 16,
    reconstructed: before.reconstructed + 1,
    presented: before.presented + 1,
  }), false);
});

test("a Network Information probe still recovers a stalled or degraded WSS route", () => {
  const before = {
    signalingGeneration: 9,
    receivedPackets: 1_600,
    reconstructed: 100,
    presented: 100,
  };
  const stalled = {
    ...before,
    online: true,
    signalingRegistered: true,
    signalingSocketOpen: true,
    usingWssMedia: true,
    openLanes: 16,
    dataShards: 15,
    recoveryState: "healthy",
    renderedAgeMs: 1_501,
  };
  assert.equal(shouldRecoverAfterNetworkRouteProbe(before, stalled), true);
  assert.equal(shouldRecoverAfterNetworkRouteProbe(before, {
    ...stalled,
    renderedAgeMs: 200,
    openLanes: 14,
  }), true);
  assert.equal(shouldRecoverAfterNetworkRouteProbe(before, {
    ...stalled,
    online: false,
  }), false);
});

test("a lab quality pause is not mistaken for a dead healthy WSS route", () => {
  const before = {
    signalingGeneration: 11,
    receivedPackets: 8_128,
    reconstructed: 828,
    presented: 828,
  };
  const paused = {
    ...before,
    online: true,
    signalingRegistered: true,
    signalingSocketOpen: true,
    usingWssMedia: true,
    openLanes: 16,
    dataShards: 15,
    recoveryState: "healthy",
    renderedAgeMs: 10_571,
    allowQuiescentIdle: true,
  };

  assert.equal(shouldRecoverAfterNetworkRouteProbe(before, paused), false);
  assert.equal(shouldRecoverAfterNetworkRouteProbe(before, { ...paused, openLanes: 14 }), true);
  assert.equal(shouldRecoverAfterNetworkRouteProbe(before, {
    ...paused,
    recoveryState: "awaiting-keyframe",
  }), true);
});
