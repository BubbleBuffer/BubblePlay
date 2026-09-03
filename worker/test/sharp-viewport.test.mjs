import test from "node:test";
import assert from "node:assert/strict";
import {
  cropGain,
  cropNeedsUpdate,
  desiredSharpCrop,
  paddedDesktopCrop,
  visibleDesktopRect,
} from "../public/sharp-viewport.js";

test("visible crop maps a transformed landscape desktop back to source pixels", () => {
  const visible = visibleDesktopRect({
    zoom: 2, x: -390, y: -219, viewportWidth: 780, viewportHeight: 438,
    content: { x: 0, y: 0, width: 780, height: 438 }, desktopWidth: 1920, desktopHeight: 1080,
  });
  assert.deepEqual(visible, { x: 480, y: 270, width: 960, height: 540 });
});

test("portrait letterboxing is intersected before conversion to desktop pixels", () => {
  const visible = visibleDesktopRect({
    zoom: 2, x: -195, y: -312.5, viewportWidth: 390, viewportHeight: 844,
    content: { x: 0, y: 312.5, width: 390, height: 219 }, desktopWidth: 1920, desktopHeight: 1080,
  });
  assert.equal(Math.round(visible.x), 480);
  assert.equal(Math.round(visible.width), 960);
  assert.equal(Math.round(visible.y), 0);
  assert.equal(Math.round(visible.height), 1080);
});

test("padded crops are bounded, quantized, and retain useful zoom gain", () => {
  const crop = paddedDesktopCrop({ x: 480, y: 270, width: 960, height: 540 }, 1920, 1080);
  assert.deepEqual(crop, { x: 384, y: 208, width: 1152, height: 656 });
  const gain = cropGain({ enabled: true, ...crop }, 1920, 1080);
  assert.ok(gain.x > 1.6 && gain.y > 1.6);
});

test("sharp mode enters and exits with hysteresis and a material crop", () => {
  const base = { x: -390, y: -219, viewportWidth: 780, viewportHeight: 438,
    content: { x: 0, y: 0, width: 780, height: 438 }, desktopWidth: 1920, desktopHeight: 1080 };
  assert.equal(desiredSharpCrop({ ...base, viewport: { zoom: 1.05, x: 0, y: 0 } }).enabled, false);
  assert.equal(desiredSharpCrop({ ...base, viewport: { zoom: 2, x: -390, y: -219 } }).enabled, true);
  assert.equal(desiredSharpCrop({ ...base, viewport: { zoom: 1.06, x: 0, y: 0 }, wasEnabled: true }).enabled, false);
});

test("active overscan suppresses jitter but recenters near its safe edge", () => {
  const active = { enabled: true, x: 384, y: 208, width: 1152, height: 656 };
  const centered = { enabled: true, ...active, visible: { x: 480, y: 270, width: 960, height: 540 } };
  const edge = { enabled: true, ...active, visible: { x: 410, y: 220, width: 960, height: 540 } };
  assert.equal(cropNeedsUpdate(active, centered, 1920, 1080), false);
  assert.equal(cropNeedsUpdate(active, edge, 1920, 1080), true);
});
