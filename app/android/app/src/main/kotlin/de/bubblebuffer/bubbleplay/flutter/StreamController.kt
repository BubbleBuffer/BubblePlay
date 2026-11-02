package de.bubblebuffer.bubbleplay.flutter

import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.util.Log
import android.view.Choreographer
import android.view.Surface
import de.bubblebuffer.bubbleplay.keys.DeviceKey
import de.bubblebuffer.bubbleplay.net.Session
import de.bubblebuffer.bubbleplay.pcs.PcEntry
import io.flutter.view.TextureRegistry

/**
 * Owns the live stream on Android: one [StreamPort] (the real session), one Flutter
 * `SurfaceProducer` texture that the decoder renders into, and the input path. Video never
 * passes through Dart. Every call can come from any thread and is run on the main thread, in order.
 */
internal class StreamController(
    private val textures: TextureRegistry,
    private val opener: StreamOpener = RealStreamOpener,
) : StreamHost {
    private val main = Handler(Looper.getMainLooper())

    private val timers = object : InputTimers {
        override fun now() = SystemClock.uptimeMillis()
        override fun postFrame(task: Runnable) {
            Choreographer.getInstance().postFrameCallback { task.run() }
        }

        override fun postAt(uptimeMillis: Long, task: Runnable) {
            main.postAtTime(task, uptimeMillis)
        }

        override fun cancel(task: Runnable) {
            main.removeCallbacks(task)
        }
    }

    @Volatile private var port: StreamPort? = null
    private var producer: TextureRegistry.SurfaceProducer? = null
    private var handoff: SurfaceHandoff<Surface>? = null
    @Volatile private var picture = false
    @Volatile private var textureValue: Int = 0

    private val input = StreamInput({ port?.input }, timers)
    private val router = StreamChannelRouter(input) { if (picture) textureValue else null }

    /** For the method channel; the result is [StreamChannelRouter.NOT_IMPLEMENTED] for an unknown method. */
    fun handle(method: String, args: Map<String, Any?>?): Any? = router.handle(method, args)

    override fun open(pc: PcEntry, key: DeviceKey, label: String, events: StreamEvents) {
        main.post { openNow(pc, key, label, events) }
    }

    override fun close() {
        main.post { closeNow() }
    }

    override fun requestMonitor(selector: String) {
        main.post { port?.requestMonitor(selector) }
    }

    override fun setGameMode(game: Boolean) {
        main.post { port?.setGameMode(game) }
    }

    override fun setSoundMuted(muted: Boolean) {
        main.post { port?.setSoundMuted(muted) }
    }

    override fun stats(): StreamStats? = port?.stats()

    /** The app went to the background (false) or came back (true). */
    fun setForeground(value: Boolean) {
        main.post { port?.setForeground(value) }
    }

    private fun openNow(pc: PcEntry, key: DeviceKey, label: String, events: StreamEvents) {
        closeNow()
        val texture = textures.createSurfaceProducer()
        textureValue = texture.id().toInt()
        picture = false
        producer = texture
        val sink = object : SurfaceHandoff.Sink<Surface> {
            override fun attach(surface: Surface) {
                port?.attachSurface(surface)
            }

            override fun detachBlocking() {
                port?.detachSurfaceBlocking()
            }
        }
        val surfaces = SurfaceHandoff(sink) { width, height -> texture.setSize(width, height) }
        handoff = surfaces
        texture.setCallback(object : TextureRegistry.SurfaceProducer.Callback {
            override fun onSurfaceAvailable() {
                if (producer === texture) surfaces.available(texture.surface)
            }

            override fun onSurfaceCleanup() {
                if (producer === texture) surfaces.cleanup()
            }
        })
        val wrapped = object : StreamEvents by events {
            override fun onVideoSize(width: Int, height: Int) {
                input.videoSize(width, height)
                surfaces.resize(width, height)
                events.onVideoSize(width, height)
            }

            override fun onLiveFrame() {
                picture = true
                events.onLiveFrame()
            }

            override fun onInputLost() {
                input.reset()
                events.onInputLost()
            }
        }
        val opened = try {
            opener.open(pc, key, label, wrapped, input::scheduleFlush)
        } catch (e: Exception) {
            Log.w(TAG, "session could not be opened: ${e.javaClass.simpleName}")
            closeNow()
            return
        }
        port = opened
        surfaces.bind()
        surfaces.available(texture.surface)
        opened.start()
    }

    private fun closeNow() {
        val surfaces = handoff
        val texture = producer
        val current = port
        // The codec lets go of the surface first, while the port is still there to do it.
        surfaces?.unbind()
        current?.input?.releaseAll()
        input.reset()
        handoff = null
        producer = null
        port = null
        picture = false
        current?.stop()
        texture?.setCallback(null)
        texture?.release()
    }

    private companion object {
        const val TAG = "BubblePlay"
    }
}
