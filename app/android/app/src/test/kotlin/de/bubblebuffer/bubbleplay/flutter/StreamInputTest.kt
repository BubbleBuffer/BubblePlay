package de.bubblebuffer.bubbleplay.flutter

import de.bubblebuffer.bubbleplay.protocol.KeyMap
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/** Pointer and key mapping from Dart events to the Kotlin input path, against fakes. */
class StreamInputTest {
    private class FakeSink : InputSink {
        val calls = ArrayList<String>()
        override var buttonMask = 0
        override fun key(code: String, up: Boolean): Boolean {
            calls += "key $code ${if (up) "up" else "down"}"
            return true
        }

        override fun absoluteNormalized(nx: Float, ny: Float) {
            calls += "abs %.3f %.3f".format(java.util.Locale.US, nx, ny)
        }

        override fun buttonsAt(x: Int, y: Int, mask: Int): Boolean {
            calls += "buttons $x $y $mask"
            buttonMask = mask
            return true
        }

        override fun motion(dx: Int, dy: Int, wheelX: Int, wheelY: Int) {
            calls += "motion $dx $dy $wheelX $wheelY"
        }

        override fun flushMotion(): Boolean {
            calls += "flush"
            return true
        }

        override fun hasPendingInput() = false
        override fun inputReady() = Unit
        override fun inputLost() = Unit
        override fun releaseAll() = Unit
    }

    private class FakeTimers : InputTimers {
        var time = 1_000L
        val frames = ArrayList<Runnable>()
        var scheduled: Pair<Long, Runnable>? = null
        override fun now() = time
        override fun postFrame(task: Runnable) {
            frames += task
        }

        override fun postAt(uptimeMillis: Long, task: Runnable) {
            scheduled = uptimeMillis to task
        }

        override fun cancel(task: Runnable) {
            if (scheduled?.second === task) scheduled = null
        }
    }

    private val sink = FakeSink()
    private val timers = FakeTimers()
    private var live: FakeSink? = sink
    private val input = StreamInput({ live }, timers).also { it.videoSize(1920, 1080) }

    private fun pointer(kind: String, x: Double, y: Double, id: Int = 1, areaW: Double = 960.0, areaH: Double = 540.0) =
        input.pointer(
            mapOf("kind" to kind, "id" to id, "x" to x, "y" to y, "areaWidth" to areaW, "areaHeight" to areaH),
        )

    private fun scroll(dy: Double) = input.pointer(
        mapOf(
            "kind" to "scroll", "id" to 0, "x" to 10.0, "y" to 10.0,
            "areaWidth" to 960.0, "areaHeight" to 540.0, "dx" to 0.0, "dy" to dy,
        ),
    )

    @Test
    fun tapClicksWhereTheFingerIsInStreamCoordinates() {
        pointer("down", 480.0, 270.0)
        timers.time += 50
        pointer("up", 480.0, 270.0)
        // The middle of the area is the middle of the picture; left button down, then up.
        assertTrue(sink.calls.toString(), sink.calls.contains("abs 0.500 0.500"))
        val buttons = sink.calls.filter { it.startsWith("buttons") }
        assertEquals(listOf("buttons 32768 32768 1", "buttons 32768 32768 0"), buttons)
    }

    @Test
    fun areaWiderThanThePictureIsLetterboxed() {
        // 2000 x 540 area for a 16:9 picture: the picture is 960 wide, centred (offset 520).
        pointer("down", 520.0, 0.0, areaW = 2000.0, areaH = 540.0)
        timers.time += 50
        pointer("up", 520.0, 0.0, areaW = 2000.0, areaH = 540.0)
        assertTrue(sink.calls.toString(), sink.calls.contains("abs 0.000 0.000"))
    }

    @Test
    fun positionsOutsideThePictureAreClamped() {
        pointer("down", 2000.0, -50.0, areaW = 2000.0, areaH = 540.0)
        timers.time += 50
        pointer("up", 2000.0, -50.0, areaW = 2000.0, areaH = 540.0)
        assertTrue(sink.calls.toString(), sink.calls.contains("abs 1.000 0.000"))
    }

    @Test
    fun scrollBecomesWheelUnitsWithDownMeaningNegative() {
        // 40 logical px per notch, 120 units per notch: 40 px down is one notch down.
        scroll(40.0)
        assertEquals(listOf("motion 0 0 0 -120"), sink.calls)
    }

    @Test
    fun smallScrollsAccumulateInsteadOfGettingLost() {
        repeat(3) { scroll(5.0) }
        // 3 x 5 px = 15 px = 45 units: whole units out, nothing lost.
        val total = sink.calls.filter { it.startsWith("motion") }.sumOf { it.split(" ")[4].toInt() }
        assertEquals(-45, total)
    }

    @Test
    fun longPressIsScheduledAndLiftingItRightClicks() {
        pointer("down", 480.0, 270.0)
        val (at, task) = timers.scheduled!!
        assertTrue(at > timers.time)
        timers.time = at
        task.run()
        // Direct mode only arms the long press; the lift decides (no movement: right click).
        assertTrue(sink.calls.toString(), sink.calls.isEmpty())
        pointer("up", 480.0, 270.0)
        val buttons = sink.calls.filter { it.startsWith("buttons") }
        assertEquals(listOf("buttons 32768 32768 2", "buttons 32768 32768 0"), buttons)
    }

    @Test
    fun pointerWithNoSessionIsIgnored() {
        live = null
        pointer("down", 1.0, 1.0)
        assertTrue(sink.calls.isEmpty())
    }

    @Test
    fun keyUsageGoesToTheDomCodeAndDownIsNotUp() {
        input.key(0x70004, down = true)
        input.key(0x70004, down = false)
        input.key(0x700E1, down = true)
        input.key(0x70052, down = true)
        assertEquals(listOf("key KeyA down", "key KeyA up", "key ShiftLeft down", "key ArrowUp down"), sink.calls)
    }

    @Test
    fun unknownKeyUsageIsDropped() {
        input.key(0x70001, down = true)
        input.key(0x7FFFF, down = true)
        assertTrue(sink.calls.isEmpty())
    }

    @Test
    fun hidTableCoversTheMainBlock() {
        assertEquals("Digit1", HidKeys.code(0x1E))
        assertEquals("Digit0", HidKeys.code(0x27))
        assertEquals("F1", HidKeys.code(0x3A))
        assertEquals("F12", HidKeys.code(0x45))
        assertEquals("F13", HidKeys.code(0x68))
        assertEquals("Numpad1", HidKeys.code(0x59))
        assertEquals("Numpad0", HidKeys.code(0x62))
        assertEquals("NumpadEnter", HidKeys.code(0x58))
        assertEquals("MetaRight", HidKeys.code(0xE7))
        assertNull(HidKeys.code(0x00))
    }

    @Test
    fun everyMappedCodeIsKnownToTheKeyMap() {
        for (usage in 0..0xFF) {
            val code = HidKeys.code(usage) ?: continue
            assertTrue("$code (usage $usage)", KeyMap.descriptor(code) != null)
        }
    }

    @Test
    fun routerAnswersTheChannelMethods() {
        var texture: Int? = null
        val router = StreamChannelRouter(input) { texture }
        assertNull(router.handle("textureId", null))
        texture = 7
        assertEquals(7, router.handle("textureId", null))
        assertNull(router.handle("videoRect", mapOf("x" to 0.0)))
        router.handle("key", mapOf("usage" to 0x70004, "down" to true, "modifiers" to 0))
        assertEquals(listOf("key KeyA down"), sink.calls)
        assertTrue(router.handle("nope", null) === StreamChannelRouter.NOT_IMPLEMENTED)
    }
}
