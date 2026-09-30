package de.bubblebuffer.bubbleplay.input

import de.bubblebuffer.bubbleplay.input.GestureAction.Click
import de.bubblebuffer.bubbleplay.input.GestureAction.Move
import de.bubblebuffer.bubbleplay.input.GestureAction.Pan
import de.bubblebuffer.bubbleplay.input.GestureAction.PointAt
import de.bubblebuffer.bubbleplay.input.GestureAction.Press
import de.bubblebuffer.bubbleplay.input.GestureAction.Release
import de.bubblebuffer.bubbleplay.input.GestureAction.Scroll
import de.bubblebuffer.bubbleplay.input.GestureAction.Zoom
import de.bubblebuffer.bubbleplay.input.TouchGestures.Companion.BUTTON_LEFT
import de.bubblebuffer.bubbleplay.input.TouchGestures.Companion.BUTTON_RIGHT
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class TouchGesturesTest {
    private val g = TouchGestures()

    @Test
    fun trackpadTapIsLeftClick() {
        assertEquals(emptyList<GestureAction>(), g.down(0, 100f, 100f, 0))
        assertEquals(emptyList<GestureAction>(), g.move(0, 104f, 103f)) // within slop
        assertEquals(listOf(Click(BUTTON_LEFT)), g.up(0, 120))
        assertFalse(g.active)
        assertNull(g.longPressDeadline)
    }

    @Test
    fun trackpadDragMovesRelativelyIncludingSlop() {
        g.down(0, 100f, 100f, 0)
        g.move(0, 105f, 100f)
        assertEquals(listOf(Move(20f, 0f)), g.move(0, 120f, 100f))
        assertEquals(listOf(Move(10f, -5f)), g.move(0, 130f, 95f))
        assertEquals(emptyList<GestureAction>(), g.up(0, 500))
    }

    @Test
    fun trackpadLongPressDrags() {
        g.down(0, 50f, 50f, 1_000)
        assertEquals(1_450L, g.longPressDeadline)
        assertEquals(emptyList<GestureAction>(), g.onLongPress(1_200)) // too early
        assertEquals(listOf(Press(BUTTON_LEFT)), g.onLongPress(1_450))
        assertEquals(listOf(Move(30f, 0f)), g.move(0, 80f, 50f))
        assertEquals(listOf(Release(BUTTON_LEFT)), g.up(0, 2_000))
    }

    @Test
    fun longPressCancelledByMovement() {
        g.down(0, 50f, 50f, 0)
        g.move(0, 90f, 50f)
        assertNull(g.longPressDeadline)
        assertEquals(emptyList<GestureAction>(), g.onLongPress(1_000))
    }

    @Test
    fun twoFingerTapIsRightClick() {
        g.down(0, 100f, 100f, 0)
        g.down(1, 200f, 100f, 40)
        assertEquals(listOf(Click(BUTTON_RIGHT)), g.up(1, 150))
        assertEquals(emptyList<GestureAction>(), g.up(0, 160))
        assertFalse(g.active)
    }

    @Test
    fun slowTwoFingerTapIsNothing() {
        g.down(0, 100f, 100f, 0)
        g.down(1, 200f, 100f, 40)
        assertEquals(emptyList<GestureAction>(), g.up(1, 900))
    }

    @Test
    fun twoFingerDragScrolls() {
        g.down(0, 100f, 100f, 0)
        g.down(1, 200f, 100f, 10)
        assertEquals(emptyList<GestureAction>(), g.move(0, 100f, 105f))
        assertEquals(listOf(Scroll(0f, 20f)), g.move(1, 200f, 135f))
        assertEquals(listOf(Scroll(0f, 5f)), g.move(0, 100f, 115f))
        assertEquals(emptyList<GestureAction>(), g.up(0, 300)) // no right click after scrolling
        assertEquals(emptyList<GestureAction>(), g.move(1, 300f, 300f)) // leftover finger is inert
        g.up(1, 310)
        assertFalse(g.active)
    }

    @Test
    fun pinchZoomsAndPans() {
        g.down(0, 100f, 100f, 0)
        g.down(1, 200f, 100f, 10)
        val first = g.move(1, 250f, 100f)
        assertEquals(1, first.size)
        val zoom = first[0] as Zoom
        assertEquals(1.5f, zoom.scale, 1e-4f)
        assertEquals(175f, zoom.focusX, 1e-4f)
        val next = g.move(0, 110f, 100f)
        assertTrue(next[0] is Zoom)
        assertEquals(Pan(5f, 0f), next[1])
    }

    @Test
    fun secondFingerEndsATrackpadDrag() {
        g.down(0, 50f, 50f, 0)
        g.onLongPress(450)
        assertEquals(listOf(Release(BUTTON_LEFT)), g.down(1, 150f, 50f, 600))
    }

    @Test
    fun directTapClicksWhereTouched() {
        g.mode = TouchMode.DIRECT
        g.down(0, 10f, 20f, 0)
        assertEquals(listOf(PointAt(10f, 20f), Click(BUTTON_LEFT)), g.up(0, 100))
    }

    @Test
    fun directOneFingerDragPansAndSendsNothing() {
        g.mode = TouchMode.DIRECT
        g.down(0, 10f, 20f, 0)
        assertEquals(emptyList<GestureAction>(), g.move(0, 15f, 22f)) // inside the slop: still a tap candidate
        assertEquals(listOf(Pan(30f, 0f)), g.move(0, 40f, 20f)) // includes the slop distance
        assertEquals(listOf(Pan(10f, 5f)), g.move(0, 50f, 25f))
        assertNull(g.longPressDeadline)
        assertEquals(emptyList<GestureAction>(), g.onLongPress(450)) // panning cancels the long-press
        assertEquals(emptyList<GestureAction>(), g.up(0, 900)) // no click after a pan
        assertFalse(g.active)
    }

    @Test
    fun directSmallJitterStaysATap() {
        g.mode = TouchMode.DIRECT
        g.down(0, 10f, 20f, 0)
        g.move(0, 18f, 24f)
        assertEquals(listOf(PointAt(18f, 24f), Click(BUTTON_LEFT)), g.up(0, 150))
    }

    @Test
    fun directLongPressThenDragIsAMouseDrag() {
        g.mode = TouchMode.DIRECT
        g.down(0, 10f, 20f, 0)
        assertEquals(listOf(GestureAction.LongPressArmed), g.onLongPress(450)) // nothing sent yet
        assertEquals(listOf(PointAt(10f, 20f), Press(BUTTON_LEFT), PointAt(40f, 20f)), g.move(0, 40f, 20f))
        assertEquals(listOf(PointAt(50f, 25f)), g.move(0, 50f, 25f))
        assertEquals(listOf(PointAt(50f, 25f), Release(BUTTON_LEFT)), g.up(0, 900))
    }

    @Test
    fun directLongPressWithoutMovingIsRightClick() {
        g.mode = TouchMode.DIRECT
        g.down(0, 10f, 20f, 0)
        g.onLongPress(450)
        g.move(0, 12f, 21f) // within slop
        assertEquals(listOf(PointAt(12f, 21f), Click(BUTTON_RIGHT)), g.up(0, 900))
    }

    @Test
    fun directCancelDuringLongPressDragReleases() {
        g.mode = TouchMode.DIRECT
        g.down(0, 10f, 20f, 0)
        g.onLongPress(450)
        g.move(0, 60f, 20f)
        assertEquals(listOf(Release(BUTTON_LEFT)), g.cancel())
    }

    @Test
    fun directTwoFingerDragScrollsAndPinchZooms() {
        g.mode = TouchMode.DIRECT
        g.down(0, 100f, 100f, 0)
        g.down(1, 200f, 100f, 10)
        assertEquals(emptyList<GestureAction>(), g.move(0, 100f, 105f))
        assertEquals(listOf(Scroll(0f, 20f)), g.move(1, 200f, 135f))
        assertEquals(emptyList<GestureAction>(), g.up(0, 300))
        g.up(1, 310)
        g.down(0, 100f, 100f, 1_000)
        g.down(1, 200f, 100f, 1_010)
        val zoom = g.move(1, 250f, 100f).single() as Zoom
        assertEquals(1.5f, zoom.scale, 1e-4f)
        assertEquals(175f, zoom.focusX, 1e-4f) // direct keeps the pinch focus
    }

    @Test
    fun directTwoFingerDoubleTapResetsZoom() {
        g.mode = TouchMode.DIRECT
        g.down(0, 100f, 100f, 0)
        g.down(1, 200f, 100f, 20)
        assertEquals(emptyList<GestureAction>(), g.up(1, 100)) // a single two-finger tap does nothing in direct mode
        g.up(0, 110)
        g.down(0, 100f, 100f, 300)
        g.down(1, 200f, 100f, 310)
        assertEquals(listOf(GestureAction.ResetZoom), g.up(0, 380))
        g.up(1, 390)
    }

    @Test
    fun directSlowSecondTwoFingerTapIsNotADoubleTap() {
        g.mode = TouchMode.DIRECT
        g.down(0, 100f, 100f, 0); g.down(1, 200f, 100f, 20)
        g.up(1, 100); g.up(0, 110)
        g.down(0, 100f, 100f, 900); g.down(1, 200f, 100f, 910)
        assertEquals(emptyList<GestureAction>(), g.up(0, 980))
    }

    @Test
    fun trackpadTwoFingerDoubleTapIsTwoRightClicks() {
        g.down(0, 100f, 100f, 0); g.down(1, 200f, 100f, 20)
        assertEquals(listOf(Click(BUTTON_RIGHT)), g.up(1, 100)); g.up(0, 110)
        g.down(0, 100f, 100f, 300); g.down(1, 200f, 100f, 310)
        assertEquals(listOf(Click(BUTTON_RIGHT)), g.up(0, 380))
    }

    @Test
    fun resetZoomReturnsToFit() {
        val v = Viewport().apply { layout(1000, 500, 1920, 1080) }
        v.zoomBy(3f, 100f, 100f)
        v.pan(-50f, -50f)
        assertTrue(v.zoomed)
        v.resetZoom()
        assertFalse(v.zoomed)
        assertEquals(0.5f, v.toNormalized(500f, 250f).first, 1e-4f)
        assertEquals(0.5f, v.toNormalized(500f, 250f).second, 1e-4f)
    }

    @Test
    fun cancelReleasesADrag() {
        g.down(0, 50f, 50f, 0)
        g.onLongPress(450)
        assertEquals(listOf(Release(BUTTON_LEFT)), g.cancel())
        assertFalse(g.active)
        assertEquals(emptyList<GestureAction>(), g.cancel())
    }

    @Test
    fun viewportFitsZoomsAndMapsBothWays() {
        val v = Viewport()
        v.layout(2000, 1000, 1920, 1080) // letterboxed horizontally
        assertEquals(1000f * 1920f / 1080f, v.contentW, 0.01f)
        assertEquals(0f, v.contentY, 0.01f)
        assertEquals(0.5f to 0.5f, v.toNormalized(1000f, 500f))
        v.zoomBy(2f, 1000f, 500f)
        assertEquals(2f, v.zoom)
        val (nx, ny) = v.toNormalized(1000f, 500f)
        assertEquals(0.5f, nx, 1e-4f) // focus point stays under the fingers
        assertEquals(0.5f, ny, 1e-4f)
        val (sx, sy) = v.toScreen(0.25f, 0.75f)
        val back = v.toNormalized(sx, sy)
        assertEquals(0.25f, back.first, 1e-4f)
        assertEquals(0.75f, back.second, 1e-4f)
        v.pan(-1e6f, -1e6f) // clamped to the content edge
        assertEquals(1f, v.toNormalized(v.viewW - 1e-3f, v.viewH - 1e-3f).second, 1e-3f)
        v.zoomBy(100f, 0f, 0f)
        assertEquals(4f, v.zoom)
        v.zoomBy(0.01f, 0f, 0f)
        assertEquals(1f, v.zoom)
        assertEquals(0f, v.x)
    }

    /**
     * The stream is laid out in the area above the docked bar (and above the
     * soft keyboard): the frame fits inside that area and touches map to the
     * frame within it, never under the bar.
     */
    private fun assertFitsAbove(windowW: Int, windowH: Int, ime: Int, bar: Int, landscapeFill: Boolean) {
        val areaH = windowH - ime - bar
        val v = Viewport().apply { layout(windowW, areaH, 1920, 1080) }
        assertEquals(areaH.toFloat(), v.viewH, 0.01f)
        assertTrue("frame bottom ${v.contentY + v.contentH} inside $areaH", v.contentY + v.contentH <= areaH + 0.01f)
        assertTrue(v.contentY >= -0.01f && v.contentX >= -0.01f)
        if (landscapeFill) assertEquals(areaH.toFloat(), v.contentH, 0.01f) else assertEquals(windowW.toFloat(), v.contentW, 0.01f)
        // Frame corners map to 0..1 inside the area.
        val (l, t) = v.toNormalized(v.contentX, v.contentY)
        val (r, b) = v.toNormalized(v.contentX + v.contentW, v.contentY + v.contentH)
        assertEquals(0f, l, 1e-4f); assertEquals(0f, t, 1e-4f)
        assertEquals(1f, r, 1e-4f); assertEquals(1f, b, 1e-4f)
        // The centre of the area is the centre of the frame.
        val (cx, cy) = v.toNormalized(windowW / 2f, areaH / 2f)
        assertEquals(0.5f, cx, 1e-4f); assertEquals(0.5f, cy, 1e-4f)
        // Zoomed in and panned to the bottom: the frame's last row reaches at most
        // the bar's top edge (exactly, once the zoomed frame is taller than the area).
        v.zoomBy(3f, windowW / 2f, areaH / 2f)
        v.pan(0f, -1e6f)
        val bottom = v.toScreen(0.5f, 1f).second
        assertTrue("zoomed frame bottom $bottom inside $areaH", bottom <= areaH + 1e-2f)
        if (v.contentH * v.zoom >= areaH) assertEquals(areaH.toFloat(), bottom, 1e-2f)
    }

    @Test
    fun viewportUsesTheAreaAboveTheBarInPortrait() {
        assertFitsAbove(1080, 2400, ime = 0, bar = 147, landscapeFill = false)
        assertFitsAbove(1080, 2400, ime = 1000, bar = 147, landscapeFill = false)
    }

    @Test
    fun viewportUsesTheAreaAboveTheBarInLandscape() {
        assertFitsAbove(2400, 1080, ime = 0, bar = 147, landscapeFill = true)
        assertFitsAbove(2400, 1080, ime = 600, bar = 147, landscapeFill = true)
    }

    @Test
    fun viewportHandlesDegenerateSizes() {
        val v = Viewport()
        v.layout(0, 0, 0, 0)
        v.zoomBy(3f, 0f, 0f)
        v.pan(10f, 10f)
        v.centerOn(1f, 1f)
        v.zoomAround(2f, 0.5f, 0.5f)
        assertTrue(v.x.isFinite() && v.y.isFinite())
    }

    private fun fitted() = Viewport().apply { layout(1000, 500, 1920, 1080) } // content 888.9 x 500, centred horizontally

    /** The visible area never goes outside the frame: every screen edge maps into 0..1 (or the letterbox when not zoomed enough). */
    private fun assertInsideFrame(v: Viewport) {
        val (l, t) = v.toNormalized(0f, 0f)
        val (r, b) = v.toNormalized(v.viewW, v.viewH)
        val eps = 1e-4f
        if (v.contentW * v.zoom >= v.viewW) { assertTrue("left $l", l >= -eps); assertTrue("right $r", r <= 1f + eps) }
        if (v.contentH * v.zoom >= v.viewH) { assertTrue("top $t", t >= -eps); assertTrue("bottom $b", b <= 1f + eps) }
    }

    @Test
    fun zoomAroundKeepsTheCursorWhereItIs() {
        val v = fitted()
        val before = v.toScreen(0.4f, 0.45f)
        v.zoomAround(2f, 0.4f, 0.45f)
        assertEquals(2f, v.zoom)
        val after = v.toScreen(0.4f, 0.45f)
        assertEquals(before.first, after.first, 1e-3f)
        assertEquals(before.second, after.second, 1e-3f)
        assertInsideFrame(v)
    }

    @Test
    fun zoomAroundAnEdgeCursorIsClampedToTheFrame() {
        val v = fitted()
        v.zoomAround(3f, 0f, 0f)
        assertEquals(3f, v.zoom)
        assertInsideFrame(v)
        // Top-left corner of the frame sits at the top-left of the view.
        val (sx, sy) = v.toScreen(0f, 0f)
        assertEquals(0f, sx, 1e-3f)
        assertEquals(0f, sy, 1e-3f)
    }

    @Test
    fun centerOnPutsTheCursorInTheMiddleWhenThereIsRoom() {
        val v = fitted()
        v.zoomAround(3f, 0.5f, 0.5f)
        v.centerOn(0.3f, 0.6f)
        val (sx, sy) = v.toScreen(0.3f, 0.6f)
        assertEquals(500f, sx, 1e-3f)
        assertEquals(250f, sy, 1e-3f)
        assertInsideFrame(v)
    }

    @Test
    fun centerOnNearAnEdgeLetsTheCursorGoOffCentre() {
        val v = fitted()
        v.zoomAround(2f, 0.5f, 0.5f)
        v.centerOn(0.98f, 0.02f)
        assertInsideFrame(v)
        val (sx, sy) = v.toScreen(0.98f, 0.02f)
        assertTrue("cursor right of centre: $sx", sx > 500f)
        assertTrue("cursor above centre: $sy", sy < 250f)
        // The frame's right and top edges are flush with the view.
        assertEquals(v.viewW, v.toScreen(1f, 0f).first, 1e-2f)
        assertEquals(0f, v.toScreen(1f, 0f).second, 1e-2f)
    }

    @Test
    fun nudgeThenCursorMoveReCentres() {
        val v = fitted()
        v.zoomAround(2f, 0.5f, 0.5f)
        v.centerOn(0.5f, 0.5f)
        v.pan(-120f, 40f) // two-finger nudge
        assertEquals(380f, v.toScreen(0.5f, 0.5f).first, 1e-3f)
        v.centerOn(0.51f, 0.5f) // next cursor move
        assertEquals(500f, v.toScreen(0.51f, 0.5f).first, 1e-3f)
        assertEquals(250f, v.toScreen(0.51f, 0.5f).second, 1e-3f)
    }

    @Test
    fun centerOnDoesNothingAtZoomOne() {
        val v = fitted()
        val x = v.x; val y = v.y
        v.centerOn(0.9f, 0.1f)
        assertEquals(x, v.x); assertEquals(y, v.y)
    }

    @Test
    fun wheelAccumulatesFractions() {
        val w = WheelAccumulator()
        // Whole WHEEL_DELTA units go out at once; fractions carry over.
        assertEquals(0 to 0, w.add(0.4f, 0.6f))
        assertEquals(1 to 1, w.add(0.7f, 0.6f))
        assertEquals(-129 to 60, w.add(-130f, 60f)) // carried 0.1 / 0.2 truncates toward zero
        assertEquals(-1 to 0, w.add(-0.2f, 0f)) // carried -0.9 plus -0.2
    }
}
