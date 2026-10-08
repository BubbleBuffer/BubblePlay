package de.bubblebuffer.bubbleplay.net

import de.bubblebuffer.bubbleplay.protocol.CursorKind
import okio.ByteString.Companion.toByteString
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test

class PendingCursorTest {
    private fun state(x: Int) = byteArrayOf(128.toByte(), 0, 0, x.toByte(), (x shr 8).toByte(), 0, 0, 6).toByteString()

    @Test
    fun rejectsUnknownTypesAndWrongLengthsBeforeAnythingIsQueued() {
        val pending = PendingCursor<Any>()
        val source = Any()
        for (bytes in listOf(byteArrayOf(), byteArrayOf(128.toByte()),
            ByteArray(7).apply { this[0] = 128.toByte() }, ByteArray(9).apply { this[0] = 128.toByte() },
            byteArrayOf(129.toByte()), byteArrayOf(129.toByte(), 1, 2), byteArrayOf(130.toByte(), 1),
            ByteArray(1024 * 1024).apply { this[0] = 128.toByte() })) {
            assertFalse(pending.offer(source, bytes.toByteString()))
            assertNull(pending.take())
        }
        assertTrue(pending.offer(source, state(1)))
    }

    @Test
    fun cursorFloodQueuesOneDeliveryWithOnlyTheLatestPosition() {
        val pending = PendingCursor<Any>()
        val source = Any()
        assertTrue(pending.offer(source, state(0)))
        for (x in 1..10000) assertFalse(pending.offer(source, state(x)))
        val update = pending.take()!!
        assertSame(source, update.source)
        assertEquals(10000, update.state!!.x)
        assertNull(pending.take())
        assertTrue(pending.offer(source, state(10001)))
    }

    @Test
    fun positionDoesNotEraseTheLatestAppearance() {
        val pending = PendingCursor<Any>()
        val source = Any()
        assertTrue(pending.offer(source, byteArrayOf(129.toByte(), 1).toByteString()))
        assertFalse(pending.offer(source, state(3)))
        assertFalse(pending.offer(source, byteArrayOf(129.toByte(), 2).toByteString()))
        val update = pending.take()!!
        assertEquals(CursorKind.Pointer, update.kind)
        assertEquals(3, update.state!!.x)
    }

    @Test
    fun aNewSocketNeverInheritsTheOldSocketsAppearance() {
        val pending = PendingCursor<Any>()
        val old = Any()
        val current = Any()
        assertTrue(pending.offer(old, byteArrayOf(129.toByte(), 1).toByteString()))
        assertFalse(pending.offer(current, state(2)))
        val update = pending.take()!!
        assertSame(current, update.source)
        assertNull(update.kind)
        assertEquals(2, update.state!!.x)
    }

    @Test
    fun malformedPacketCannotDisplaceAPendingValidUpdate() {
        val pending = PendingCursor<Any>()
        val source = Any()
        assertTrue(pending.offer(source, state(42)))
        assertFalse(pending.offer(Any(), byteArrayOf(129.toByte(), 1, 2).toByteString()))
        assertEquals(42, pending.take()!!.state!!.x)
    }
}
