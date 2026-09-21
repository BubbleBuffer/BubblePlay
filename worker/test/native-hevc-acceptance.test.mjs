import test from "node:test";
import assert from "node:assert/strict";
import { nativeFrameState } from "../tools/native-hevc-acceptance.mjs";

test("native acceptance requires HEVC, current selected monitor and fresh presented frames", () => {
  const sample = { signaling: { registered: true, monitors: [{ selector: "display5", selected: true }] },
    route: "host->host", rtc: { generation: 1, codec: { mimeType: "video/H265" }, inbound: { timestamp: 1200, framesDecoded: 20 } }, renderedAgeMs: 20,
    nativePresentation: { generation: 1, supported: true, firstFrameAtMs: 1000, framesPresented: 20, width: 1920, height: 1080 } };
  assert.equal(nativeFrameState(sample, "DISPLAY5").ready, true);
  assert.equal(nativeFrameState(sample, "display1").ready, false);
  assert.equal(nativeFrameState({ ...sample, renderedAgeMs: null }, "display5").ready, false);
  assert.equal(nativeFrameState({ ...sample, renderedAgeMs: 2000 }, "display5").ready, false);
  assert.equal(nativeFrameState({ ...sample, rtc: { codec: { mimeType: "video/H264" } } }, "display5").ready, false);
  assert.equal(nativeFrameState({ ...sample, rtc: { ...sample.rtc, inbound: { timestamp: 900, framesDecoded: 20 } } }, "display5").ready, false);
});
