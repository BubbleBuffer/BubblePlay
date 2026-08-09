import test from "node:test";
import assert from "node:assert/strict";
import { Room } from "../src/index.js";
import {
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
