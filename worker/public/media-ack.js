// Own the single pending ACK timer. The snapshot is read at send time so a
// queued ACK describes current delivery, while its generation remains bound
// to the request that armed it.
export function createMediaAckScheduler({ snapshot, send, record,
  schedule = setTimeout, cancel = clearTimeout }) {
  let timer = null;
  function clear() {
    if (timer !== null) cancel(timer);
    timer = null;
  }
  function request(generation, immediate = false) {
    const flush = () => {
      clear();
      const state = snapshot();
      const message = {
        type: state.protocol === 3 ? "media-ack-v3" : "media-ack-v2",
        ...(state.protocol === 3 ? { protocol: 3, epoch: state.epoch,
          topology: state.topology, completionMissingByLane: [...state.reassembler.stats.completionMissingByLane] } : {}),
        generation, lanes: [...state.reassembler.highestSequence],
        reconstructed: state.reassembler.highestReconstructed, presented: state.presented,
        recoveredFrames: state.reassembler.stats.recoveredFrames, expiredFrames: state.reassembler.stats.expiredFrames,
        dependencyLosses: state.dependencyLosses, deadlineMs: Math.round(state.deadline.deadlineMs),
        reassemblyP95Ms: Math.round(state.deadline.p95Ms), recoveryState: state.recoveryState,
        decoderDrops: state.decoderDrops,
      };
      const sent = send(message);
      record?.(message, immediate, sent);
    };
    if (immediate) { flush(); return; }
    if (timer === null) timer = schedule(flush, 100);
  }
  return { request, clear };
}
