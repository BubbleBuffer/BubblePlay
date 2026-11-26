import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { FAILURES, HOST_TYPES, LIMITS, STATES, TYPES, WINDOW_LIST_CAP, WINDOW_LIST_VERSION, focusMessage, iconRequest,
  parseFocusFailed, parseIcon, parseWindowList, startMessage, stopMessage } from "../public/window-list-wire.js";
import { NOTICES, createAppFocus, focusView } from "../public/app-focus.js";
import { FocusViewport } from "../public/app-focus-viewport.js";
import { VIEWER_HELLO_CAPS } from "../public/dk-session.js";

// fixtures/window-list-contract.json, the shared contract of app focus mode.
const contract = JSON.parse(readFileSync(new URL("../../fixtures/window-list-contract.json", import.meta.url)));
const host = name => JSON.parse(contract.hostMessages[name].text);
const [excel, notes, calculator] = parseWindowList(host("windowList"));

test("names and limits come from the contract, and the viewer advertises the capability", () => {
  assert.equal(WINDOW_LIST_CAP, contract.capability);
  assert.ok(VIEWER_HELLO_CAPS.includes(contract.capability));
  assert.equal(WINDOW_LIST_VERSION, contract.wireVersion);
  assert.deepEqual({ ...LIMITS }, contract.limits);
  assert.deepEqual(Object.values(STATES), contract.states);
  assert.deepEqual(Object.values(FAILURES), contract.focusFailureReasons);
  for (const type of HOST_TYPES) assert.ok(Object.values(TYPES).includes(type));
});

test("host messages of the contract parse, in z-order, with rect and monitor", () => {
  assert.deepEqual([excel.id, notes.id, calculator.id], [265142, 131330, 66012]);
  assert.deepEqual(excel.rect, { left: 120, top: 80, width: 1480, height: 820 });
  assert.equal(excel.monitor, "\\\\.\\DISPLAY1");
  assert.equal(notes.icon, null);
  assert.equal(calculator.state, STATES.minimized);
  assert.deepEqual(parseFocusFailed(host("focusFailed")), { id: 265142, reason: "elevated" });
  const icon = parseIcon(host("icon"));
  assert.equal(icon.hash, "9f2c4e6a1b3d5f70");
  assert.equal(icon.bytes[1], 0x50);
});

test("a malformed entry is dropped without losing the list", () => {
  const message = host("windowList");
  message.windows.splice(1, 0, { id: -1, title: "x", process: "x", rect: [0, 0, 1, 1], state: "normal" }, { ...message.windows[1], rect: [0, 0, 0, 5] },
    { ...message.windows[1], state: "hidden" });
  assert.deepEqual(parseWindowList(message).map(w => w.id), [265142, 131330, 66012]);
  assert.equal(parseWindowList({ ...host("windowList"), v: 2 }), null);
  assert.equal(parseIcon({ ...host("icon"), png: "AAAA" }), null);
});

test("viewer messages equal the contract's accepted vectors, and the rejected ones cannot be built", () => {
  const built = { start: startMessage(), stop: stopMessage() };
  for (const vector of contract.clientMessages.accepted) {
    const sent = JSON.parse(vector.text);
    if (vector.kind === "start" || vector.kind === "stop") assert.deepEqual(built[vector.kind], sent);
    if (vector.kind === "focus" && sent.v) assert.deepEqual(focusMessage(vector.id), sent);
    if (vector.kind === "icon") assert.deepEqual(iconRequest(vector.icon), sent);
  }
  for (const { text } of contract.clientMessages.rejected) {
    const sent = JSON.parse(text);
    if (sent.type === TYPES.focus && sent.v === WINDOW_LIST_VERSION) assert.equal(focusMessage(sent.id), null, text);
    if (sent.type === TYPES.icon) assert.equal(iconRequest(sent.icon), null, text);
  }
});

const display = (selector, originX, width = 2560, selected = false) => ({ selector, originX, originY: 0, width, height: 1440, selected });
const monitors = [display("\\\\.\\DISPLAY1", 0, 2560, true), display("\\\\.\\DISPLAY2", 2560)];
const win = (over, base = excel) => ({ ...base, ...over, rect: { ...base.rect, ...over.rect } });

test("focusView: crop fractions, minus the streamed monitor's origin, or the full desktop with the reason", () => {
  const view = (windows, extra = {}) => focusView({ windows, focusedId: windows[0].id, monitors, secure: false, ...extra });
  assert.deepEqual(view([excel]).crop, { x: 120 / 2560, y: 80 / 1440, width: 1480 / 2560, height: 820 / 1440 });
  // Stream moved to the second monitor: the same window now needs its origin removed.
  const second = [display("\\\\.\\DISPLAY1", 0), display("\\\\.\\DISPLAY2", 2560, 2560, true)];
  assert.deepEqual(view([win({ monitor: "\\\\.\\DISPLAY2", rect: { left: 2700 } })], { monitors: second }).crop.x, 140 / 2560);
  // Spans the monitor edge: only the part on the streamed monitor.
  const spanning = view([win({ rect: { left: 2000, width: 1000 } })]).crop;
  assert.equal(spanning.x + spanning.width, 1);
  const cases = [
    ["app on another display", view([notes]), NOTICES.otherDisplay],
    ["app closed (not in the list)", focusView({ windows: [excel], focusedId: 1, monitors, secure: false }), NOTICES.closed],
    ["secure desktop up", view([excel], { secure: true }), null],
    ["minimized", view([calculator]), null],
    ["no focus", view([excel], { focusedId: null }), null],
  ];
  for (const [name, result, notice] of cases) { assert.equal(result.mode, "full", name); assert.equal(result.notice, notice, name); }
});

function rig({ followOn = true, capable = true } = {}) {
  const sent = [], followSent = [];
  let opened = 0;
  const focus = createAppFocus({ send: m => { sent.push(m.type + (m.id !== undefined ? `:${m.id}` : "")); return true; },
    follow: { capable: () => followOn !== null, shown: () => followOn, send: v => followSent.push(v) },
    openSwitcher: () => { opened++; }, createUrl: () => "blob:icon", revokeUrl: () => {} });
  focus.setCapable(capable);
  focus.setMonitors(monitors);
  return { focus, sent, followSent, opened: () => opened };
}

test("enter: list on while the switcher is open, follow off, focus sent; leave restores follow and stops the list", () => {
  const { focus, sent, followSent } = rig();
  focus.openSwitcher();
  focus.onMessage(host("windowList"));
  assert.equal(focus.pick(999), false);
  assert.ok(focus.pick(excel.id));
  focus.closeSwitcher(); // the popover closing after the pick must not stop the list
  assert.deepEqual(sent, [TYPES.start, `${TYPES.icon}`, `${TYPES.focus}:${excel.id}`]);
  assert.deepEqual(followSent, [false]);
  assert.equal(focus.view.mode, "crop");
  focus.leave();
  assert.deepEqual(followSent, [false, true]);
  assert.equal(sent.at(-1), TYPES.stop);
  assert.equal(focus.view.mode, "full");
});

test("follow that was off is left alone", () => {
  const { focus, followSent } = rig({ followOn: false });
  focus.openSwitcher(); focus.onMessage(host("windowList")); focus.pick(excel.id); focus.leave();
  assert.deepEqual(followSent, []);
});

test("focus ends with the reason when the host fails it, and with the switcher when the app closes", () => {
  const failed = rig();
  failed.focus.openSwitcher(); failed.focus.onMessage(host("windowList")); failed.focus.pick(excel.id);
  failed.focus.onMessage(host("focusFailed"));
  assert.equal(failed.focus.active, false);
  assert.equal(failed.focus.notice, NOTICES.elevated);
  assert.deepEqual(failed.followSent, [false, true]);

  const closed = rig();
  closed.focus.openSwitcher(); closed.focus.onMessage(host("windowList")); closed.focus.pick(excel.id);
  closed.focus.onMessage({ ...host("windowList"), windows: host("windowList").windows.slice(1) });
  assert.equal(closed.focus.active, false);
  assert.equal(closed.focus.notice, NOTICES.closed);
  assert.equal(closed.opened(), 1);
});

test("without the negotiated capability nothing is sent, and a new session drops focus mode", () => {
  const off = rig({ capable: false });
  off.focus.openSwitcher(); off.focus.onMessage(host("windowList"));
  assert.equal(off.focus.pick(excel.id), false);
  assert.deepEqual(off.sent, []);
  const on = rig();
  on.focus.openSwitcher(); on.focus.onMessage(host("windowList")); on.focus.pick(excel.id);
  on.focus.reset();
  assert.equal(on.focus.active, false);
  assert.deepEqual(on.followSent, [false]); // the new host session starts with follow off; the saved setting is resent by the client
});

// Pointer input through the crop. The picture is #visual's transform of the viewport; the mouse path reads the
// transformed content rect (videoContentRect on the element) and normalizes over it, the touch path uses the
// same zoom and pan through RemoteCursor. Both must land on the monitor pixel under the pointer.
test("a pointer over the cropped picture maps to the monitor pixel it shows", () => {
  const monitor = { width: 2560, height: 1440 };
  const strip = 32; // the stream area starts below the docked strip
  const cases = [
    { name: "wide window", window: [120, 80, 1480, 820], area: [1280, 720] },
    { name: "tall window, letterboxed sides", window: [2000, 100, 400, 1200], area: [1280, 720] },
    { name: "small window in a narrow area", window: [900, 500, 320, 240], area: [600, 900] },
    { name: "whole monitor", window: [0, 0, 2560, 1440], area: [1000, 700] },
  ];
  for (const { name, window: [left, top, width, height], area: [areaWidth, areaHeight] } of cases) {
    const viewport = new FocusViewport(areaWidth, areaHeight);
    viewport.resize(areaWidth, areaHeight, 0, strip);
    const scale = Math.min(areaWidth / monitor.width, areaHeight / monitor.height);
    const content = { x: (areaWidth - monitor.width * scale) / 2, y: strip + (areaHeight - monitor.height * scale) / 2, width: monitor.width * scale, height: monitor.height * scale };
    viewport.setContent(content);
    viewport.setFocus({ x: left / monitor.width, y: top / monitor.height, width: width / monitor.width, height: height / monitor.height });
    const { zoom, x, y } = viewport.snapshot();
    const shown = { left: x + content.x * zoom, top: y + content.y * zoom, width: content.width * zoom, height: content.height * zoom };
    const toClient = (px, py) => ({ x: shown.left + px / monitor.width * shown.width, y: shown.top + py / monitor.height * shown.height });
    const toDesktop = point => ({ x: (point.x - shown.left) / shown.width, y: (point.y - shown.top) / shown.height });
    // The window's corners and centre land on the stream area, its centre in the middle, and map back exactly.
    const centre = toClient(left + width / 2, top + height / 2);
    assert.ok(Math.abs(centre.x - areaWidth / 2) < 1e-6 && Math.abs(centre.y - (strip + areaHeight / 2)) < 1e-6, name);
    const fitsAxis = Math.abs(width * scale * zoom - areaWidth) < 1e-6 || Math.abs(height * scale * zoom - areaHeight) < 1e-6;
    assert.ok(fitsAxis, `${name}: fills one axis of the area`);
    for (const [px, py] of [[left, top], [left + width, top + height], [left + width / 3, top + height / 4]]) {
      const back = toDesktop(toClient(px, py));
      assert.ok(Math.abs(back.x * monitor.width - px) < 1e-6 && Math.abs(back.y * monitor.height - py) < 1e-6, name);
    }
    // RemoteCursor.screenPoint's formula agrees with the mouse path's transformed rect.
    const touch = { x: x + (content.x + (left / monitor.width) * content.width) * zoom, y: y + (content.y + (top / monitor.height) * content.height) * zoom };
    assert.ok(Math.abs(touch.x - toClient(left, top).x) < 1e-6 && Math.abs(touch.y - toClient(left, top).y) < 1e-6, name);
    // The letterbox is not the app: a point outside the crop is held at its edge.
    const edge = viewport.clampPoint(0, 1);
    assert.deepEqual(edge, { x: left / monitor.width, y: (top + height) / monitor.height });
  }
});

test("clearing the crop restores the ordinary viewport", () => {
  const viewport = new FocusViewport(800, 600);
  viewport.setContent({ x: 0, y: 0, width: 800, height: 600 });
  viewport.setFocus({ x: 0.25, y: 0.25, width: 0.5, height: 0.5 });
  assert.equal(viewport.zoom, 2);
  assert.equal(viewport.clipPath({ left: 0, top: 0, width: 800, height: 600 }), "inset(150px 200px 150px 200px)");
  viewport.setFocus(null);
  assert.deepEqual(viewport.snapshot(), { zoom: 1, x: 0, y: 0 });
  assert.equal(viewport.clipPath({ left: 0, top: 0, width: 800, height: 600 }), "");
  assert.deepEqual(viewport.clampPoint(0.1, 0.9), { x: 0.1, y: 0.9 });
});
