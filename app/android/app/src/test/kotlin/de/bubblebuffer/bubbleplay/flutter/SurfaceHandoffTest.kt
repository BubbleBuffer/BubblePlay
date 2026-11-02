package de.bubblebuffer.bubbleplay.flutter

import org.junit.Assert.assertEquals
import org.junit.Test

/** The decoder follows the texture surface: lost -> detach, available -> attach, size -> resize. */
class SurfaceHandoffTest {
    private val log = ArrayList<String>()
    private val sink = object : SurfaceHandoff.Sink<String> {
        override fun attach(surface: String) {
            log += "attach $surface"
        }

        override fun detachBlocking() {
            log += "detach"
        }
    }
    private val handoff = SurfaceHandoff(sink) { w, h -> log += "resize ${w}x$h" }

    @Test
    fun surfaceThatArrivesBeforeTheSessionIsAttachedOnBind() {
        handoff.available("s1")
        assertEquals(emptyList<String>(), log)
        handoff.bind()
        assertEquals(listOf("attach s1"), log)
    }

    @Test
    fun lostSurfaceDetachesAndNewSurfaceAttaches() {
        handoff.bind()
        handoff.available("s1")
        handoff.cleanup()
        handoff.available("s2")
        assertEquals(listOf("attach s1", "detach", "attach s2"), log)
    }

    @Test
    fun sameSurfaceTwiceAttachesOnce() {
        handoff.bind()
        handoff.available("s1")
        handoff.available("s1")
        assertEquals(listOf("attach s1"), log)
    }

    @Test
    fun cleanupWithoutAnAttachedSurfaceDetachesNothing() {
        handoff.bind()
        handoff.cleanup()
        handoff.unbind()
        assertEquals(emptyList<String>(), log)
    }

    @Test
    fun replacementWithoutCleanupDetachesTheOldFirst() {
        handoff.bind()
        handoff.available("s1")
        handoff.available("s2")
        assertEquals(listOf("attach s1", "detach", "attach s2"), log)
    }

    @Test
    fun unbindReleasesTheCodecAndNothingAttachesAfterwards() {
        handoff.bind()
        handoff.available("s1")
        handoff.unbind()
        handoff.available("s2")
        assertEquals(listOf("attach s1", "detach"), log)
    }

    @Test
    fun cleanupWhileUnboundIsRememberedSoTheNextBindDoesNotUseAStaleSurface() {
        handoff.available("s1")
        handoff.cleanup()
        handoff.bind()
        assertEquals(emptyList<String>(), log)
    }

    @Test
    fun resizeFollowsTheStreamSizeOnlyWhenItChanges() {
        handoff.resize(1920, 1080)
        handoff.resize(1920, 1080)
        handoff.resize(2560, 1440)
        handoff.resize(0, 0)
        assertEquals(listOf("resize 1920x1080", "resize 2560x1440"), log)
    }
}
