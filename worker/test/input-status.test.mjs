import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";
import { describeConnectionStatus } from "../public/connection-status.js";

const receipt = { type: "input-status", protocol: 1, generation: 4,
  receivedPackets: 12, acceptedPackets: 11, failedPackets: 1, droppedPackets: 1,
  releasedInputs: 2, requestedEvents: 14, appliedEvents: 13 };

test("browser shows and clears Admin warnings without changing input readiness", () => {
  const source = readFileSync(new URL("../public/receiver.js", import.meta.url), "utf8").replace(/\r\n/g, "\n");
  const renderer = source.match(/function renderConnectionStatus\(\) \{[\s\S]*?\n\}/)?.[0];
  const handler = source.match(/case "input-status": \{([\s\S]*?)\n    \}\n    case/)?.[1];
  assert.ok(renderer); assert.ok(handler);
  const classes = new Map();
  const retry = { hidden: true };
  const logs = [];
  const context = vm.createContext({
    latestInputStatus: null, inputStatusReason: null, connectionStatusDetail: "",
    desktopAvailable: true, desktopKind: "default", terminalSignalingFailure: false,
    usingWssMedia: false, inputReady: true, hudState: { ice: "connected", ws: "open", rtt: "…" },
    monitorList: [], connectionStatus: { dataset: {}, title: "" }, connectionStatusLabel: { textContent: "" },
    document: { getElementById: id => id === "connection-retry" ? retry : { classList: { toggle: (key, value) => classes.set(key, value) } } },
    viewerLog: (...args) => logs.push(args), describeConnectionStatus,
  });
  vm.runInContext(renderer + `\nfunction receive(msg) { switch(msg.type) { case "input-status": { ${handler} } } }`, context);
  const receive = fields => { context.msg = { ...receipt, ...fields }; vm.runInContext("receive(msg)", context); };
  receive({ reason: "admin-required", route: "local" });
  assert.equal(context.connectionStatusLabel.textContent, "This window needs Admin mode — turn it on at the PC");
  assert.equal(classes.get("status-warning"), true);
  assert.equal(retry.hidden, true);
  assert.equal(context.inputReady, true);
  const savedReceipt = context.latestInputStatus;
  receive({ reason: "admin-required", route: "local", receivedPackets: 0, acceptedPackets: 0,
    failedPackets: 0, droppedPackets: 0, releasedInputs: 0, requestedEvents: 0, appliedEvents: 0 });
  assert.equal(context.latestInputStatus, savedReceipt);
  const logCount = logs.length;
  receive({}); // receipt-only messages do not erase the foreground warning
  assert.equal(logs.length, logCount); // cumulative failures are not reported twice
  assert.equal(context.inputStatusReason, "admin-required");
  receive({ reason: "invalid" });
  receive({ reason: null, route: "invalid" });
  assert.equal(context.inputStatusReason, "admin-required");
  receive({ reason: "admin-unavailable", route: "admin" });
  assert.equal(context.connectionStatusLabel.textContent, "Admin mode helper isn't responding");
  receive({ reason: null, route: "local" });
  assert.equal(context.connectionStatusLabel.textContent, "Connected");
  assert.equal(classes.get("status-warning"), false);
  assert.equal(context.inputReady, true);
});
