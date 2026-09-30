package de.bubblebuffer.bubbleplay.input

import kotlin.math.abs
import kotlin.math.hypot

/** What a touch sequence means; the stream screen turns these into packets. */
sealed interface GestureAction {
    /** Trackpad: move the remote cursor by a finger delta in screen pixels. */
    data class Move(val dx: Float, val dy: Float) : GestureAction
    /** Direct: put the remote cursor under this screen point. */
    data class PointAt(val x: Float, val y: Float) : GestureAction
    data class Press(val button: Int) : GestureAction
    data class Release(val button: Int) : GestureAction
    data class Click(val button: Int) : GestureAction
    /** Two-finger drag: finger delta in screen pixels (natural scrolling). */
    data class Scroll(val dx: Float, val dy: Float) : GestureAction
    /** Pinch: local view zoom around a focus point; never sent to the host. */
    data class Zoom(val scale: Float, val focusX: Float, val focusY: Float) : GestureAction
    /** Local view pan while pinching. */
    data class Pan(val dx: Float, val dy: Float) : GestureAction
}

enum class TouchMode { TRACKPAD, DIRECT }

/**
 * Touch gesture state machine, free of Android types so it is unit-testable.
 *
 * Trackpad: one-finger drag moves the cursor, tap = left click, long-press
 * then drag = left drag, two-finger tap = right click, two-finger drag =
 * wheel scroll, pinch = local zoom.
 * Direct: tap = left click where touched, one-finger drag = left drag,
 * long-press = right click, two-finger gestures as in trackpad mode.
 *
 * The caller schedules [longPressDeadline] and calls [onLongPress] when it
 * passes (the view posts a delayed runnable).
 */
class TouchGestures(
    var mode: TouchMode = TouchMode.TRACKPAD,
    private val slopPx: Float = 12f,
    private val pinchThresholdPx: Float = 24f,
    private val longPressMs: Long = 450,
    private val twoFingerTapMs: Long = 600,
) {
    private enum class Phase { IDLE, ONE, TWO, DONE }
    private enum class TwoMode { UNDECIDED, SCROLL, PINCH }

    private data class P(val x: Float, val y: Float)

    private val points = LinkedHashMap<Int, P>()
    private val origins = HashMap<Int, P>()
    private var phase = Phase.IDLE
    private var twoMode = TwoMode.UNDECIDED
    private var downAt = 0L
    private var moved = false
    private var dragging = false
    private var longPressed = false
    private var twoFingerTap = false
    private var pendingDx = 0f
    private var pendingDy = 0f
    private var baseDist = 0f
    private var baseMid = P(0f, 0f)

    /** When non-null, call [onLongPress] at this uptime (ms) if still pending. */
    var longPressDeadline: Long? = null
        private set

    val active: Boolean get() = points.isNotEmpty()

    fun down(id: Int, x: Float, y: Float, now: Long): List<GestureAction> {
        val out = ArrayList<GestureAction>()
        when (phase) {
            Phase.IDLE -> {
                points[id] = P(x, y); origins[id] = P(x, y)
                phase = Phase.ONE
                downAt = now; moved = false; dragging = false; longPressed = false
                pendingDx = 0f; pendingDy = 0f
                longPressDeadline = now + longPressMs
            }
            Phase.ONE -> {
                points[id] = P(x, y); origins[id] = P(x, y)
                longPressDeadline = null
                if (dragging) { dragging = false; out += GestureAction.Release(BUTTON_LEFT) }
                phase = Phase.TWO
                twoMode = TwoMode.UNDECIDED
                // A second finger landing soon after the first, before any
                // cursor motion, is a two-finger tap candidate.
                twoFingerTap = !moved && !longPressed
                val (a, b) = twoPoints()
                baseDist = dist(a, b); baseMid = mid(a, b)
            }
            Phase.TWO, Phase.DONE -> { /* third finger: ignored */ }
        }
        return out
    }

    fun move(id: Int, x: Float, y: Float): List<GestureAction> {
        val old = points[id] ?: return emptyList()
        val point = P(x, y)
        return when (phase) {
            Phase.ONE -> {
                points[id] = point
                oneFingerMove(id, old, point)
            }
            Phase.TWO -> {
                val (a0, b0) = twoPoints()
                points[id] = point
                val (a, b) = twoPoints()
                twoFingerMove(a0, b0, a, b)
            }
            else -> { points[id] = point; emptyList() }
        }
    }

    fun up(id: Int, now: Long): List<GestureAction> {
        if (!points.containsKey(id)) return emptyList()
        val point = points.remove(id)!!
        origins.remove(id)
        val out = ArrayList<GestureAction>()
        when (phase) {
            Phase.ONE -> {
                longPressDeadline = null
                if (mode == TouchMode.TRACKPAD) {
                    if (dragging) out += GestureAction.Release(BUTTON_LEFT)
                    else if (!moved && !longPressed) out += GestureAction.Click(BUTTON_LEFT)
                } else {
                    if (dragging) { out += GestureAction.PointAt(point.x, point.y); out += GestureAction.Release(BUTTON_LEFT) }
                    else if (!moved && !longPressed) { out += GestureAction.PointAt(point.x, point.y); out += GestureAction.Click(BUTTON_LEFT) }
                }
                reset()
            }
            Phase.TWO -> {
                if (twoFingerTap && twoMode == TwoMode.UNDECIDED && now - downAt <= twoFingerTapMs) {
                    if (mode == TouchMode.DIRECT) {
                        val o = origins.values.firstOrNull() ?: point
                        out += GestureAction.PointAt(o.x, o.y)
                    }
                    out += GestureAction.Click(BUTTON_RIGHT)
                }
                // Remaining finger does nothing until every finger lifts.
                phase = if (points.isEmpty()) Phase.IDLE else Phase.DONE
                if (points.isEmpty()) reset()
            }
            Phase.DONE -> if (points.isEmpty()) reset()
            Phase.IDLE -> {}
        }
        return out
    }

    /** Touch cancelled by the system: end any drag, emit nothing else. */
    fun cancel(): List<GestureAction> {
        val out = ArrayList<GestureAction>()
        if (dragging) out += GestureAction.Release(BUTTON_LEFT)
        reset()
        return out
    }

    fun onLongPress(now: Long): List<GestureAction> {
        val deadline = longPressDeadline ?: return emptyList()
        if (phase != Phase.ONE || moved || now < deadline) return emptyList()
        longPressDeadline = null
        longPressed = true
        val o = points.values.first()
        return if (mode == TouchMode.TRACKPAD) {
            dragging = true
            listOf(GestureAction.Press(BUTTON_LEFT))
        } else {
            listOf(GestureAction.PointAt(o.x, o.y), GestureAction.Click(BUTTON_RIGHT))
        }
    }

    private fun oneFingerMove(id: Int, old: P, point: P): List<GestureAction> {
        val origin = origins[id] ?: return emptyList()
        if (!moved) {
            if (hypot(point.x - origin.x, point.y - origin.y) < slopPx) {
                pendingDx += point.x - old.x; pendingDy += point.y - old.y
                return emptyList()
            }
            moved = true
            longPressDeadline = null
            if (mode == TouchMode.DIRECT) {
                if (longPressed) return emptyList() // right click already sent
                dragging = true
                return listOf(GestureAction.PointAt(origin.x, origin.y), GestureAction.Press(BUTTON_LEFT), GestureAction.PointAt(point.x, point.y))
            }
            val dx = pendingDx + point.x - old.x
            val dy = pendingDy + point.y - old.y
            pendingDx = 0f; pendingDy = 0f
            return listOf(GestureAction.Move(dx, dy))
        }
        return if (mode == TouchMode.DIRECT) {
            if (dragging) listOf(GestureAction.PointAt(point.x, point.y)) else emptyList()
        } else listOf(GestureAction.Move(point.x - old.x, point.y - old.y))
    }

    private fun twoFingerMove(a0: P, b0: P, a: P, b: P): List<GestureAction> {
        val m0 = mid(a0, b0)
        val m = mid(a, b)
        if (twoMode == TwoMode.UNDECIDED) {
            twoMode = when {
                abs(dist(a, b) - baseDist) >= pinchThresholdPx -> TwoMode.PINCH
                hypot(m.x - baseMid.x, m.y - baseMid.y) >= slopPx -> TwoMode.SCROLL
                else -> return emptyList()
            }
            twoFingerTap = false
            // Report the whole movement since the second finger landed.
            return if (twoMode == TwoMode.SCROLL) listOf(GestureAction.Scroll(m.x - baseMid.x, m.y - baseMid.y))
            else listOf(GestureAction.Zoom(if (baseDist > 0f) dist(a, b) / baseDist else 1f, m.x, m.y))
        }
        return if (twoMode == TwoMode.SCROLL) listOf(GestureAction.Scroll(m.x - m0.x, m.y - m0.y))
        else {
            val d0 = dist(a0, b0)
            listOf(GestureAction.Zoom(if (d0 > 0f) dist(a, b) / d0 else 1f, m.x, m.y), GestureAction.Pan(m.x - m0.x, m.y - m0.y))
        }
    }

    private fun twoPoints(): Pair<P, P> {
        val it = points.values.iterator()
        return it.next() to it.next()
    }

    private fun reset() {
        points.clear(); origins.clear()
        phase = Phase.IDLE; twoMode = TwoMode.UNDECIDED
        moved = false; dragging = false; longPressed = false; twoFingerTap = false
        pendingDx = 0f; pendingDy = 0f
        longPressDeadline = null
    }

    private fun dist(a: P, b: P) = hypot(a.x - b.x, a.y - b.y)
    private fun mid(a: P, b: P) = P((a.x + b.x) / 2f, (a.y + b.y) / 2f)

    companion object {
        const val BUTTON_LEFT = 1
        const val BUTTON_RIGHT = 2
    }
}

/**
 * Local zoom/pan of the video (input.js Viewport): the frame is fitted into
 * the view, then scaled by [zoom] (1..4) and offset by [x],[y].
 */
class Viewport {
    var viewW = 1f; private set
    var viewH = 1f; private set
    var contentX = 0f; private set
    var contentY = 0f; private set
    var contentW = 1f; private set
    var contentH = 1f; private set
    var zoom = 1f; private set
    var x = 0f; private set
    var y = 0f; private set

    fun layout(viewWidth: Int, viewHeight: Int, frameWidth: Int, frameHeight: Int) {
        viewW = maxOf(1, viewWidth).toFloat(); viewH = maxOf(1, viewHeight).toFloat()
        if (frameWidth <= 0 || frameHeight <= 0) {
            contentX = 0f; contentY = 0f; contentW = viewW; contentH = viewH
        } else {
            val scale = minOf(viewW / frameWidth, viewH / frameHeight)
            contentW = frameWidth * scale; contentH = frameHeight * scale
            contentX = (viewW - contentW) / 2f; contentY = (viewH - contentH) / 2f
        }
        clamp()
    }

    fun zoomBy(factor: Float, focusX: Float, focusY: Float) {
        val old = zoom
        zoom = (zoom * factor).coerceIn(1f, 4f)
        x = focusX - (focusX - x) * zoom / old
        y = focusY - (focusY - y) * zoom / old
        if (zoom == 1f) { x = 0f; y = 0f }
        clamp()
    }

    fun pan(dx: Float, dy: Float) { x += dx; y += dy; clamp() }

    /** Keeps a normalized remote point visible (cursor follow while zoomed). */
    fun keepVisible(nx: Float, ny: Float, marginPx: Float = 48f) {
        if (zoom <= 1f) return
        val (sx, sy) = toScreen(nx, ny)
        if (sx < marginPx) x += marginPx - sx else if (sx > viewW - marginPx) x -= sx - (viewW - marginPx)
        if (sy < marginPx) y += marginPx - sy else if (sy > viewH - marginPx) y -= sy - (viewH - marginPx)
        clamp()
    }

    /** Screen point -> normalized frame coordinates (may be outside 0..1). */
    fun toNormalized(sx: Float, sy: Float): Pair<Float, Float> =
        ((sx - x) / zoom - contentX) / contentW to ((sy - y) / zoom - contentY) / contentH

    fun toScreen(nx: Float, ny: Float): Pair<Float, Float> =
        x + (contentX + nx * contentW) * zoom to y + (contentY + ny * contentH) * zoom

    /** Screen pixels -> normalized delta (trackpad motion at the current zoom). */
    fun deltaToNormalized(dx: Float, dy: Float): Pair<Float, Float> = dx / (contentW * zoom) to dy / (contentH * zoom)

    private fun axis(value: Float, start: Float, size: Float, viewport: Float): Float {
        val scaled = size * zoom
        return if (scaled < viewport) (viewport - scaled) / 2f - start * zoom
        else minOf(maxOf(value, viewport - (start + size) * zoom), -start * zoom)
    }

    private fun clamp() {
        x = axis(x, contentX, contentW, viewW)
        y = axis(y, contentY, contentH, viewH)
    }
}
