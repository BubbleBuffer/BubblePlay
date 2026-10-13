// Host -> viewer PC audio (BPA1). Over WSS: one Opus packet per binary message
// on the media socket, told apart from BPV4 video by magic. Over WebRTC: an
// Opus track on the peer. The contract lives in fixtures/audio-contract.json.
// Audio never takes part in the v4 ACK, sequence or keyframe logic and never
// gates video.
//
// Browser objects (AudioContext, AudioDecoder, media elements, event targets)
// are passed in, so node tests can drive everything with fakes.

export const AUDIO_MAGIC = 0x42504131;
export const AUDIO_VERSION = 1;
export const AUDIO_HEADER_BYTES = 28;
export const AUDIO_MAX_PAYLOAD_BYTES = 1500;
export const AUDIO_FLAG_DISCONTINUITY = 0x01;
export const AUDIO_CODEC_OPUS = 1;
export const AUDIO_CHANNELS = 2;
export const AUDIO_SAMPLE_RATE = 48_000;
export const AUDIO_FRAME_SAMPLES = 480;
export const AUDIO_LEAD_MS = 40;
export const AUDIO_MAX_AHEAD_MS = 150;
const MAX_SAFE_TIMESTAMP = 9_007_199_254_740_991n;
const GESTURE_EVENTS = ["pointerdown", "click", "keydown"];
const AUDIO_STAT_KEYS = ["packets", "payloadBytes", "rejected", "failures", "framesDecoded",
  "framesScheduled", "droppedCap", "scheduleResets"];

function bytesOf(value) {
  if (value instanceof ArrayBuffer) return new Uint8Array(value);
  if (ArrayBuffer.isView(value)) return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
  return null;
}

/** True when the packet starts with the BPA1 magic. Does not validate the rest. */
export function isAudioPacket(packet) {
  const bytes = bytesOf(packet);
  return bytes !== null && bytes.byteLength >= 4 &&
    new DataView(bytes.buffer, bytes.byteOffset, 4).getUint32(0) === AUDIO_MAGIC;
}

/** Parse one BPA1 packet; returns null for anything the contract rejects. */
export function parseAudioPacket(packet) {
  const bytes = bytesOf(packet);
  if (!bytes || bytes.byteLength <= AUDIO_HEADER_BYTES ||
      bytes.byteLength > AUDIO_HEADER_BYTES + AUDIO_MAX_PAYLOAD_BYTES) return null;
  const view = new DataView(bytes.buffer, bytes.byteOffset, AUDIO_HEADER_BYTES);
  const flags = view.getUint8(5);
  if (view.getUint32(0) !== AUDIO_MAGIC || view.getUint8(4) !== AUDIO_VERSION ||
      (flags & ~AUDIO_FLAG_DISCONTINUITY) !== 0 || view.getUint8(6) !== AUDIO_CODEC_OPUS ||
      view.getUint8(7) !== AUDIO_CHANNELS || view.getUint32(8) !== AUDIO_SAMPLE_RATE ||
      view.getUint16(24) !== AUDIO_FRAME_SAMPLES || view.getUint16(26) !== 0) return null;
  return {
    discontinuity: (flags & AUDIO_FLAG_DISCONTINUITY) !== 0,
    codec: AUDIO_CODEC_OPUS,
    channels: AUDIO_CHANNELS,
    sampleRate: AUDIO_SAMPLE_RATE,
    seq: view.getUint32(12),
    captureTsUs: view.getBigUint64(16),
    frameSamples: AUDIO_FRAME_SAMPLES,
    data: bytes.subarray(AUDIO_HEADER_BYTES),
  };
}

/**
 * Playout schedule for decoded Opus. Times are AudioContext seconds.
 * receive() tracks the packet sequence (u32, wrapping) and resets the schedule
 * on a discontinuity flag or a gap. schedule() gives each decoded packet a
 * start time about AUDIO_LEAD_MS ahead, or null (drop) while more than
 * AUDIO_MAX_AHEAD_MS is already queued.
 */
export class AudioPlayout {
  constructor({ leadS = AUDIO_LEAD_MS / 1000, maxAheadS = AUDIO_MAX_AHEAD_MS / 1000 } = {}) {
    this.leadS = leadS;
    this.maxAheadS = maxAheadS;
    // Mid-stream resets (a gap or a discontinuity flag). The first packet and
    // reset() calls are not counted.
    this.resets = 0;
    this.reset();
  }

  reset() {
    this.expectedSeq = null;
    this.nextAt = null;
  }

  /** @returns {"accept"|"reset"} */
  receive(seq, discontinuity = false) {
    const reset = discontinuity || this.expectedSeq === null || seq !== this.expectedSeq;
    if (reset && this.expectedSeq !== null) this.resets++;
    if (reset) this.nextAt = null;
    this.expectedSeq = (seq + 1) >>> 0;
    return reset ? "reset" : "accept";
  }

  /** @returns {number|null} start time for this packet, or null to drop it */
  schedule(now, durationS) {
    if (this.nextAt === null || this.nextAt < now) this.nextAt = now + this.leadS;
    if (this.nextAt - now > this.maxAheadS) return null;
    const startAt = this.nextAt;
    this.nextAt += durationS;
    return startAt;
  }
}

/**
 * WSS audio: parse, decode and schedule BPA1 packets. Create-on-first-packet:
 * the AudioContext and decoder are built when audio arrives, and stop() closes
 * both. createContext() -> AudioContext-like; createDecoder({output, error}) ->
 * AudioDecoder-like; createChunk(init) -> EncodedAudioChunk-like.
 */
export class WssAudioPlayer {
  constructor({ createContext, createDecoder, createChunk, onError = () => {}, playout = new AudioPlayout() }) {
    Object.assign(this, { createContext, createDecoder, createChunk, onError, playout });
    this.context = null;
    this.gain = null;
    this.decoder = null;
    this.muted = false;
    this.dropped = 0;
    this.counts = Object.fromEntries(AUDIO_STAT_KEYS.map(key => [key, 0]));
  }

  get running() { return this.decoder !== null; }

  /** @returns {"decoded"|"muted"|"rejected"} Sequence tracking continues while muted. */
  push(packet) {
    this.counts.packets++;
    const audio = parseAudioPacket(packet);
    if (!audio) {
      this.counts.rejected++;
      return "rejected";
    }
    this.counts.payloadBytes += audio.data.byteLength;
    this.playout.receive(audio.seq, audio.discontinuity);
    if (!this.#ensure()) return "rejected";
    if (this.muted) return "muted";
    try {
      this.decoder.decode(this.createChunk({
        type: "key",
        timestamp: Number(audio.captureTsUs % MAX_SAFE_TIMESTAMP),
        data: audio.data,
      }));
    } catch (error) {
      this.#fail(error);
      return "rejected";
    }
    return "decoded";
  }

  /**
   * Plain counters since this player was created. packets = every BPA1 packet
   * pushed; payloadBytes = Opus bytes of parsed packets; rejected = failed
   * parseAudioPacket; failures = decoder/context errors (each one stops the
   * player); framesDecoded = AudioDecoder output; framesScheduled and droppedCap
   * split those frames; scheduleResets = mid-stream gap or discontinuity resets.
   */
  stats() {
    return {
      ...this.counts,
      droppedCap: this.dropped,
      scheduleResets: this.playout.resets ?? 0,
    };
  }

  setMuted(muted) {
    const wasMuted = this.muted;
    this.muted = muted === true;
    if (this.gain) this.gain.gain.value = this.muted ? 0 : 1;
    // Audio from before the mute is not replayed: unmuting starts a fresh schedule.
    if (wasMuted && !this.muted) this.playout.reset();
  }

  /** Resume inside a user gesture. True when the context is running. */
  async resume() {
    if (!this.context) return false;
    try { await this.context.resume(); } catch { /* stays suspended */ }
    return this.context.state === "running";
  }

  stop() {
    try { this.decoder?.close(); } catch { /* already closed */ }
    try { this.context?.close()?.catch?.(() => {}); } catch { /* already closed */ }
    this.decoder = null;
    this.context = null;
    this.gain = null;
    this.playout.reset();
  }

  #ensure() {
    if (this.decoder) return true;
    try {
      this.context = this.createContext();
      this.gain = this.context.createGain();
      this.gain.gain.value = this.muted ? 0 : 1;
      this.gain.connect(this.context.destination);
      this.decoder = this.createDecoder({
        output: data => this.#output(data),
        error: error => this.#fail(error),
      });
      this.decoder.configure({ codec: "opus", sampleRate: AUDIO_SAMPLE_RATE, numberOfChannels: AUDIO_CHANNELS });
      return true;
    } catch (error) {
      this.#fail(error);
      return false;
    }
  }

  #output(audioData) {
    this.counts.framesDecoded++;
    if (!this.context) { audioData.close?.(); return; }
    try {
      const { numberOfChannels: channels, numberOfFrames: frames, sampleRate } = audioData;
      const buffer = this.context.createBuffer(channels, frames, sampleRate);
      for (let channel = 0; channel < channels; channel++) {
        const samples = new Float32Array(frames);
        audioData.copyTo(samples, { planeIndex: channel, format: "f32-planar" });
        buffer.copyToChannel(samples, channel);
      }
      const startAt = this.playout.schedule(this.context.currentTime, frames / sampleRate);
      if (startAt === null) {
        this.dropped++;
        return;
      }
      this.counts.framesScheduled++;
      const source = this.context.createBufferSource();
      source.buffer = buffer;
      source.connect(this.gain);
      source.start(startAt);
    } catch (error) {
      this.#fail(error);
    } finally {
      audioData.close?.();
    }
  }

  #fail(error) {
    this.counts.failures++;
    this.onError(error);
    this.stop();
  }
}

/** Sums stats() objects. Missing or null sets count as zero. */
export function sumAudioStats(...sets) {
  const total = Object.fromEntries(AUDIO_STAT_KEYS.map(key => [key, 0]));
  for (const set of sets) {
    if (!set) continue;
    for (const key of AUDIO_STAT_KEYS) total[key] += set[key] ?? 0;
  }
  return total;
}

/** Resolves true when this browser can decode the BPA1 Opus configuration. */
export async function probeOpusDecode(AudioDecoderCtor) {
  if (typeof AudioDecoderCtor !== "function" || typeof AudioDecoderCtor.isConfigSupported !== "function") return false;
  try {
    const support = await AudioDecoderCtor.isConfigSupported({
      codec: "opus", sampleRate: AUDIO_SAMPLE_RATE, numberOfChannels: AUDIO_CHANNELS,
    });
    return support?.supported === true;
  } catch {
    return false;
  }
}

/**
 * The strip button for local sound. state is the controller state. "silent"
 * (muted or blocked) shows the muted icon; "pressed" is set only for a user mute.
 */
export function soundButtonView({ capable = false, state = "off" } = {}) {
  const silent = state === "muted" || state === "blocked";
  return {
    hidden: !capable,
    pressed: state === "muted",
    silent,
    label: silent ? "Unmute sound" : "Mute sound",
  };
}

/**
 * Local sound state for the session. Sinks are {start(), setMuted(bool), stop()}:
 * start() resolves true when the sink is playing (false when the browser blocks
 * it). A blocked controller starts again on the first pointerdown, click or
 * keydown on target, or on a mute click, and then plays unmuted.
 * States: "off" (no sink), "playing", "muted", "blocked".
 */
export class SoundController {
  #gesture = () => { this.unblock(); };
  #listening = false;

  constructor({ target = null, onChange = () => {} } = {}) {
    this.target = target;
    this.onChange = onChange;
    this.sinks = new Map();
    this.muted = false;
    this.blocked = false;
  }

  get state() {
    if (this.sinks.size === 0) return "off";
    if (this.blocked) return "blocked";
    return this.muted ? "muted" : "playing";
  }

  /** Adds or replaces a named sink and starts it. Resolves true when playing. */
  attachSink(name, sink) {
    const previous = this.sinks.get(name);
    if (previous) {
      try { previous.stop?.(); } catch { /* already gone */ }
    }
    this.sinks.set(name, sink);
    try { sink.setMuted?.(this.muted); } catch { /* sink refuses mute state */ }
    return this.#start();
  }

  detachSink(name) {
    const sink = this.sinks.get(name);
    if (!sink) return;
    this.sinks.delete(name);
    try { sink.stop?.(); } catch { /* already gone */ }
    if (this.sinks.size === 0) this.blocked = false;
    this.#listen(this.blocked);
    this.#emit();
  }

  /** Starts playback unmuted when blocked. Called from user gestures. */
  unblock() {
    if (!this.blocked) return Promise.resolve(true);
    return this.#start();
  }

  /** Mute click: unblocks when blocked, otherwise toggles the local mute. */
  toggle() {
    if (this.blocked) return this.unblock();
    this.muted = !this.muted;
    for (const sink of this.sinks.values()) {
      try { sink.setMuted?.(this.muted); } catch { /* sink refuses mute state */ }
    }
    this.#emit();
    return Promise.resolve(!this.muted);
  }

  dispose() {
    for (const name of [...this.sinks.keys()]) this.detachSink(name);
    this.#listen(false);
  }

  async #start() {
    const sinks = [...this.sinks.values()];
    const started = await Promise.all(sinks.map(async sink => {
      try { return (await sink.start()) === true; } catch { return false; }
    }));
    const ok = started.length > 0 && started.every(Boolean);
    this.blocked = this.sinks.size > 0 && !ok;
    this.#listen(this.blocked);
    this.#emit();
    return ok;
  }

  #listen(on) {
    if (!this.target || on === this.#listening) return;
    this.#listening = on;
    for (const type of GESTURE_EVENTS) {
      if (on) this.target.addEventListener(type, this.#gesture);
      else this.target.removeEventListener(type, this.#gesture);
    }
  }

  #emit() { this.onChange(this.state); }
}
