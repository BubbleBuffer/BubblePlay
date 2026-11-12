import test from "node:test";
import assert from "node:assert/strict";
import { describeConnectionStatus, CONNECTION_STATE } from "../public/connection-status.js";

const base = {
  terminalSignalingFailure: false, desktopAvailable: true, usingWssMedia: false, wssStatus: null,
  inputReady: false, ice: "new", ws: "open", desktopKind: "default", inputStatusReason: null,
  rttMs: null, monitorName: null,
};

test("connection chip: state, label and tooltip per transport", () => {
  const wss = status => describeConnectionStatus({ ...base, usingWssMedia: true, wssStatus: status });
  assert.equal(wss("paused-static").state, CONNECTION_STATE.CONNECTED);
  assert.equal(wss("paused-static").detail, "Connected via WSS (static)");
  assert.equal(wss("paused-static").shortLabel, "Connected");
  assert.equal(wss("reconnecting").state, CONNECTION_STATE.CONNECTING);
  assert.equal(wss("reconnecting").shortLabel, "Reconnecting media…");

  const rtc = describeConnectionStatus({ ...base, inputReady: true, ice: "completed", rttMs: 12, monitorName: "Display 1" });
  assert.equal(rtc.state, CONNECTION_STATE.CONNECTED);
  assert.equal(rtc.detail, "Connected via WebRTC · round trip 12 ms · Display 1");

  assert.equal(describeConnectionStatus({ ...base, ws: "error" }).state, CONNECTION_STATE.ERROR);
  assert.equal(describeConnectionStatus({ ...base, desktopAvailable: false }).shortLabel, "Desktop unavailable");
  assert.equal(describeConnectionStatus({ ...base, terminalSignalingFailure: true, ws: "closed" }).shortLabel, "Session unavailable");
});

test("connection chip: Admin warning replaces the short label only for a default desktop", () => {
  const admin = { ...base, inputReady: true, ice: "connected", inputStatusReason: "admin-required" };
  const shown = describeConnectionStatus(admin);
  assert.equal(shown.shortLabel, shown.warning);
  assert.equal(shown.state, CONNECTION_STATE.CONNECTED);
  assert.equal(describeConnectionStatus({ ...admin, desktopKind: "secure" }).warning, "");
});
