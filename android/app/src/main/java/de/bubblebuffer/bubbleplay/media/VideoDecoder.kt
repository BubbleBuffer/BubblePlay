package de.bubblebuffer.bubbleplay.media

import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaCodecList
import android.media.MediaFormat
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.util.Log
import android.view.Surface
import de.bubblebuffer.bubbleplay.protocol.V4
import de.bubblebuffer.bubbleplay.protocol.V4Frame

/**
 * MediaCodec (async) -> Surface decoder for the v4 stream.
 *
 * Every accepted access unit is decoded in order; nothing is reordered (the
 * host sends no B-frames). Presentation is latest-wins: a decoded output is
 * held until the decoder thread goes idle, and if a newer output arrives in
 * the meantime the older one is released without rendering. No presentation
 * queue is ever built. A decoder error drops the codec and asks for a
 * keyframe; the next keyframe recreates it.
 */
class VideoDecoder(private val listener: Listener) {
    interface Listener {
        /** Called on the decoder thread after a frame is released to the surface. */
        fun onFramePresented(generation: Long, seq: Long, width: Int, height: Int, mediaLeg: Int)
        /** The decoder cannot continue from the current reference chain. */
        fun onNeedsKeyframe(reason: String)
        fun onDecoderSelected(name: String, mime: String, hardware: Boolean, lowLatency: Boolean)
        fun onSurfaceChanged(generation: Long, attached: Boolean)
        fun onSurfaceOutput(generation: Long)
    }

    private val thread = HandlerThread("bp-decoder").apply { start() }
    private val handler = Handler(thread.looper)

    private var surface: Surface? = null
    private var surfaceGeneration = 0L
    private var surfaceOutputReported = false
    private var codec: MediaCodec? = null
    private var codecType = 0
    private var width = 0
    private var height = 0
    private var csd: List<ByteArray> = emptyList()
    private val freeInputs = ArrayDeque<Int>()
    private data class QueuedFrame(val frame: V4Frame, val mediaLeg: Int)
    private val pending = ArrayDeque<QueuedFrame>()
    private val inFlight = HashMap<Long, QueuedFrame>()
    private var nextPts = 0L
    private var heldOutput = -1
    private var heldFrame: QueuedFrame? = null
    private var presentPosted = false

    @Volatile var decodedFrames = 0L; private set
    @Volatile var presentedFrames = 0L; private set
    @Volatile var skippedFrames = 0L; private set
    /** Sequence of the newest decoded output (sent back in ACKs as decodedSeq). */
    @Volatile var lastDecodedSeq: Long? = null; private set

    fun setSurface(value: Surface?) = handler.post {
        surface = value
        releaseCodec()
        pending.clear()
        surfaceOutputReported = false
        listener.onSurfaceChanged(++surfaceGeneration, value != null)
        if (value != null) listener.onNeedsKeyframe("surface")
    }

    /** Releases the codec before the surface goes away (surfaceDestroyed must not return first). */
    fun detachSurfaceBlocking() {
        val done = java.util.concurrent.CountDownLatch(1)
        val posted = handler.post {
            surface = null
            releaseCodec()
            pending.clear()
            listener.onSurfaceChanged(++surfaceGeneration, false)
            done.countDown()
        }
        if (posted) done.await(500, java.util.concurrent.TimeUnit.MILLISECONDS)
    }

    fun submit(frame: V4Frame, mediaLeg: Int) = handler.post { onFrame(QueuedFrame(frame, mediaLeg)) }

    /** Drop frames not yet queued (gate reset / reconnect). */
    fun dropPending() = handler.post { pending.clear() }

    fun release() {
        handler.post { releaseCodec(); surface = null }
        thread.quitSafely()
    }

    private fun onFrame(queued: QueuedFrame) {
        val frame = queued.frame
        if (surface == null) return
        if (frame.key) {
            val frameCsd = AnnexB.codecSpecificData(frame.codec, frame.data)
            if (codec == null || frame.codec != codecType || frame.width != width || frame.height != height ||
                (frameCsd.isNotEmpty() && !sameCsd(frameCsd, csd))
            ) {
                if (!configure(frame, frameCsd)) { listener.onNeedsKeyframe("decoder-configure"); return }
            }
        }
        if (codec == null) return
        pending.addLast(queued)
        if (pending.size > MAX_PENDING) { fail("decode-backlog"); return }
        drain()
    }

    private fun drain() {
        val c = codec ?: return
        try {
            while (freeInputs.isNotEmpty() && pending.isNotEmpty()) {
                val queued = pending.removeFirst()
                val frame = queued.frame
                val index = freeInputs.removeFirst()
                val buffer = c.getInputBuffer(index) ?: continue
                if (frame.data.size > buffer.capacity()) { fail("input-too-large"); return }
                buffer.clear()
                buffer.put(frame.data)
                val pts = nextPts
                nextPts += 16_667
                inFlight[pts] = queued
                c.queueInputBuffer(index, 0, frame.data.size, pts, if (frame.key) MediaCodec.BUFFER_FLAG_KEY_FRAME else 0)
            }
        } catch (e: IllegalStateException) {
            Log.w(TAG, "queueInputBuffer failed", e)
            fail("decoder-input")
        }
    }

    private fun configure(frame: V4Frame, frameCsd: List<ByteArray>): Boolean {
        releaseCodec()
        val target = surface ?: return false
        val mime = if (frame.codec == V4.CODEC_HEVC) MediaFormat.MIMETYPE_VIDEO_HEVC else MediaFormat.MIMETYPE_VIDEO_AVC
        val info = selectDecoder(mime)
        for (withExtras in listOf(true, false)) {
            var created: MediaCodec? = null
            try {
                created = if (info != null) MediaCodec.createByCodecName(info.name) else MediaCodec.createDecoderByType(mime)
                val format = MediaFormat.createVideoFormat(mime, frame.width, frame.height)
                frameCsd.forEachIndexed { i, bytes -> format.setByteBuffer("csd-$i", java.nio.ByteBuffer.wrap(bytes)) }
                format.setInteger(MediaFormat.KEY_MAX_INPUT_SIZE, maxOf(frame.width * frame.height * 3 / 2, frame.data.size * 2, 1 shl 21))
                var lowLatency = false
                if (withExtras) {
                    format.setInteger(MediaFormat.KEY_PRIORITY, 0)
                    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                        format.setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
                        lowLatency = info?.getCapabilitiesForType(mime)
                            ?.isFeatureSupported(MediaCodecInfo.CodecCapabilities.FEATURE_LowLatency) == true
                    }
                    // Vendor low-latency switches; unknown keys are ignored.
                    format.setInteger("vendor.qti-ext-dec-low-latency.enable", 1)
                    format.setInteger("vendor.low-latency.enable", 1)
                }
                val c = created
                c.setCallback(callback(c), handler)
                c.configure(format, target, null, 0)
                c.start()
                codec = c
                codecType = frame.codec; width = frame.width; height = frame.height; csd = frameCsd
                freeInputs.clear(); inFlight.clear()
                listener.onDecoderSelected(c.name, mime, info?.isHardwareAccelerated == true, lowLatency)
                return true
            } catch (e: Exception) {
                Log.w(TAG, "configure $mime (extras=$withExtras) failed", e)
                try { created?.release() } catch (_: Exception) {}
            }
        }
        return false
    }

    private fun callback(owner: MediaCodec) = object : MediaCodec.Callback() {
        override fun onInputBufferAvailable(c: MediaCodec, index: Int) {
            if (c !== codec || c !== owner) return
            freeInputs.addLast(index)
            drain()
        }

        override fun onOutputBufferAvailable(c: MediaCodec, index: Int, info: MediaCodec.BufferInfo) {
            if (c !== codec || c !== owner) return
            val queued = inFlight.remove(info.presentationTimeUs)
            val frame = queued?.frame
            if (info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG != 0 || info.size < 0) {
                try { c.releaseOutputBuffer(index, false) } catch (_: IllegalStateException) {}
                return
            }
            decodedFrames++
            if (frame != null && !surfaceOutputReported) {
                surfaceOutputReported = true
                listener.onSurfaceOutput(surfaceGeneration)
            }
            if (frame != null) lastDecodedSeq = frame.seq
            // Latest wins: a newer decoded frame replaces the held one unpainted.
            if (heldOutput >= 0) {
                try { c.releaseOutputBuffer(heldOutput, false) } catch (_: IllegalStateException) {}
                skippedFrames++
            }
            heldOutput = index
            heldFrame = queued
            if (!presentPosted) {
                presentPosted = true
                handler.post { presentHeld(owner) }
            }
        }

        override fun onError(c: MediaCodec, e: MediaCodec.CodecException) {
            if (c !== codec) return
            Log.w(TAG, "decoder error", e)
            fail("decoder-error")
        }

        override fun onOutputFormatChanged(c: MediaCodec, format: MediaFormat) {}
    }

    private fun presentHeld(owner: MediaCodec) {
        presentPosted = false
        val c = codec
        val index = heldOutput
        val queued = heldFrame
        heldOutput = -1
        heldFrame = null
        if (c == null || c !== owner || index < 0) return
        try {
            c.releaseOutputBuffer(index, true)
        } catch (e: IllegalStateException) {
            fail("decoder-output"); return
        }
        presentedFrames++
        if (queued != null) listener.onFramePresented(queued.frame.generation, queued.frame.seq, queued.frame.width, queued.frame.height, queued.mediaLeg)
    }

    private fun fail(reason: String) {
        releaseCodec()
        pending.clear()
        listener.onNeedsKeyframe(reason)
    }

    private fun releaseCodec() {
        val c = codec ?: return
        codec = null
        heldOutput = -1; heldFrame = null
        freeInputs.clear(); inFlight.clear()
        try { c.stop() } catch (_: Exception) {}
        try { c.release() } catch (_: Exception) {}
    }

    private fun sameCsd(a: List<ByteArray>, b: List<ByteArray>) =
        a.size == b.size && a.indices.all { a[it].contentEquals(b[it]) }

    companion object {
        private const val TAG = "BubblePlayDecoder"
        private const val MAX_PENDING = 90

        /** Decoder for [mime]: hardware first, software otherwise. */
        fun selectDecoder(mime: String): MediaCodecInfo? {
            val decoders = MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos
                .filter { !it.isEncoder && !it.isAlias && it.supportedTypes.any { t -> t.equals(mime, true) } }
            return decoders.firstOrNull { it.isHardwareAccelerated } ?: decoders.firstOrNull()
        }

        fun hasHardwareDecoder(mime: String): Boolean =
            MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos
                .any { !it.isEncoder && it.isHardwareAccelerated && it.supportedTypes.any { t -> t.equals(mime, true) } }
    }
}
