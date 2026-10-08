import { randomUUID } from "node:crypto";

import { enumerateDisplays, LiveStreamLabSession } from "./stream-lab-live.mjs";

const holdMs = Number.parseInt(process.argv[2] ?? "30000", 10);
if (!Number.isInteger(holdMs) || holdMs < 1_000 || holdMs > 120_000)
  throw new RangeError("hold duration must be between 1000 and 120000 ms");
const displayMode = process.argv[3] ?? "transient";
if (!["transient", "persistent"].includes(displayMode))
  throw new RangeError("display mode must be transient or persistent");
const persistentDeviceName = process.argv[4] ?? String.raw`\\.\DISPLAY5`;
const compositionStimulus = process.argv[5] ?? "none";
if (!["none", "window-nudge"].includes(compositionStimulus))
  throw new RangeError("composition stimulus must be none or window-nudge");

class DdaIsolationSession extends LiveStreamLabSession {
  async startReceiver() {}
  async admitCompatibility() {}
  async startCandidateHost() {}
  async placeReceiverOnLabDisplay() {}
  async resolvePersistentVirtualDisplay() {
    const display = enumerateDisplays().find(candidate =>
      candidate.deviceName.toLowerCase() === persistentDeviceName.toLowerCase());
    if (!display) throw new Error(`persistent display ${persistentDeviceName} was not attached`);
    return display;
  }
}

const runId = randomUUID();
const store = {
  appendTelemetry() {},
  readCampaignState() { return null; },
};
const session = new DdaIsolationSession({
  runId,
  store,
  transientVdd: displayMode === "transient",
});

try {
  await session.start("wss", { mode: "benchmark" });
  await session.source.evaluate("window.__bubbleplaySource.setScene('motion-60')");
  const source = await session.source.evaluate("window.__bubbleplaySource.metadata() ");
  process.stdout.write(`${JSON.stringify({
    event: "ready",
    runId,
    display: session.display,
    source,
    sourcePid: session.sourceProcess?.pid ?? null,
    holdMs,
    displayMode,
    compositionStimulus,
  })}\n`);
  if (compositionStimulus === "window-nudge") {
    const target = await session.source.call("Browser.getWindowForTarget");
    const deadline = Date.now() + holdMs;
    let offset = 0;
    while (Date.now() < deadline) {
      offset ^= 1;
      await session.source.call("Browser.setWindowBounds", {
        windowId: target.windowId,
        bounds: {
          left: session.display.x + offset,
          top: session.display.y,
          width: session.display.width,
          height: session.display.height,
        },
      });
      await new Promise(resolve => setTimeout(resolve, 250));
    }
    await session.source.call("Browser.setWindowBounds", {
      windowId: target.windowId,
      bounds: {
        left: session.display.x,
        top: session.display.y,
        width: session.display.width,
        height: session.display.height,
      },
    });
  } else {
    await new Promise(resolve => setTimeout(resolve, holdMs));
  }
} finally {
  await session.stop();
}
