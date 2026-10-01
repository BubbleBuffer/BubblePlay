import test from "node:test";
import assert from "node:assert/strict";
import {
  STUN_FALLBACK,
  deriveCandidateRoute,
  parseIceTransportPolicy,
  validateIceServers,
  wantsTurn,
} from "../public/turn.js";

test("rejects malformed, malicious, and oversized signaled ICE payloads to STUN-only", () => {
  const cases = [
    undefined,
    [{ urls: "javascript:alert(1)" }],
    [{ urls: "stun:" }],
    [{ urls: "turn:", username: "user", credential: "signed" }],
    [{ urls: "turn:relay.example?transport=sctp", username: "user", credential: "signed" }],
    [{ urls: "turn:relay.example\n?transport=udp", username: "user", credential: "signed" }],
    [{ urls: "turn:relay.example", username: "user" }],
    Array.from({ length: 5 }, () => ({ urls: "stun:stun.example" })),
    [{ urls: "turn:relay.example", username: "u", credential: "x".repeat(513) }],
  ];
  for (const value of cases) assert.deepEqual(validateIceServers(value), STUN_FALLBACK);
});

test("normalizes a bounded safe TURN/STUN payload without retaining extra fields", () => {
  const normalized = validateIceServers([
    { urls: "stun:stun.example", ignored: "discard" },
    { urls: ["turn:relay.example?transport=udp", "turns:relay.example"], username: "600:user", credential: "signed", ignored: true },
  ]);
  assert.deepEqual(normalized, [
    { urls: "stun:stun.example" },
    { urls: ["turn:relay.example?transport=udp", "turns:relay.example"], username: "600:user", credential: "signed" },
  ]);
});

test("defaults ICE transport policy to all and only relay query forces relay", () => {
  assert.equal(parseIceTransportPolicy("?ice=relay"), "relay");
  assert.equal(parseIceTransportPolicy("?ice=all"), "all");
  assert.equal(parseIceTransportPolicy("?anything=relay"), "all");
});

test("TURN is requested only by ?turn=1 or a relay-only ICE policy", () => {
  assert.equal(wantsTurn(""), false);
  assert.equal(wantsTurn("?transport=webrtc"), false);
  assert.equal(wantsTurn("?turn=0"), false);
  assert.equal(wantsTurn("?turn=1"), true);
  assert.equal(wantsTurn("?ice=relay"), true);
});

test("derives a type-only candidate route without address-like stat values", () => {
  assert.equal(deriveCandidateRoute({ candidatePair: { local: { candidateType: "relay", address: "192.0.2.1", port: 3478 }, remote: { candidateType: "srflx", ip: "198.51.100.8", usernameFragment: "secret" } } }), "relay->srflx");
  assert.equal(deriveCandidateRoute({ candidatePair: { local: { candidateType: "unknown" }, remote: { candidateType: "host" } } }), "…");
  assert.equal(deriveCandidateRoute(null), "…");
});
