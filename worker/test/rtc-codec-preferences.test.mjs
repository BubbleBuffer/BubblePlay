import test from "node:test";
import assert from "node:assert/strict";
import { orderRtpCodecPreferences, rtpCodecMimeTypes } from "../public/transport.js";

const hevc = (sdpFmtpLine = "profile-id=1") => ({ mimeType: "video/H265", clockRate: 90000, sdpFmtpLine });
const hevcAlias = { mimeType: "video/hevc", clockRate: 90000, sdpFmtpLine: "profile-id=2" };
const h264High = { mimeType: "video/H264", clockRate: 90000, sdpFmtpLine: "packetization-mode=1;profile-level-id=640c1f" };
const h264Baseline = { mimeType: "video/H264", clockRate: 90000, sdpFmtpLine: "packetization-mode=1;profile-level-id=42e01f" };
const rtx = { mimeType: "video/rtx", clockRate: 90000, sdpFmtpLine: "apt=96" };
const ulpfec = { mimeType: "video/ulpfec", clockRate: 90000 };

const names = (list) => list.map((c) => `${c.mimeType.toLowerCase()}|${c.sdpFmtpLine ?? ""}`);

test("HEVC selection places every HEVC entry before H.264 when capabilities list H.264 first", () => {
  const caps = [h264High, rtx, hevc(), ulpfec, h264Baseline, hevcAlias];
  assert.deepEqual(orderRtpCodecPreferences(caps, "hevc"), [
    hevc(), hevcAlias, h264High, h264Baseline,
  ]);
});

test("HEVC selection gives the same family order whether H.264 or HEVC is listed first", () => {
  // Both inputs keep the same order inside each family; only the family order differs.
  const h264First = [h264High, h264Baseline, hevc(), hevcAlias];
  const hevcFirst = [hevc(), hevcAlias, h264High, h264Baseline];
  const expected = names([hevc(), hevcAlias, h264High, h264Baseline]);
  assert.deepEqual(names(orderRtpCodecPreferences(h264First, "hevc")), expected);
  assert.deepEqual(names(orderRtpCodecPreferences(hevcFirst, "hevc")), expected);
});

test("HEVC selection puts H.264 second when one H.264 capability is present", () => {
  const ordered = orderRtpCodecPreferences([h264High, hevc()], "hevc");
  assert.equal(ordered[0].mimeType.toLowerCase(), "video/h265");
  assert.equal(ordered[1].mimeType.toLowerCase(), "video/h264");
  assert.equal(ordered.length, 2);
});

test("HEVC Main10 entries are not dropped and keep their capability order", () => {
  const main10 = { mimeType: "video/H265", clockRate: 90000, sdpFmtpLine: "profile-id=2;tier-flag=0" };
  const main = hevc("profile-id=1");
  const ordered = orderRtpCodecPreferences([h264High, main10, main], "hevc");
  assert.deepEqual(ordered, [main10, main, h264High]);
});

test("H.264 selection is unchanged: only H.264 entries, in capability order, never HEVC", () => {
  const caps = [hevc(), h264Baseline, rtx, hevcAlias, h264High];
  assert.deepEqual(orderRtpCodecPreferences(caps, "h264"), [h264Baseline, h264High]);
  assert.deepEqual(orderRtpCodecPreferences([h264High, hevc(), h264Baseline], "h264"), [h264High, h264Baseline]);
});

test("H.264 selection with no H.264 capability returns nothing, so no preferences are set", () => {
  assert.deepEqual(orderRtpCodecPreferences([hevc(), rtx], "h264"), []);
});

test("HEVC selection with no HEVC capability yields only H.264, so the caller's HEVC-missing fallback still triggers", () => {
  const ordered = orderRtpCodecPreferences([rtx, h264High, ulpfec], "hevc");
  assert.deepEqual(ordered, [h264High]);
  const primary = rtpCodecMimeTypes("hevc");
  assert.equal(ordered.some((c) => primary.includes(c.mimeType.toLowerCase())), false);
});

test("missing or empty capabilities and unknown codecs yield an empty preference list", () => {
  assert.deepEqual(orderRtpCodecPreferences(undefined, "hevc"), []);
  assert.deepEqual(orderRtpCodecPreferences(null, "h264"), []);
  assert.deepEqual(orderRtpCodecPreferences([], "hevc"), []);
  assert.deepEqual(orderRtpCodecPreferences([hevc(), h264High], "vp9"), []);
});

test("capability entries without a mimeType are skipped rather than throwing", () => {
  assert.deepEqual(orderRtpCodecPreferences([{ clockRate: 90000 }, h264High, null], "hevc"), [h264High]);
});
