package de.bubblebuffer.bubbleplay.ui

import android.content.Context
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.Path
import android.view.View
import de.bubblebuffer.bubbleplay.protocol.CursorKind

/**
 * Draws the remote pointer. Desktop Duplication frames do not contain the
 * cursor, so the app overlays it from host cursor-state packets and local
 * trackpad prediction.
 */
class CursorView(context: Context) : View(context) {
    private val density = resources.displayMetrics.density
    private val shape = Path()
    private val lines = Path()
    private val fill = Paint(Paint.ANTI_ALIAS_FLAG).apply { color = Color.WHITE; style = Paint.Style.FILL }
    private val stroke = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        color = Color.BLACK; style = Paint.Style.STROKE; strokeWidth = 1.2f; strokeJoin = Paint.Join.ROUND
    }
    private val lineOutline = Paint(stroke).apply { strokeWidth = 3.8f; strokeCap = Paint.Cap.ROUND }
    private val lineInk = Paint(lineOutline).apply { color = Color.WHITE; strokeWidth = 1.7f }
    private var x = 0f
    private var y = 0f
    private var shown = false
    private var kind = CursorKind.Arrow

    init { buildGlyph() }

    fun setKind(value: CursorKind) {
        if (kind == value) return
        kind = value
        buildGlyph()
        invalidate()
    }

    /** Touch/captured-pointer overlays need their own glyph: PointerIcon only draws a hardware mouse. */
    private fun buildGlyph() {
        shape.reset(); lines.reset()
        when (kind) {
            CursorKind.Arrow -> arrow()
            CursorKind.Text -> {
                line(0f, -9f, 0f, 9f)
                line(-4f, -9f, 4f, -9f); line(-4f, 9f, 4f, 9f)
            }
            CursorKind.Pointer -> shape.apply {
                // The fingertip remains the hotspot, just as the arrow's tip does.
                moveTo(-2f, 11f); lineTo(-2f, 2f); quadTo(-2f, 0f, 0f, 0f)
                quadTo(2f, 0f, 2f, 2f); lineTo(2f, 8f)
                quadTo(4f, 5f, 6f, 8f); quadTo(8f, 6f, 10f, 9f)
                quadTo(13f, 8f, 13f, 12f); lineTo(12f, 18f)
                quadTo(11f, 21f, 8f, 21f); lineTo(3f, 21f)
                quadTo(1f, 21f, -1f, 18f); lineTo(-5f, 13f)
                quadTo(-7f, 10f, -4f, 10f); lineTo(-2f, 11f); close()
            }
            CursorKind.Crosshair -> { line(-10f, 0f, 10f, 0f); line(0f, -10f, 0f, 10f) }
            CursorKind.Move -> { doubleArrow(0f); doubleArrow(90f) }
            CursorKind.ResizeNS -> doubleArrow(90f)
            CursorKind.ResizeEW -> doubleArrow(0f)
            CursorKind.ResizeNESW -> doubleArrow(-45f)
            CursorKind.ResizeNWSE -> doubleArrow(45f)
            CursorKind.Wait -> shape.apply {
                moveTo(-6f, -9f); lineTo(6f, -9f); lineTo(6f, -6f)
                lineTo(2f, 0f); lineTo(6f, 6f); lineTo(6f, 9f)
                lineTo(-6f, 9f); lineTo(-6f, 6f); lineTo(-2f, 0f)
                lineTo(-6f, -6f); close()
            }
            CursorKind.Progress -> {
                arrow()
                lines.addCircle(18f, 6f, 4f, Path.Direction.CW)
                line(18f, 3f, 18f, 6f); line(18f, 6f, 20f, 7f)
            }
            CursorKind.NotAllowed -> {
                lines.addCircle(0f, 0f, 8f, Path.Direction.CW)
                line(-5.6f, -5.6f, 5.6f, 5.6f)
            }
            CursorKind.Help -> {
                arrow()
                lines.apply {
                    moveTo(15f, 3f); cubicTo(15f, -1f, 22f, -1f, 22f, 3f)
                    cubicTo(22f, 6f, 18f, 5f, 18f, 8f)
                    addCircle(18f, 11f, 0.3f, Path.Direction.CW)
                }
            }
            CursorKind.UpArrow -> shape.apply {
                moveTo(0f, 0f); lineTo(-6f, 7f); lineTo(-2f, 7f)
                lineTo(-2f, 18f); lineTo(2f, 18f); lineTo(2f, 7f)
                lineTo(6f, 7f); close()
            }
            CursorKind.Hidden -> Unit
        }
    }

    private fun arrow() = shape.apply {
        moveTo(0f, 0f); lineTo(0f, 17f); lineTo(4.2f, 13f); lineTo(7f, 19.5f)
        lineTo(9.6f, 18.4f); lineTo(6.9f, 12.2f); lineTo(12.5f, 12.2f); close()
    }

    private fun line(x1: Float, y1: Float, x2: Float, y2: Float) {
        lines.moveTo(x1, y1); lines.lineTo(x2, y2)
    }

    private fun doubleArrow(degrees: Float) {
        val radians = Math.toRadians(degrees.toDouble())
        val c = kotlin.math.cos(radians).toFloat()
        val s = kotlin.math.sin(radians).toFloat()
        fun segment(x1: Float, y1: Float, x2: Float, y2: Float) =
            line(x1 * c - y1 * s, x1 * s + y1 * c, x2 * c - y2 * s, x2 * s + y2 * c)
        segment(-9f, 0f, 9f, 0f)
        segment(-9f, 0f, -5f, -4f); segment(-9f, 0f, -5f, 4f)
        segment(9f, 0f, 5f, -4f); segment(9f, 0f, 5f, 4f)
    }

    fun update(screenX: Float, screenY: Float, visible: Boolean) {
        if (x == screenX && y == screenY && shown == visible) return
        x = screenX; y = screenY; shown = visible
        invalidate()
    }

    override fun onDraw(canvas: Canvas) {
        if (!shown || kind == CursorKind.Hidden) return
        canvas.save()
        canvas.translate(x, y)
        canvas.scale(density * 1.1f, density * 1.1f)
        canvas.drawPath(shape, fill)
        canvas.drawPath(shape, stroke)
        canvas.drawPath(lines, lineOutline)
        canvas.drawPath(lines, lineInk)
        canvas.restore()
    }
}
