package de.bubblebuffer.bubbleplay.input

import de.bubblebuffer.bubbleplay.ui.TextEdit
import de.bubblebuffer.bubbleplay.ui.RemoteKeyRepeats
import de.bubblebuffer.bubbleplay.ui.ImeTextSender
import de.bubblebuffer.bubbleplay.ui.LatchedKeySender
import de.bubblebuffer.bubbleplay.protocol.CursorState
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.json.JSONObject
import java.io.File

class RemoteInputTest {
    @Test fun largeNativeWheelSampleIsNotClampedBeforePacketSplitting() {
        val transport = FakeTransport()
        val remote = RemoteInput(transport)
        val units = WheelAccumulator().add(60000f, -120000f)
        remote.motion(0, 0, units.first, units.second)
        assertTrue(remote.flushMotion())
        assertEquals(60000, transport.sent.sumOf { signed16(it, 7) })
        assertEquals(-120000, transport.sent.sumOf { signed16(it, 9) })
        assertFalse(remote.hasPendingMotion())
    }
    @Test fun scrollInterleavingUsesTheSameSharedSequencesAsBrowserReplay() {
        val fixture = JSONObject(File(
            File(requireNotNull(System.getProperty("bubbleplay.fixtures"))).parentFile,
            "worker/test/fixtures/scroll-client-replay.json",
        ).readText())
        val transport = FakeTransport()
        val remote = RemoteInput(transport)
        val steps = fixture.getJSONArray("steps")
        for (index in 0 until steps.length()) {
            val step = steps.getJSONObject(index)
            val args = step.optJSONArray("args")
            when (step.getString("op")) {
                "absolute" -> remote.absolute(args!!.getInt(0), args.getInt(1))
                "motion" -> remote.motion(args!!.getInt(0), args.getInt(1), args.getInt(2), args.getInt(3))
                "buttonsAt" -> assertTrue(remote.buttonsAt(args!!.getInt(0), args.getInt(1), args.getInt(2)))
                "flush" -> assertTrue(remote.flushMotion())
                else -> error("unknown replay step")
            }
        }
        val packets = fixture.getJSONArray("packets")
        assertEquals(
            (0 until packets.length()).map { packets.getJSONObject(it).getString("hex") },
            transport.sent.map { bytes -> bytes.joinToString("") { "%02x".format(it.toInt() and 0xff) } },
        )
        assertEquals(6, remote.lastSentSequence)
    }
    private class FakeTransport : RemoteInput.Transport {
        var open = true
        var queued = 0L
        var accept = true
        var countQueuedBytes = false
        val sent = ArrayList<ByteArray>()
        override fun inputOpen() = open
        override fun send(bytes: ByteArray): Boolean {
            if (!accept) return false
            sent += bytes
            if (countQueuedBytes) queued += bytes.size
            return true
        }
        override fun queuedBytes() = queued
        override fun scancodeHost() = true
    }

    private val t = FakeTransport()
    private val input = RemoteInput(t)

    @Test
    fun textSplitsAt512BytesOnCodePointBoundaries() {
        val text = "é".repeat(300) + "😀".repeat(10) // 600 + 40 bytes
        assertTrue(input.text(text))
        val texts = t.sent.filter { it[0].toInt() == 11 }
        assertTrue(texts.size >= 2)
        for (p in texts) assertTrue(p.size - 1 <= 512)
        val joined = texts.joinToString("") { String(it, 1, it.size - 1, Charsets.UTF_8) }
        assertEquals(text, joined)
    }

    @Test
    fun keysUseScanCodes() {
        input.key("KeyA", false)
        assertArrayEquals(byteArrayOf(13, 0, 30, 0, 0x41, 0), t.sent.last())
    }

    @Test
    fun nothingIsSentWhileInputIsClosed() {
        t.open = false
        assertFalse(input.key("KeyA", false))
        input.motion(3, 4)
        assertFalse(input.flushMotion())
        assertTrue(t.sent.none { it[0].toInt() == 13 || it[0].toInt() == 7 })
    }

    @Test
    fun motionCoalescesUntilFlushed() {
        input.motion(3, 4)
        input.motion(2, -1, 0, 120)
        assertTrue(input.hasPendingMotion())
        assertTrue(input.flushMotion())
        val p = t.sent.last()
        assertEquals(7, p[0].toInt())
        assertEquals(11, p.size)
        assertEquals(5, p[3].toInt()); assertEquals(3, p[5].toInt()); assertEquals(120, p[9].toInt() and 0xff)
        assertFalse(input.hasPendingMotion())
    }

    @Test
    fun motionIsHeldBackWhenTheSocketIsBacklogged() {
        input.absolute(100, 200)
        t.queued = RemoteInput.MOTION_BACKLOG_BYTES + 1
        val before = t.sent.size
        assertFalse(input.flushMotion())
        assertEquals(before, t.sent.size)
        t.queued = 0
        assertTrue(input.flushMotion())
        assertEquals(8, t.sent.last()[0].toInt())
    }

    @Test fun repeatedDeleteTapsAreAllSentAndReleased() {
        for (code in listOf("Backspace", "Delete")) {
            t.sent.clear()
            // One initial down followed by three Android repeats / ACTION_MULTIPLE.
            assertTrue(RemoteKeyRepeats.tap(1) { input.key(code, it) })
            assertTrue(RemoteKeyRepeats.tap(3) { input.key(code, it) })
            assertEquals(8, t.sent.size)
            // Delete also sets the extended scan-code flag; bit 0 carries key-up.
            assertEquals(listOf(0, 1, 0, 1, 0, 1, 0, 1), t.sent.map { it[1].toInt() and 1 })
        }
    }

    @Test fun repeatedDeleteTapsStopWhenInputIsGated() {
        var calls = 0
        assertFalse(RemoteKeyRepeats.tap(3) {
            calls++
            if (calls == 3) t.open = false
            input.key("Backspace", it)
        })
        assertEquals(3, calls)
        assertEquals(2, t.sent.size)
    }

    @Test fun imeTextAndLatchedEnterSendOneOrderedPressAndReleasePair() {
        val latches = linkedSetOf("ControlLeft")
        val keys = LatchedKeySender(latches, hashMapOf(), input::key, {})
        var resetRequested = false
        val sender = ImeTextSender(input::text,
            { code -> input.key(code, false) && input.key(code, true) },
            { code -> keys.down(code) && keys.up(code) },
            { latches.isNotEmpty() }, { resetRequested = true })
        assertTrue(sender.send(0, "hello\n"))
        assertEquals(listOf(11, 13, 13, 13, 13), t.sent.map { it[0].toInt() })
        assertEquals("hello", String(t.sent.first(), 1, t.sent.first().size - 1, Charsets.UTF_8))
        val packets = t.sent.drop(1)
        // Scan-code packets identify the left Ctrl key as VK_LCONTROL (0xA2).
        assertEquals(listOf(0xA2, 0x0D, 0x0D, 0xA2), packets.map { it[4].toInt() and 255 })
        assertEquals(listOf(0, 0, 1, 1), packets.map { it[1].toInt() and 1 })
        assertTrue(resetRequested)
        assertTrue(latches.isEmpty())
    }

    private fun signed16(packet: ByteArray, offset: Int): Int =
        ((packet[offset].toInt() and 255) or ((packet[offset + 1].toInt() and 255) shl 8)).toShort().toInt()

    @Test fun motionBudgetIncludesThePacketAndKeepsLatestAbsoluteTarget() {
        assertEquals(512L, RemoteInput.MOTION_BACKLOG_BYTES)
        input.absolute(100, 200)
        t.queued = 506 // A seven-byte position would exceed 512.
        assertFalse(input.flushMotion())
        input.absolute(300, 400)
        assertFalse(input.flushMotion())
        t.queued = 505
        assertTrue(input.flushMotion())
        assertEquals(300, signed16(t.sent.single(), 3))
        assertEquals(400, signed16(t.sent.single(), 5))
    }

    @Test fun congestedMotionRetainsButtonsAndLaterMotionInDragOrder() {
        t.queued = 512
        input.motion(10, 20)
        assertFalse(input.flushMotion())
        assertTrue(input.buttons(1))
        input.motion(30, 40)
        assertTrue(input.buttons(0))
        input.motion(50, 60)
        assertTrue(t.sent.isEmpty())
        t.queued = 0
        assertTrue(input.flushMotion())
        assertEquals(listOf(7, 4, 7, 4, 7), t.sent.map { it[0].toInt() })
        assertEquals(listOf(10, 30, 50), t.sent.filter { it[0].toInt() == 7 }.map { signed16(it, 3) })
        assertEquals(listOf(1, 0), t.sent.filter { it[0].toInt() == 4 }.map { it[1].toInt() })
        assertFalse(input.hasPendingInput())
    }

    @Test fun laterReliableKeysAndTextStayBehindDeferredMouseBarrier() {
        t.queued = 512
        input.motion(10, 0)
        input.buttons(1)
        assertTrue(input.key("KeyA", false))
        assertTrue(input.text("hello"))
        assertTrue(t.sent.isEmpty())
        t.queued = 0
        input.flushMotion()
        assertEquals(listOf(7, 4, 13, 11), t.sent.map { it[0].toInt() })
        // Reliable controls alone still bypass the motion budget.
        t.queued = 10_000
        assertTrue(input.key("KeyA", true))
        assertEquals(13, t.sent.last()[0].toInt())
    }

    @Test fun relativeAndWheelTotalsSurviveMultipleI16PacketsAndCongestion() {
        t.queued = 501
        t.countQueuedBytes = true
        input.motion(40_000, -40_000, 50_000, -50_000)
        assertTrue(input.flushMotion()) // One 11-byte packet fits, remainder stays.
        assertTrue(input.hasPendingMotion())
        input.motion(12, -13, 14, -15)
        t.queued = 0
        input.flushMotion()
        assertEquals(40_012, t.sent.sumOf { signed16(it, 3) })
        assertEquals(-40_013, t.sent.sumOf { signed16(it, 5) })
        assertEquals(50_014, t.sent.sumOf { signed16(it, 7) })
        assertEquals(-50_015, t.sent.sumOf { signed16(it, 9) })
        assertFalse(input.hasPendingMotion())
    }

    @Test fun wheelFollowingPendingAbsoluteIsPreserved() {
        input.absolute(100, 200)
        input.motion(0, 0, 120, -240)
        input.flushMotion()
        assertEquals(listOf(8, 7), t.sent.map { it[0].toInt() })
        assertEquals(120, signed16(t.sent.last(), 7))
        assertEquals(-240, signed16(t.sent.last(), 9))
    }

    private fun cursorFeedback(ack: Int = 0, hasAck: Boolean = false) =
        CursorState(ack, 100, 200, hasAck, inBounds = true, visible = true)

    @Test fun wheelOnlyActualSendDoesNotCreatePendingCursorAuthority() {
        val cursor = CursorReconciler()
        val remote = RemoteInput(t, cursor::noteSent)
        remote.motion(0, 0, 120, -240)
        assertTrue(remote.hasPendingMotion())
        assertFalse(remote.hasPendingCursorMotion())
        t.queued = 512
        assertFalse(remote.flushMotion())
        assertTrue(cursor.apply(cursorFeedback(), localMotionPending = remote.hasPendingCursorMotion()))
        t.queued = 0
        assertTrue(remote.flushMotion())
        assertEquals(0, remote.lastSentSequence)
        assertNull(cursor.lastSent)
        assertFalse(cursor.pending)
        assertTrue(cursor.apply(cursorFeedback()))
        remote.motion(0, 0, 0, 120)
        remote.flushMotion()
        assertEquals(1, remote.lastSentSequence)
        assertEquals(1, signed16(t.sent.last(), 1))
        assertFalse(cursor.pending)
    }

    @Test fun wheelSequenceDoesNotOverwriteOrClearPendingRealCursorMovement() {
        val cursor = CursorReconciler()
        val remote = RemoteInput(t, cursor::noteSent)
        remote.motion(5, -6)
        assertTrue(remote.hasPendingCursorMotion())
        remote.flushMotion()
        assertEquals(0, cursor.lastSent)
        remote.motion(0, 0, 0, 120)
        remote.flushMotion()
        assertEquals(1, remote.lastSentSequence)
        assertEquals(0, cursor.lastSent)
        assertTrue(cursor.pending)
        assertFalse(cursor.apply(cursorFeedback()))
        // ACK for actual movement remains sufficient despite the later wheel sequence.
        assertTrue(cursor.apply(cursorFeedback(0, hasAck = true)))
        assertFalse(cursor.pending)
        assertTrue(cursor.apply(cursorFeedback()))
    }

    @Test fun wheelRemainderChunkDoesNotReplaceTheCursorMovementSequence() {
        val cursor = CursorReconciler()
        val remote = RemoteInput(t, cursor::noteSent)
        remote.motion(5, -6, 40_000, 0)
        remote.flushMotion()
        assertEquals(listOf(7, 7), t.sent.map { it[0].toInt() })
        assertEquals(0, signed16(t.sent.last(), 3))
        assertEquals(0, signed16(t.sent.last(), 5))
        assertEquals(1, remote.lastSentSequence)
        assertEquals(0, cursor.lastSent)
        assertTrue(cursor.pending)
        assertTrue(cursor.apply(cursorFeedback(0, hasAck = true)))
        assertTrue(cursor.apply(cursorFeedback()))
    }

    @Test fun cancellingRelativeSamplesWithWheelDoNotCreateCursorPendingState() {
        val cursor = CursorReconciler()
        val remote = RemoteInput(t, cursor::noteSent)
        remote.motion(5, -6, 0, 120)
        remote.motion(-5, 6)
        assertFalse(remote.hasPendingCursorMotion())
        remote.flushMotion()
        assertEquals(0, remote.lastSentSequence)
        assertFalse(cursor.pending)
        assertTrue(cursor.apply(cursorFeedback()))
    }

    @Test fun congestedAbsoluteThenRelativeRetainsTargetBeforeSummedTailAndButton() {
        t.queued = 512
        input.absolute(100, 200)
        assertFalse(input.flushMotion())
        input.motion(10, 20, 120, 0)
        input.motion(3, -4, 0, -240)
        assertFalse(input.flushMotion())
        input.buttons(1)
        assertTrue(t.sent.isEmpty())
        t.queued = 0
        input.flushMotion()
        assertEquals(listOf(8, 7, 4), t.sent.map { it[0].toInt() })
        assertEquals(100, signed16(t.sent[0], 3))
        assertEquals(200, signed16(t.sent[0], 5))
        assertEquals(13, signed16(t.sent[1], 3))
        assertEquals(16, signed16(t.sent[1], 5))
        assertEquals(120, signed16(t.sent[1], 7))
        assertEquals(-240, signed16(t.sent[1], 9))
    }

    @Test fun latestAbsoluteSupersedesEarlierRelativePositionButKeepsWheelDistance() {
        t.queued = 512
        input.motion(1, 2, 120, 0)
        input.absolute(100, 200)
        input.motion(10, 20, 0, -240)
        assertFalse(input.flushMotion())
        input.absolute(300, 400)
        input.motion(3, -4, 0, 0)
        assertFalse(input.flushMotion())
        t.queued = 0
        input.flushMotion()
        assertEquals(listOf(8, 7), t.sent.map { it[0].toInt() })
        assertEquals(300, signed16(t.sent[0], 3))
        assertEquals(400, signed16(t.sent[0], 5))
        assertEquals(3, signed16(t.sent[1], 3))
        assertEquals(-4, signed16(t.sent[1], 5))
        assertEquals(120, signed16(t.sent[1], 7))
        assertEquals(-240, signed16(t.sent[1], 9))
    }

    @Test fun refusedReliableAdmissionAcceptsLocallySchedulesRetryAndPreservesOrder() {
        var scheduled = 0
        val remote = RemoteInput(t, onDeferredInput = { scheduled++ })
        t.accept = false
        assertTrue(remote.buttons(1))
        assertTrue(remote.key("KeyA", false))
        assertTrue(remote.text("hello"))
        assertTrue(remote.hasPendingInput())
        assertEquals(3, scheduled)
        assertTrue(t.sent.isEmpty())
        t.accept = true
        remote.flushMotion()
        assertEquals(listOf(4, 13, 11), t.sent.map { it[0].toInt() })
        assertFalse(remote.hasPendingInput())
    }

    @Test fun refusedMotionSendRetainsTheButtonForRetry() {
        t.accept = false
        input.motion(10, 20)
        assertTrue(input.buttons(1))
        assertTrue(input.hasPendingInput())
        assertNull(input.lastSentSequence)
        t.accept = true
        input.flushMotion()
        assertEquals(listOf(7, 4), t.sent.map { it[0].toInt() })
    }

    @Test fun lostSessionDropsDeferredButtonsAndResynchronizesBeforeFreshInput() {
        t.queued = 512
        input.motion(10, 20)
        input.buttons(1)
        input.releaseAll(send = false)
        assertFalse(input.hasPendingInput())
        t.queued = 0
        input.motion(30, 40)
        input.flushMotion()
        assertEquals(listOf(6, 7), t.sent.map { it[0].toInt() })
        assertEquals(30, signed16(t.sent.last(), 3))
        assertEquals(0, input.buttonMask)
    }

    @Test fun sequencesAreReportedOnlyWhenDeferredPacketsAreActuallySent() {
        val sequences = ArrayList<Int>()
        val remote = RemoteInput(t, { sequences += it })
        t.queued = 512
        remote.motion(1, 2)
        remote.buttonsAt(100, 200, 1)
        assertTrue(sequences.isEmpty())
        assertNull(remote.lastSentSequence)
        t.queued = 0
        remote.flushMotion()
        assertEquals(listOf(0, 1), sequences)
        assertEquals(1, signed16(t.sent.last(), 1))
    }

    @Test
    fun clipboardPasteUsesType12() {
        assertTrue(input.clipboard("hé", paste = true))
        val p = t.sent.last()
        assertEquals(12, p[0].toInt())
        assertEquals(1, p[1].toInt())
    }

    @Test
    fun textEditReconcilesByCodePoint() {
        assertEquals(TextEdit.Edit(0, "hello"), TextEdit.reconcile("", "hello"))
        assertEquals(TextEdit.Edit(2, "p"), TextEdit.reconcile("hel", "hp"))
        assertEquals(TextEdit.Edit(1, "😃"), TextEdit.reconcile("a😀", "a😃"))
        assertEquals(TextEdit.Edit(0, ""), TextEdit.reconcile("same", "same"))
        assertEquals(TextEdit.Edit(4, ""), TextEdit.reconcile("gone", ""))
    }
}
