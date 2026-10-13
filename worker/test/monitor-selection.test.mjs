import assert from "node:assert/strict";
import test from "node:test";

import {
  monitorRequestAfterRestart,
  monitorRequestReadiness,
  restartPeerInNewEpoch,
} from "../public/monitor-selection.js";

test("a native monitor transition recycles signaling before restarting the peer", () => {
  const order = [];
  const reason = restartPeerInNewEpoch({
    recycleSignaling: value => order.push(`recycle:${value}`),
    restartPeer: (value, reconnect) => order.push(`restart:${value}:${reconnect}`),
  }, "monitor-transition");
  assert.equal(reason, "monitor-transition");
  // The host accepts one offer per signaling epoch: restarting the peer first
  // would make the new offer unnegotiable and end the host session.
  assert.deepEqual(order, ["recycle:monitor-transition", "restart:monitor-transition:false"]);
});

test("a native desktop-source transition uses the same epoch order", () => {
  const order = [];
  restartPeerInNewEpoch({
    recycleSignaling: value => order.push(`recycle:${value}`),
    restartPeer: (value, reconnect) => order.push(`restart:${value}:${reconnect}`),
  }, "desktop-source-transition");
  assert.deepEqual(order, ["recycle:desktop-source-transition", "restart:desktop-source-transition:false"]);
});

test("a peer restart in a new epoch requires both operations", () => {
  assert.throws(() => restartPeerInNewEpoch({ recycleSignaling: () => {} }, "peer-left"), TypeError);
  assert.throws(() => restartPeerInNewEpoch({ restartPeer: () => {} }, "peer-left"), TypeError);
});

test("the monitor control only needs an open, registered signaling socket", () => {
  assert.deepEqual(
    monitorRequestReadiness({ socketOpen: true, signalingRegistered: true, pending: null }),
    { allowed: true, reason: null });
  assert.deepEqual(
    monitorRequestReadiness({ socketOpen: false, signalingRegistered: true, pending: null }),
    { allowed: false, reason: "signaling-socket" });
  assert.deepEqual(
    monitorRequestReadiness({ socketOpen: true, signalingRegistered: false, pending: null }),
    { allowed: false, reason: "signaling-registration" });
});

test("input channel readiness is not a precondition for the monitor control", () => {
  const readiness = monitorRequestReadiness({
    socketOpen: true,
    signalingRegistered: true,
    pending: null,
    inputReady: false,
  });
  assert.equal(readiness.allowed, true);
});

test("one outstanding monitor request blocks a second request", () => {
  assert.deepEqual(
    monitorRequestReadiness({ socketOpen: true, signalingRegistered: true, pending: "\\\\.\\DISPLAY5" }),
    { allowed: false, reason: "pending" });
  assert.deepEqual(
    monitorRequestReadiness({ socketOpen: false, signalingRegistered: false, pending: "\\\\.\\DISPLAY5" }),
    { allowed: false, reason: "pending" });
});

test("a monitor confirmation cannot survive an epoch restart", () => {
  assert.deepEqual(
    monitorRequestAfterRestart("\\\\.\\DISPLAY5", "peer-left"),
    { selector: "\\\\.\\DISPLAY5", reason: "peer-left", invalidated: true });
  assert.deepEqual(
    monitorRequestAfterRestart("\\\\.\\DISPLAY5", "monitor-transition"),
    { selector: "\\\\.\\DISPLAY5", reason: "monitor-transition", invalidated: true });
  assert.deepEqual(
    monitorRequestAfterRestart("\\\\.\\DISPLAY5"),
    { selector: "\\\\.\\DISPLAY5", reason: "epoch-restart", invalidated: true });
  assert.equal(monitorRequestAfterRestart(null, "peer-left"), null);
  assert.equal(monitorRequestAfterRestart("", "peer-left"), null);
});

test("follow active window: the client sends the saved choice and shows the host's truth", async () => {
  const { readFileSync } = await import("node:fs");
  const client = readFileSync(new URL("../public/receiver.js", import.meta.url), "utf8").replace(/\r\n/g, "\n");
  // Exact wire shape from docs/architecture/follow-active-window.md.
  assert.match(client, /sendSignal\(\{ type: "media-control", action: "follow", follow: settings\.followWindow, protocol: WSS_PROTOCOL, epoch: mediaEpoch, reason: "user-follow" \}\)/);
  // Capability gate: integer version >= 1; resent once per host-capabilities while on.
  assert.match(client, /hostFollowCapable = Number\.isInteger\(message\.followWindow\) && message\.followWindow >= 1;/);
  assert.match(client, /if \(settings\.followWindow\) sendFollowWindow\(\);/);
  assert.match(client, /case "host-capabilities":[\s\S]*?applyFollowCapability\(msg\);[\s\S]*?break;/);
  // The switch shows monitor-list.follow when present, otherwise the setting.
  assert.match(client, /if \(typeof message\.follow === "boolean"\) hostFollowState = message\.follow;/);
  assert.match(client, /return hostFollowState \?\? settings\.followWindow;/);
  assert.match(client, /group\.hidden = !hostFollowCapable;/);
  assert.match(client, /row\.setAttribute\("aria-checked", String\(followWindowShown\(\)\)\);/);
  // Toggling persists the choice and keeps the popover open.
  assert.match(client, /settings\.followWindow = !followWindowShown\(\);\s*persistSettings\(\);/);
  assert.match(client, /document\.getElementById\("follow-window"\)\.onclick = \(\) => toggleFollowWindow\(\);/);
  // Automatic switches reuse the manual path: no follow-specific desktop-state handling.
  const desktopState = client.match(/function handleDesktopState[\s\S]*?\n}\n/)?.[0];
  assert.ok(desktopState, "handleDesktopState must be extracted from receiver.js");
  // Comments may describe the rule; only code is forbidden to mention follow.
  assert.doesNotMatch(desktopState.replace(/(^|\s)\/\/[^\n]*/gm, "$1"), /follow/i);
});
