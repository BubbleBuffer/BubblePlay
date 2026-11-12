// WSS audio: one player per media session, attached to the sound controller as the "wss" sink
// when the first valid packet arrives. Counters of stopped players are kept so diagnostics stay
// cumulative across restarts.
import { WssAudioPlayer, sumAudioStats, AUDIO_SAMPLE_RATE } from "./audio.js";

export const WSS_AUDIO_SINK = "wss";
export const AUDIO_PACKET_REJECTED = "rejected";

// env: soundController (attachSink, detachSink), onRejected(), log(level, event, detail),
// createPlayer (optional; tests).
export function createWssAudioSession(env) {
  let player = null;
  let sinkAttached = false;
  let retired = sumAudioStats();

  const defaultPlayer = () => new WssAudioPlayer({
    createContext: () => new AudioContext({ latencyHint: "interactive", sampleRate: AUDIO_SAMPLE_RATE }),
    createDecoder: init => new AudioDecoder(init),
    createChunk: init => new EncodedAudioChunk(init),
    onError: error => env.log("warn", "wss-audio-error", { name: error?.name || "error" }),
  });

  return {
    push(data) {
      player ??= (env.createPlayer ?? defaultPlayer)();
      if (player.push(data) === AUDIO_PACKET_REJECTED) { env.onRejected(); return; }
      if (sinkAttached) return;
      sinkAttached = true;
      const current = player;
      env.soundController.attachSink(WSS_AUDIO_SINK, {
        start: () => current.resume(),
        setMuted: muted => current.setMuted(muted),
        stop: () => current.stop(),
      });
    },
    stop() {
      if (player) retired = sumAudioStats(retired, player.stats());
      player?.stop();
      player = null;
      if (sinkAttached) {
        sinkAttached = false;
        env.soundController.detachSink(WSS_AUDIO_SINK);
      }
    },
    stats() { return sumAudioStats(retired, player?.stats()); },
  };
}
