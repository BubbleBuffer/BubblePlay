package de.bubblebuffer.bubbleplay.media

import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioTrack
import android.media.MediaCodec
import android.media.MediaCodecList
import android.media.MediaFormat
import android.os.Handler
import android.os.HandlerThread
import android.util.Log
import de.bubblebuffer.bubbleplay.protocol.AudioPacket
import de.bubblebuffer.bubbleplay.protocol.AudioWire
import java.nio.ByteBuffer

/**
 * Host sound: BPA1 Opus packets -> MediaCodec (audio/opus) -> AudioTrack, on
 * its own thread. Created on the first audio packet and released with the
 * media leg. Muting and backgrounding set the track volume to 0; packets keep
 * being decoded so the queue stays short.
 */
class AudioPlayer(audible: Boolean) {
    private val thread = HandlerThread("bp-audio").apply { start() }
    private val handler = Handler(thread.looper)
    private var audible = audible
    private var codec: MediaCodec? = null
    private var track: AudioTrack? = null
    private var failed = false
    private var writtenFrames = 0L
    private var ptsUs = 0L
    private val drop = AudioDropPolicy()
    private val sequence = AudioSequence()
    private val info = MediaCodec.BufferInfo()

    fun offer(packet: AudioPacket) = handler.post { onPacket(packet) }

    fun setAudible(value: Boolean) = handler.post {
        audible = value
        track?.setVolume(if (value) 1f else 0f)
    }

    fun release() {
        handler.post { releaseAll() }
        thread.quitSafely()
    }

    private fun onPacket(packet: AudioPacket) {
        if (failed) return
        // Every received packet is counted, including the ones the drop policy skips below.
        if (packet.discontinuity || sequence.observe(packet.seq)) Log.d(TAG, "audio gap or discontinuity at seq ${packet.seq}")
        if (!ensureOpen()) return
        val t = track ?: return
        val queuedMs = AudioPolicy.framesToMs(AudioPolicy.queuedFrames(writtenFrames, t.playbackHeadPosition))
        if (drop.shouldDrop(queuedMs)) return
        decode(packet.payload)
    }

    private fun decode(payload: ByteArray) {
        val c = codec ?: return
        val index = c.dequeueInputBuffer(0)
        if (index >= 0) {
            val buffer = c.getInputBuffer(index)
            if (buffer == null) { fail("input-buffer"); return }
            buffer.clear()
            buffer.put(payload)
            c.queueInputBuffer(index, 0, payload.size, ptsUs, 0)
            ptsUs += FRAME_US
        }
        // Drain whatever is ready; output that is not ready yet comes with a later packet.
        while (true) {
            val out = c.dequeueOutputBuffer(info, 0)
            if (out == MediaCodec.INFO_TRY_AGAIN_LATER) return
            if (out < 0) continue // output format or buffers changed
            if (info.size > 0) c.getOutputBuffer(out)?.let { write(it, info.offset, info.size) }
            c.releaseOutputBuffer(out, false)
        }
    }

    private fun write(buffer: ByteBuffer, offset: Int, size: Int) {
        val t = track ?: return
        val view = buffer.duplicate()
        view.position(offset)
        view.limit(offset + size)
        val bytes = ByteArray(size)
        view.get(bytes)
        // Non-blocking: when the track is full the rest of the packet is lost rather than delaying video.
        val written = t.write(bytes, 0, size, AudioTrack.WRITE_NON_BLOCKING)
        if (written > 0) writtenFrames += written / BYTES_PER_FRAME
    }

    /** Opens the decoder and track on first use. False when that failed (no sound for this player). */
    private fun ensureOpen(): Boolean {
        if (codec != null && track != null) return true
        val c = try { MediaCodec.createDecoderByType(MIME) } catch (e: Exception) { return fail("create-decoder", e) }
        try {
            c.configure(opusFormat(), null, null, 0)
            c.start()
            codec = c
            val t = createTrack()
            track = t
            t.setVolume(if (audible) 1f else 0f)
            t.play()
            Log.i(TAG, "audio decoder ${c.name} at ${AudioWire.SAMPLE_RATE} Hz, ${AudioWire.CHANNELS} ch")
            return true
        } catch (e: Exception) {
            try { c.release() } catch (_: Exception) {}
            return fail("open", e)
        }
    }

    private fun opusFormat(): MediaFormat =
        MediaFormat.createAudioFormat(MIME, AudioWire.SAMPLE_RATE, AudioWire.CHANNELS).apply {
            setByteBuffer("csd-0", ByteBuffer.wrap(AudioWire.opusHead()))
            setByteBuffer("csd-1", ByteBuffer.wrap(AudioWire.longLittleEndian(AudioWire.PRE_SKIP_NS)))
            setByteBuffer("csd-2", ByteBuffer.wrap(AudioWire.longLittleEndian(AudioWire.SEEK_PRE_ROLL_NS)))
            setInteger(MediaFormat.KEY_PCM_ENCODING, AudioFormat.ENCODING_PCM_16BIT)
        }

    private fun createTrack(): AudioTrack {
        val channels = AudioFormat.CHANNEL_OUT_STEREO
        val minBytes = AudioTrack.getMinBufferSize(AudioWire.SAMPLE_RATE, channels, AudioFormat.ENCODING_PCM_16BIT)
        val track = AudioTrack.Builder()
            .setAudioAttributes(AudioAttributes.Builder()
                .setUsage(AudioAttributes.USAGE_MEDIA)
                .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
                .build())
            .setAudioFormat(AudioFormat.Builder()
                .setSampleRate(AudioWire.SAMPLE_RATE)
                .setChannelMask(channels)
                .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                .build())
            .setBufferSizeInBytes(maxOf(minBytes, MIN_BUFFER_FRAMES * BYTES_PER_FRAME))
            .setTransferMode(AudioTrack.MODE_STREAM)
            .setPerformanceMode(AudioTrack.PERFORMANCE_MODE_LOW_LATENCY)
            .build()
        if (track.state != AudioTrack.STATE_INITIALIZED) {
            track.release()
            throw IllegalStateException("AudioTrack not initialized")
        }
        return track
    }

    private fun fail(step: String, e: Exception? = null): Boolean {
        Log.w(TAG, "audio $step failed; no sound for this session", e)
        failed = true
        releaseAll()
        return false
    }

    private fun releaseAll() {
        try { codec?.stop() } catch (_: Exception) {}
        try { codec?.release() } catch (_: Exception) {}
        codec = null
        try { track?.stop() } catch (_: Exception) {}
        try { track?.release() } catch (_: Exception) {}
        track = null
        writtenFrames = 0L
        ptsUs = 0L
        drop.reset()
        sequence.reset()
    }

    companion object {
        private const val TAG = "BubblePlayAudio"
        private const val MIME = "audio/opus"
        private const val BYTES_PER_FRAME = 4 // 16-bit stereo
        private const val FRAME_US = 10_000L // 480 samples at 48 kHz
        /** Track buffer floor: 4 packets, 40 ms. */
        private const val MIN_BUFFER_FRAMES = 4 * AudioWire.FRAME_SAMPLES

        /** Whether this device has an Opus decoder; checked once. */
        val decoderAvailable: Boolean by lazy {
            MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos
                .any { !it.isEncoder && it.supportedTypes.any { t -> t.equals(MIME, true) } }
        }
    }
}
