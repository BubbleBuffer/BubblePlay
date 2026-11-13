// fixtures/webrtc-offer-contract.json: what the browser peer offers. client-win reads the same
// fixture in its offer test, so the two cannot drift apart.
import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { peerHarness } from "./helpers/peer-harness.mjs";

const contract = JSON.parse(readFileSync(new URL("../../fixtures/webrtc-offer-contract.json", import.meta.url), "utf8"));
const APPLICATION_MEDIA = "application";

test("the peer offers the media order, directions and data channels of the offer contract", async () => {
  const h = peerHarness();
  await h.peer.start();
  const pc = h.pcs[0];
  const kinds = pc.transceivers.map(transceiver => transceiver.kind);
  assert.deepEqual([...kinds, APPLICATION_MEDIA], contract.mediaOrder);
  for (const transceiver of pc.transceivers) assert.equal(transceiver.init.direction, contract.directions[transceiver.kind]);
  const channels = pc.channels.map(channel => ({
    label: channel.label,
    ordered: channel.options.ordered,
    maxRetransmits: channel.options.maxRetransmits ?? null,
  }));
  assert.deepEqual(channels, contract.dataChannels);
});
