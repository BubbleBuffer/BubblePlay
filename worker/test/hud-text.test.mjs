import test from "node:test";
import assert from "node:assert/strict";
import { formatHud, formatQuality } from "../public/hud-text.js";

const counters = { ws: "open", ice: "connected", route: "p2p", control: "ok", motion: "ok", zoom: 1, pointer: "desktop",
  cursorSeq: "1/1", cursorAge: 3, motionSent: 1, motionCoalesced: 2, motionDropped: 0, touch: "-", dec: "h264",
  recv: 10, queued: 0, dropped: 0, resyncs: 0, keyframes: 1, fps: 60, presented: 9, frameGap: 16, maxFrameGap: 40,
  dimensions: "1920x1080", lag: 5, rtt: 12, jit: 1, jbuf: 2, loss: 0, mbps: 8, mediaGeneration: 3 };
const extra = { clientBuild: "b1", fullFrame: false, sharp: { state: "idle", revision: 2, crop: "full" }, hostQuality: null };

test("HUD has one line per group and the quality line reflects the host report", () => {
  assert.equal(formatHud(counters, extra).split("\n").length, 7);
  assert.match(formatHud(counters, extra), /sharp-zoom \| media g3 \| view idle r2 full\nquality …$/);
  const quality = { profile: "balanced", width: 1280, height: 720, fps: 60, targetBitrateBps: 8_000_000, bweBps: null, reason: "bwe" };
  assert.equal(formatQuality(quality), "quality balanced 1280x720@60 8000k/… bwe");
  assert.equal(formatQuality({ ...quality, bweBps: 6_500_000 }), "quality balanced 1280x720@60 8000k/6500k bwe");
});

test("host-supplied quality strings never become markup in the HUD", async () => {
  const { parseHostQuality } = await import("../public/latency.js");
  const { renderHud } = await import("../public/hud-text.js");
  const { readFileSync } = await import("node:fs");
  const evil = "<img src=x onerror=alert(1)>";
  const base = { type: "quality", profile: "720p30", width: 1280, height: 720, fps: 30, targetBitrateBps: 3_000_000, bweBps: null, reason: "bwe" };
  assert.equal(parseHostQuality({ ...base, profile: evil }), null);
  assert.equal(parseHostQuality({ ...base, reason: evil }), null);
  assert.ok(parseHostQuality({ ...base, profile: "480p24", reason: "bwe-down" }));
  // Even a value that got past the parser is only assigned as text.
  const element = { textContent: "", set innerHTML(_) { throw new Error("HUD must not parse markup"); } };
  renderHud(element, formatHud(counters, { ...extra, hostQuality: { ...base, profile: evil, reason: evil } }));
  assert.ok(element.textContent.includes(evil));
  assert.doesNotMatch(readFileSync(new URL("../public/receiver.js", import.meta.url), "utf8"), /hud\.innerHTML/);
});
