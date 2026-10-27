import test from "node:test";
import assert from "node:assert/strict";
import { createCodecSession } from "../public/transport.js";

test("diagnostic WebRTC selects H264 while native WebRTC and WSS retain HEVC", () => {
  assert.equal(createCodecSession({ transport: "webrtc", decoderPath: "webcodecs" }).codec, "h264");
  assert.equal(createCodecSession({ transport: "webrtc", decoderPath: "native" }).codec, "hevc");
  assert.equal(createCodecSession({ transport: "wss", decoderPath: "webcodecs" }).codec, "hevc");
});
