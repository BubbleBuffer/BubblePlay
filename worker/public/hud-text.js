// Text of the diagnostics HUD overlay. Pure: the receiver passes its counters and applies the result.

const BITS_PER_KILOBIT = 1_000;
const UNKNOWN = "…";

export function formatQuality(quality) {
  if (!quality) return `quality ${UNKNOWN}`;
  const target = `${Math.round(quality.targetBitrateBps / BITS_PER_KILOBIT)}k`;
  const bwe = quality.bweBps === null ? UNKNOWN : `${Math.round(quality.bweBps / BITS_PER_KILOBIT)}k`;
  return `quality ${quality.profile} ${quality.width}x${quality.height}@${quality.fps} ${target}/${bwe} ${quality.reason}`;
}

// h: the HUD counters; extra: { clientBuild, fullFrame, sharp: { state, revision, crop }, hostQuality }.
export function formatHud(h, extra) {
  return [
    `ws ${h.ws} | ice ${h.ice} | route ${h.route} | control ${h.control} motion ${h.motion}`,
    `zoom ${h.zoom}× | pointer ${h.pointer} ${h.cursorSeq} ${h.cursorAge}ms | motion ${h.motionSent}/${h.motionCoalesced}/${h.motionDropped} | touch ${h.touch}`,
    `decoder ${h.dec} | recv ${h.recv} | queue ${h.queued} | drop ${h.dropped} | reset ${h.resyncs} | key ${h.keyframes} | ${h.fps} fps`,
    `presented ${h.presented} | gap ${h.frameGap}/${h.maxFrameGap}ms | ${h.dimensions}`,
    `lag ${h.lag}ms | rtt ${h.rtt}ms | jitter ${h.jit}ms | jbuf ${h.jbuf}ms | loss ${h.loss}% | rx ${h.mbps}Mbps`,
    `${extra.clientBuild} | ${extra.fullFrame ? "full-frame" : "sharp-zoom"} | media g${h.mediaGeneration} | view ${extra.sharp.state} r${extra.sharp.revision} ${extra.sharp.crop}`,
    formatQuality(extra.hostQuality),
  ].join("\n");
}
