package de.bubblebuffer.bubbleplay.ui

import android.content.Context
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.Path
import android.view.View

/**
 * Draws the remote pointer. Desktop Duplication frames do not contain the
 * cursor, so the app overlays it from host cursor-state packets and local
 * trackpad prediction.
 */
class CursorView(context: Context) : View(context) {
    private val density = resources.displayMetrics.density
    private val arrow = Path().apply {
        moveTo(0f, 0f); lineTo(0f, 17f); lineTo(4.2f, 13f); lineTo(7f, 19.5f)
        lineTo(9.6f, 18.4f); lineTo(6.9f, 12.2f); lineTo(12.5f, 12.2f); close()
    }
    private val fill = Paint(Paint.ANTI_ALIAS_FLAG).apply { color = Color.WHITE; style = Paint.Style.FILL }
    private val stroke = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        color = Color.BLACK; style = Paint.Style.STROKE; strokeWidth = 1.2f; strokeJoin = Paint.Join.ROUND
    }
    private var x = 0f
    private var y = 0f
    private var shown = false

    fun update(screenX: Float, screenY: Float, visible: Boolean) {
        if (x == screenX && y == screenY && shown == visible) return
        x = screenX; y = screenY; shown = visible
        invalidate()
    }

    override fun onDraw(canvas: Canvas) {
        if (!shown) return
        canvas.save()
        canvas.translate(x, y)
        canvas.scale(density * 1.1f, density * 1.1f)
        canvas.drawPath(arrow, fill)
        canvas.drawPath(arrow, stroke)
        canvas.restore()
    }
}
