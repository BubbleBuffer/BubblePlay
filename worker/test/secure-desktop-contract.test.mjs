import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { Room } from "../src/index.js";
import { validDesktopState, validSecureAction, validSecureActionResult } from "../src/protocol-validation.js";

const contract = JSON.parse(readFileSync(new URL("../../fixtures/secure-desktop-contract.json", import.meta.url), "utf8"));
const valid = (message, role, raw = JSON.stringify(message)) => {
  const validator = { "desktop-state": validDesktopState, "secure-action": validSecureAction,
    "secure-action-result": validSecureActionResult }[message.type];
  return validator(message, role, raw);
};
const accepted = [
  ...contract.desktopStates.map(message => ({ role: "host", message })),
  { role: "host", message: contract.recoveredState },
  ...contract.secureActions.map(message => ({ role: "client", message })),
  ...contract.secureActionResults.map(message => ({ role: "host", message })),
];

test("secure desktop contract preserves legacy states and six helper permission states", () => {
  for (const { role, message } of accepted) assert.equal(valid(message, role), true, JSON.stringify(message));
  const legacy = { type: "desktop-state", state: "unavailable", generation: 2, reason: "capture-unavailable" };
  assert.equal(valid(legacy, "host"), true);
  assert.equal(contract.policyCases.length, 6);
  for (const policy of contract.policyCases) {
    const state = contract.desktopStates[policy.desktopStateIndex];
    assert.equal(state.state, "active");
    assert.equal(state.desktop, "secure");
    assert.equal(state.secureKind, policy.secureKind);
    assert.deepEqual(state.secureCapabilities,
      ["view", ...(policy.secureKind === "uac" ? ["deny"] : []), ...(policy.adminMode ? ["input"] : [])]);
    assert.equal(policy.inputAccepted, policy.adminMode);
    assert.equal(policy.denyAllowed, policy.secureKind === "uac");
  }
});

test("secure desktop validators reject parameters, unknown values, duplicate capabilities and wrong roles", () => {
  for (const { name, role, message } of contract.invalidMessages) assert.equal(valid(message, role), false, name);
  for (const { role, message, accepted } of contract.roleCases) assert.equal(valid(message, role), accepted, `${role}: ${message.type}`);
});

test("secure desktop wire and reason limits count UTF-8 bytes", () => {
  for (const { role, message } of accepted) {
    const raw = JSON.stringify(message);
    const padded = raw + " ".repeat(contract.limits.messageMaxBytes - Buffer.byteLength(raw));
    assert.equal(valid(message, role, padded), true);
    assert.equal(valid(message, role, padded + " "), false);
  }
  for (const message of [contract.desktopStates[0], contract.secureActionResults[0]]) {
    assert.equal(valid({ ...message, reason: "é".repeat(48) }, "host"), true);
    assert.equal(valid({ ...message, reason: "é".repeat(49) }, "host"), false);
  }
});

class FakeSocket {
  constructor(role) { this.attachment = { role }; this.readyState = 1; this.messages = []; this.closed = []; }
  deserializeAttachment() { return this.attachment; }
  serializeAttachment(value) { this.attachment = value; }
  send(message) { this.messages.push(message); }
  close(code, reason) { this.closed.push({ code, reason }); }
}

async function relay(role, message, raw = JSON.stringify(message)) {
  const sender = new FakeSocket(role), host = new FakeSocket("host"), client = new FakeSocket("client");
  const sockets = role === "host" ? [sender, client] : role === "client" ? [sender, host] : [sender, host, client];
  const storage = { get: async () => [], put: async () => {}, delete: async () => {} };
  const room = new Room({ getWebSockets: () => sockets, storage }, { TOKEN: "test" });
  await room.webSocketMessage(sender, raw);
  return sockets.filter(socket => socket !== sender).flatMap(socket => socket.messages);
}

test("active Worker Room applies secure desktop validators before forwarding", async () => {
  for (const { role, message } of accepted) {
    const raw = JSON.stringify(message);
    assert.deepEqual(await relay(role, message), [raw]);
    assert.deepEqual(await relay(role === "host" ? "client" : "host", message), []);
    assert.deepEqual(await relay(role, message, raw + " ".repeat(513 - Buffer.byteLength(raw))), []);
  }
  for (const { name, role, message } of contract.invalidMessages) assert.deepEqual(await relay(role, message), [], name);
  // Same generation announces changes in kind and capabilities; the relay
  // must forward both instead of treating generation as a deduplication key.
  const host = new FakeSocket("host"), client = new FakeSocket("client");
  const room = new Room({ getWebSockets: () => [host, client], storage: {} }, { TOKEN: "test" });
  for (const message of contract.desktopStates.slice(3)) await room.webSocketMessage(host, JSON.stringify(message));
  assert.deepEqual(client.messages, contract.desktopStates.slice(3).map(message => JSON.stringify(message)));
});
