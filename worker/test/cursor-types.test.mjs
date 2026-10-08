import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { BrowserCursorAppearance, CursorKind, cursorOverlayForKind, decodeCursorType, nativeCursorForKind } from "../public/cursor.js";
import { decodeCursorState } from "../public/input.js";

const contract = JSON.parse(readFileSync(new URL("../../fixtures/cursor-types.json", import.meta.url), "utf8"));
const packet = kind => Uint8Array.of(contract.packetType, kind);
function harness() {
  const surface = { style: {} }, classes = new Set();
  const overlay = { style: {}, innerHTML: "", classList: { toggle(name, enabled) { enabled ? classes.add(name) : classes.delete(name); } } };
  const appearance = new BrowserCursorAppearance(surface, overlay);
  const state = { pointerMode: "desktop", inputReady: true, point: { x: 100, y: 200 }, overlayVisible: true, mousePointerActive: false };
  return { surface, overlay, classes, appearance, render: overrides => appearance.render({ ...state, ...overrides }) };
}

test("all wire cursor types map to the shared native cursor contract", () => {
  for (const { id, name, css } of contract.types) {
    assert.equal(CursorKind[name], id, name);
    assert.equal(decodeCursorType(packet(id)), id, name);
    assert.equal(nativeCursorForKind(id), css, name);
  }
});

test("cursor-type decoder respects view offsets and rejects unrelated/truncated messages", () => {
  assert.equal(decodeCursorType(new DataView(Uint8Array.of(0, 129, 2, 0).buffer, 1, 2)), CursorKind.Pointer);
  assert.equal(decodeCursorType(packet(CursorKind.Text).buffer), CursorKind.Text);
  for (const value of [null, "129,1", [129, 1], Uint8Array.of(129), Uint8Array.of(129, 1, 0), Uint8Array.of(128, 1)]) {
    assert.equal(decodeCursorType(value), null);
  }
  assert.equal(decodeCursorType(packet(255)), CursorKind.Arrow, "future/custom kinds have a safe native fallback");
});

test("old position-only hosts retain their arrow and independent input acknowledgements", () => {
  const h = harness(), oldState = Uint8Array.of(128, 7, 0, 0, 64, 0, 192, 3);
  assert.equal(h.appearance.receive(oldState), false);
  assert.equal(decodeCursorState(oldState).ackSequence, 7);
  h.render();
  assert.equal(h.surface.style.cursor, "default");
  assert.equal(h.classes.has("visible"), true, "legacy OS-hidden overlay usability is preserved");
});

test("stationary appearance feedback updates a native mouse without drawing a second cursor", () => {
  const h = harness();
  h.render({ mousePointerActive: true });
  h.appearance.receive(packet(CursorKind.Text));
  h.render({ mousePointerActive: true });
  assert.equal(h.surface.style.cursor, "text");
  assert.equal(h.classes.has("visible"), false);
  h.appearance.receive(packet(CursorKind.ResizeEW)); h.render({ mousePointerActive: true });
  assert.equal(h.surface.style.cursor, "ew-resize");
});

test("one authoritative reliable lane prevents delayed duplicate kinds from rewinding appearance", () => {
  const h = harness();
  assert.equal(h.appearance.receive(packet(CursorKind.Text), { source: "rtc", controlOpen: true }), true);
  assert.equal(h.appearance.receive(packet(CursorKind.Arrow), { source: "signaling", controlOpen: true }), false);
  h.render({ mousePointerActive: true });
  assert.equal(h.surface.style.cursor, "text");
  assert.equal(h.appearance.receive(packet(CursorKind.Wait), { source: "signaling", controlOpen: false }), true);
  assert.equal(h.appearance.receive(packet(CursorKind.Text), { source: "rtc", controlOpen: false }), false);
  h.render({ mousePointerActive: true });
  assert.equal(h.surface.style.cursor, "wait", "WSS/fallback accepts only signaling");
});

test("Work mode keeps the pointer usable when Windows hides its cursor sprite", () => {
  const h = harness();
  h.appearance.receive(packet(CursorKind.Text)); h.render();
  assert.equal(h.overlay.style.transform, "translate3d(88px,184px,0)");
  const text = h.overlay.innerHTML;
  h.appearance.receive(packet(CursorKind.ResizeNS)); h.render();
  assert.notEqual(h.overlay.innerHTML, text);
  assert.equal(h.overlay.style.transform, "translate3d(88px,184px,0)");
  assert.equal(h.classes.has("visible"), true);
  h.appearance.receive(packet(CursorKind.Hidden)); h.render();
  assert.equal(h.surface.style.cursor, "default");
  assert.equal(h.classes.has("visible"), true);
  h.render({ mousePointerActive: true });
  assert.equal(h.surface.style.cursor, "default", "a physical mouse retains a native pointer");
  assert.equal(h.classes.has("visible"), false, "a physical mouse still avoids a duplicate overlay");
  h.appearance.receive(packet(CursorKind.Text)); h.render({ mousePointerActive: true });
  assert.equal(h.surface.style.cursor, "text", "visible shape feedback still updates after Hidden");
  h.appearance.receive(packet(CursorKind.Arrow)); h.render();
  assert.equal(h.classes.has("visible"), true);
  assert.equal(h.overlay.style.transform, "translate3d(98px,198px,0)");
});

test("session reset and Game mode cannot retain a remote hidden or busy native cursor", () => {
  const h = harness();
  h.appearance.receive(packet(CursorKind.Hidden)); h.render();
  h.render({ pointerMode: "game", overlayVisible: false });
  assert.equal(h.surface.style.cursor, "default");
  h.render(); assert.equal(h.surface.style.cursor, "default", "returning to Work restores a usable pointer");
  h.appearance.reset(); h.render({ inputReady: false, overlayVisible: false });
  assert.equal(h.surface.style.cursor, "default");
  assert.equal(h.appearance.kind, CursorKind.Arrow);
  h.appearance.receive(packet(CursorKind.Wait)); h.render({ inputReady: false, overlayVisible: false });
  assert.equal(h.surface.style.cursor, "default", "gated input leaves local navigation usable");
});

test("all visible touch glyphs have a bounded hotspot and a contrasting outline", () => {
  for (const { id, name } of contract.types.filter(type => type.id !== CursorKind.Hidden)) {
    const { markup, hotspot, hidden } = cursorOverlayForKind(id);
    assert.equal(hidden, false, name);
    assert.match(markup, /(?:stroke|fill)="black"/, name);
    assert.ok(hotspot.x >= 0 && hotspot.x < 24 && hotspot.y >= 0 && hotspot.y < 32, name);
  }
});
