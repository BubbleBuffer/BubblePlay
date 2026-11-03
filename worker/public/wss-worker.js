// Dedicated Worker for the WSS video path: decode plus every draw happen here
// (see wss-pipeline.js). The main thread sends canvases once, then frames and
// small control messages; results come back as plain messages.
import { createWssPipeline } from "./wss-pipeline.js";

let pipeline = null;
const post = (message) => self.postMessage(message);

function supported() {
  try {
    return typeof VideoDecoder !== "undefined" && typeof EncodedVideoChunk !== "undefined" &&
      typeof OffscreenCanvas !== "undefined" && !!new OffscreenCanvas(1, 1).getContext("2d");
  } catch { return false; }
}

self.onmessage = ({ data }) => {
  try {
    switch (data.type) {
      case "capabilities":
        post({ type: "capabilities", supported: supported() });
        break;
      case "init": {
        // The main thread's clock is the reference for every timestamp it sees.
        const offset = performance.timeOrigin - data.timeOrigin;
        pipeline = createWssPipeline({ post, now: () => performance.now() + offset });
        pipeline.resync({ epoch: data.epoch });
        pipeline.setCodec(data.codec);
        post(pipeline.attach({ main: data.main }) ? { type: "worker-ready" }
          : { type: "worker-init-error", message: "OffscreenCanvas 2D context unavailable" });
        break;
      }
      case "frame": pipeline?.handleFrame(data); break;
      case "resync": pipeline?.resync(data); break;
      default: break;
    }
  } catch (error) {
    post({ type: "worker-error", message: String(error?.message || error) });
  }
};
