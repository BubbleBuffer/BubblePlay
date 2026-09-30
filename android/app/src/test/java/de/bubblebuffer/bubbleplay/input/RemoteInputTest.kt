package de.bubblebuffer.bubbleplay.input

import de.bubblebuffer.bubbleplay.ui.TextEdit
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class RemoteInputTest {
    private class FakeTransport : RemoteInput.Transport {
        var open = true
        var queued = 0L
        val sent = ArrayList<ByteArray>()
        override fun inputOpen() = open
        override fun send(bytes: ByteArray): Boolean { sent += bytes; return true }
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
