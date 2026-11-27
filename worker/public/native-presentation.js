// Presentation counters of one native <video> stream, one object per peer generation.
export function newNativePresentation(generation, supported) {
  return {
    generation,
    supported,
    firstFrameAtMs: null,
    firstFramePerformanceMs: null,
    framesPresented: 0,
    fps: 0,
    recentFrameGapMs: null,
    maxFrameGapMs: 0,
    width: 0,
    height: 0,
    mediaTime: null,
    presentationTime: null,
    expectedDisplayTime: null,
    processingDurationMs: null,
    lastFrameAtMs: null,
    rtpTimestamps: false,
    lastRtpTimestamp: null,
  };
}
