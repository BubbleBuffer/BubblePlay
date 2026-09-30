import { readClientSource } from "./helpers/client-source.mjs";
import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { campaignCheckpointContract } from "../tools/stream-lab-live.mjs";

const client = readClientSource();
const runner = readFileSync(new URL("../tools/stream-lab-live.mjs", import.meta.url), "utf8");

test("diagnostics exposes structured signaling and monitor evidence", () => {
  assert.match(client, /signaling:\s*\{/);
  assert.match(client, /registered:\s*signalingRegistered/);
  assert.match(client, /hostEvidence/);
  assert.match(client, /lastError/);
  assert.match(client, /monitors:\s*monitorList\.map/);
  assert.match(client, /requestMonitors\(\)/);
  assert.match(client, /action:\s*"monitors"/);
  assert.match(client, /selectMonitor\(selector\)/);
  assert.match(client, /injectDecoderFault\(requestReason = "diagnostic-injected-fault", qualityRequestId = null\)/);
  assert.match(client, /requestReason === "stream-lab-quality-capture"[\s\S]*?: "diagnostic-injected-fault"/);
  assert.match(client, /beginWssRecovery\(reason, qualityRequestId\)/);
  assert.match(client, /qualityKeyframeAck\(requestId\)/);
  assert.match(client, /quality-keyframe-admitted/);
  assert.match(client, /return beginWssRecovery\(reason, qualityRequestId\)/);
  assert.match(client, /setFixedZoom\(zoom\)/);
  assert.match(client, /setStreamMode\(mode\)/);
  assert.match(client, /forceH264Fallback\(\)/);
  assert.match(client, /viewport:\s*viewportHealthSnapshot\(\)/);
});

test("diagnostic absolute cursor movement predicts overlay and sharp crop before feedback", () => {
  const diagnostic = client.slice(client.indexOf("moveCursorNormalized(x, y)"),
    client.indexOf("setStreamMode(mode)", client.indexOf("moveCursorNormalized(x, y)")));
  assert.match(diagnostic, /updateRemoteCursor\(cursor => cursor\.setNormalized\(nx, ny\)\)/);
  assert.match(diagnostic, /return queueTouchAbsolute\(point\.x, point\.y\)/);
  const update = client.slice(client.indexOf("function updateRemoteCursor(update)"),
    client.indexOf("function showTouchCursor", client.indexOf("function updateRemoteCursor(update)")));
  assert.match(update, /locallyAuthoritativeCursor=true/);
  assert.match(update, /renderViewport\(\)/);
  assert.match(update, /scheduleSharpViewport\("cursor-follow"\)/);
  assert.match(runner, /const moveCursor = async \(x, y\) => poll\(/);
  assert.match(runner, /moveCursor\(0\.95, 0\.5\)/);
});

test("WSS recovery watches for a keyframe and never suppresses an identified lab request", () => {
  const request = client.slice(client.indexOf("function requestWssKeyframe(reason"),
    client.indexOf("function clearWssKeyframeWatchdog", client.indexOf("function requestWssKeyframe(reason")));
  assert.match(request, /if \(!requestId && requestWssKeyframe\.lastAt/);
  assert.match(request, /return false/);
  assert.match(request, /sendSignal\([\s\S]*?requestId[\s\S]*?return true/);
  const recovery = client.slice(client.indexOf("function beginWssRecovery(reason"),
    client.indexOf("function scheduleWssAck", client.indexOf("function beginWssRecovery(reason")));
  assert.match(recovery, /function beginWssRecovery\(reason, requestId = null\)/);
  assert.match(recovery, /requestWssKeyframe\(reason, requestId\)/);
  assert.match(recovery, /if \(requested\) armWssKeyframeWatchdog\(\)/);
  const diagnostic = client.slice(client.indexOf("injectDecoderFault(requestReason"),
    client.indexOf("setFixedZoom(zoom)", client.indexOf("injectDecoderFault(requestReason")));
  assert.match(diagnostic, /return beginWssRecovery\(reason, qualityRequestId\)/);
  assert.doesNotMatch(diagnostic, /resetWssDecoderForRecovery/);
});

test("campaign runner checkpoints the complete baseline and post-champion evidence matrix", () => {
  const units = campaignCheckpointContract();
  assert.deepEqual(units.baseline, [
    "baseline:unshaped:r1", "baseline:2m:r1", "baseline:4m:r1", "baseline:4m:r2", "baseline:4m:r3",
    "baseline:8m:r1", "baseline:12m:r1",
  ]);
  assert.deepEqual(units.stability, [
    "stability:decoder-fault-recovery", "stability:sharp-viewport-2x", "stability:adaptive-bandwidth:8m-1",
    "stability:adaptive-bandwidth:4m-1", "stability:adaptive-bandwidth:2m", "stability:adaptive-bandwidth:4m-2",
    "stability:adaptive-bandwidth:8m-2", "stability:adaptive-bandwidth-staircase", "stability:wss-gaming",
    "stability:webrtc-gaming-unshaped", "stability:webrtc-gaming-relay-8m",
    "stability:h264-fallback-smoke", "stability:post-cleanup-idle",
  ]);
});

test("lab runner refreshes and selects monitors through diagnostics, never monitor DOM", () => {
  assert.match(runner, /__bubbleplayDiagnostics\?\.requestMonitors/);
  assert.match(runner, /__bubbleplayDiagnostics\?\.selectMonitor/);
  assert.doesNotMatch(runner, /#monitor-list \.monitor-card/);
});

test("lab quality pass records exact frame traces and rechecks identity after screenshots", () => {
  assert.match(runner, /exactFrameAlignment\(/);
  assert.match(runner, /phase:\s*"quality-frame-trace"/);
  assert.match(runner, /postCapture/);
  assert.match(runner, /retainWorstSamplePairs\(/);
});

test("performance and parity screenshots remain separate passes", () => {
  const performanceBody = runner.match(/async collectPerformance\([\s\S]*?\n  async measureActionToPhoton/)?.[0] ?? "";
  assert.doesNotMatch(performanceBody, /captureScreenshot|captureViewport/);
  assert.match(runner, /phase:\s*"performance"/);
  assert.match(runner, /phase:\s*"quality"/);
});

test("lab CLI exposes smoke and resumable campaign commands", () => {
  const cli = readFileSync(new URL("../tools/stream-lab.mjs", import.meta.url), "utf8");
  const pkg = JSON.parse(readFileSync(new URL("../package.json", import.meta.url), "utf8"));
  assert.match(cli, /campaign/);
  assert.match(cli, /--resume/);
  assert.equal(pkg.scripts["lab:smoke"], "node tools/stream-lab.mjs smoke");
  assert.equal(pkg.scripts["lab:campaign"], "node tools/stream-lab.mjs campaign");
});
