package de.bubblebuffer.bubbleplay.flutter

import de.bubblebuffer.bubbleplay.input.GestureAction
import de.bubblebuffer.bubbleplay.input.RemoteInput
import de.bubblebuffer.bubbleplay.input.TouchGestures
import de.bubblebuffer.bubbleplay.input.TouchMode
import de.bubblebuffer.bubbleplay.input.Viewport
import de.bubblebuffer.bubbleplay.input.WheelAccumulator
import kotlin.math.roundToInt

/** The part of [RemoteInput] the stream screen uses; a fake in tests. */
internal interface InputSink {
    fun key(code: String, up: Boolean): Boolean
    fun absoluteNormalized(nx: Float, ny: Float)
    fun buttonsAt(x: Int, y: Int, mask: Int): Boolean
    val buttonMask: Int
    fun motion(dx: Int, dy: Int, wheelX: Int, wheelY: Int)
    fun flushMotion(): Boolean
    fun hasPendingInput(): Boolean

    /** Input became usable after a presented frame: resume a held transition, resync the cursor. */
    fun inputReady()

    /** The socket that carried input is gone: forget what is held, send nothing. */
    fun inputLost()
    fun releaseAll()
}

internal class RemoteInputSink(private val remote: RemoteInput) : InputSink {
    override fun key(code: String, up: Boolean) = remote.key(code, up)
    override fun absoluteNormalized(nx: Float, ny: Float) = remote.absoluteNormalized(nx, ny)
    override fun buttonsAt(x: Int, y: Int, mask: Int) = remote.buttonsAt(x, y, mask)
    override val buttonMask: Int get() = remote.buttonMask
    override fun motion(dx: Int, dy: Int, wheelX: Int, wheelY: Int) = remote.motion(dx, dy, wheelX, wheelY)
    override fun flushMotion() = remote.flushMotion()
    override fun hasPendingInput() = remote.hasPendingInput()
    override fun inputReady() {
        remote.resumeAfterTransition()
        remote.cursorSync()
    }

    override fun inputLost() = remote.releaseAll(send = false)
    override fun releaseAll() = remote.releaseAll()
}

/** Frame and timer scheduling the input path needs; the main thread in production. */
internal interface InputTimers {
    fun now(): Long
    fun postFrame(task: Runnable)
    fun postAt(uptimeMillis: Long, task: Runnable)
    fun cancel(task: Runnable)
}

/**
 * Touch and key events from Dart into the Kotlin input path of the native app. The finger is
 * the pointer (the DIRECT gesture mode of `TouchGestures`): a tap clicks where touched, a long
 * press then drag drags, two fingers scroll. Pinch and pan are local zoom of the native view
 * and have no meaning for a Flutter texture, so they are dropped. Main thread only.
 */
internal class StreamInput(
    private val sink: () -> InputSink?,
    private val timers: InputTimers,
    private val notchLogicalPx: Float = SCROLL_PX_PER_NOTCH,
) {
    private val gestures = TouchGestures(TouchMode.DIRECT)
    private val viewport = Viewport()
    private val wheel = WheelAccumulator()
    private var videoWidth = 0
    private var videoHeight = 0
    private var cursorX = 0f
    private var cursorY = 0f
    private var flushPosted = false

    private val longPress = Runnable {
        sink()?.let { apply(gestures.onLongPress(timers.now()), it) }
    }
    private val frame = Runnable {
        flushPosted = false
        val remote = sink() ?: return@Runnable
        remote.flushMotion()
        if (remote.hasPendingInput()) scheduleFlush()
    }

    /** The stream size, to map area coordinates onto the picture. */
    fun videoSize(width: Int, height: Int) {
        videoWidth = width
        videoHeight = height
    }

    /** Asks for a flush on the next frame (also what `RemoteInput` calls for deferred input). */
    fun scheduleFlush() {
        if (flushPosted) return
        flushPosted = true
        timers.postFrame(frame)
    }

    /** Forget gestures in progress (the session or its input socket went away). */
    fun reset() {
        timers.cancel(longPress)
        gestures.cancel()
        wheel.reset()
    }

    fun pointer(args: Map<String, Any?>) {
        val remote = sink() ?: return
        val kind = args["kind"] as? String ?: return
        val id = num(args["id"]).toInt()
        val x = num(args["x"]).toFloat()
        val y = num(args["y"]).toFloat()
        viewport.layout(num(args["areaWidth"]).roundToInt(), num(args["areaHeight"]).roundToInt(), videoWidth, videoHeight)
        val now = timers.now()
        when (kind) {
            "down" -> run(gestures.down(id, x, y, now), remote)
            "move" -> run(gestures.move(id, x, y), remote)
            "up" -> run(gestures.up(id, now), remote)
            "cancel" -> run(gestures.cancel(), remote)
            "scroll" -> {
                // Flutter: positive dy scrolls down; the wheel counts positive as up.
                val (wx, wy) = wheel.add(
                    num(args["dx"]).toFloat() * WHEEL_NOTCH / notchLogicalPx,
                    -num(args["dy"]).toFloat() * WHEEL_NOTCH / notchLogicalPx,
                )
                if (wx != 0 || wy != 0) {
                    remote.motion(0, 0, wx, wy)
                    scheduleFlush()
                }
            }
        }
    }

    /** A physical key. [usage] is the USB HID usage; modifiers arrive as keys of their own. */
    fun key(usage: Int, down: Boolean) {
        val remote = sink() ?: return
        val code = HidKeys.code(usage) ?: return
        remote.key(code, !down)
        scheduleFlush()
    }

    private fun run(actions: List<GestureAction>, remote: InputSink) {
        timers.cancel(longPress)
        gestures.longPressDeadline?.let { timers.postAt(it, longPress) }
        apply(actions, remote)
    }

    private fun apply(actions: List<GestureAction>, remote: InputSink) {
        for (action in actions) when (action) {
            is GestureAction.PointAt -> {
                val (nx, ny) = viewport.toNormalized(action.x, action.y)
                cursorX = nx.coerceIn(0f, 1f)
                cursorY = ny.coerceIn(0f, 1f)
                remote.absoluteNormalized(cursorX, cursorY)
                scheduleFlush()
            }
            is GestureAction.Press -> buttons(remote, remote.buttonMask or action.button)
            is GestureAction.Release -> buttons(remote, remote.buttonMask and action.button.inv())
            is GestureAction.Click -> {
                buttons(remote, action.button)
                buttons(remote, 0)
            }
            is GestureAction.Scroll -> {
                // Natural scrolling: content follows the fingers. Finger deltas are logical px.
                val (wx, wy) = wheel.add(-action.dx * WHEEL_NOTCH / notchLogicalPx, action.dy * WHEEL_NOTCH / notchLogicalPx)
                if (wx != 0 || wy != 0) {
                    remote.motion(0, 0, wx, wy)
                    scheduleFlush()
                }
            }
            // Trackpad motion, local zoom/pan and cues: not used in direct mode, or no view to move.
            is GestureAction.Move, is GestureAction.Zoom, is GestureAction.Pan,
            GestureAction.LongPressArmed, GestureAction.ResetZoom -> Unit
        }
    }

    private fun buttons(remote: InputSink, mask: Int) {
        remote.flushMotion()
        remote.buttonsAt((cursorX * 65535f).roundToInt(), (cursorY * 65535f).roundToInt(), mask)
        if (remote.hasPendingInput()) scheduleFlush()
    }

    private fun num(value: Any?): Double = (value as? Number)?.toDouble() ?: 0.0

    companion object {
        /** The native app scrolls one notch per 40 dp of finger travel. */
        const val SCROLL_PX_PER_NOTCH = 40f
        private const val WHEEL_NOTCH = 120f
    }
}
