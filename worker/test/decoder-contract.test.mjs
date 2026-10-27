import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createCodecSession } from "../public/transport.js";
import { LiveStreamLabSession, webRtcDiagnosticProgress } from "../tools/lab/stream-lab-live.mjs";

test("diagnostic WebRTC selects H264 while native WebRTC and WSS retain HEVC", () => {
  assert.equal(createCodecSession({ transport: "webrtc", decoderPath: "webcodecs" }).codec, "h264");
  assert.equal(createCodecSession({ transport: "webrtc", decoderPath: "native" }).codec, "hevc");
  assert.equal(createCodecSession({ transport: "wss", decoderPath: "webcodecs" }).codec, "hevc");
});

test("diagnostic compatibility requires sustained fresh AVC presentation", () => {
  const sample = (frames, timestamp, ageMs = 30) => ({ route: "host->host", framesDrawn: frames,
    renderedAgeMs: ageMs, worker: { codec: "avc1.64002a" }, glassToGlass: { timestamp, ageMs } });
  assert.equal(webRtcDiagnosticProgress([sample(1, 100), sample(1, 100, 3000)]), false);
  assert.equal(webRtcDiagnosticProgress([sample(1, 100), sample(30, 200)]), true);
  assert.equal(webRtcDiagnosticProgress([sample(1, 100), { ...sample(30, 200), route: "wss" }]), false);
});

test("lab rejects unsupported diagnostic codec before touching session resources", async () => {
  await assert.rejects(
    LiveStreamLabSession.prototype.applyCandidateProfile.call({}, { codec: "hevc" }, null, "webrtc"),
    error => error.code === "unsupported-decoder-codec",
  );
});

test("H264 fallback waits for the newly navigated WSS route before issuing controls", async () => {
  const directory = mkdtempSync(join(tmpdir(), "bubbleplay-fallback-"));
  let snapshots = 0;
  const reachedControl = new Error("reached ready receiver control");
  const session = {
    candidateProfilePath: join(directory, "profile.json"),
    assertLeaseHealthy() {},
    async snapshot() { return ++snapshots === 1 ? null : { route: "wss" }; },
    receiver: { async evaluate() {
      assert.equal(snapshots, 2, "control must wait for WSS startup after navigation");
      throw reachedControl;
    } },
  };
  try {
    await assert.rejects(LiveStreamLabSession.prototype.collectH264FallbackSmoke.call(session, 1),
      error => error === reachedControl);
  } finally { rmSync(directory, { recursive: true, force: true }); }
});
