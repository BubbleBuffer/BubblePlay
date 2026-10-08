package de.bubblebuffer.bubbleplay.protocol

import de.bubblebuffer.bubbleplay.Fixtures
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Test

class CursorTypeTest {
    private val fixture = Fixtures.json("cursor-types.json")

    @Test
    fun everySemanticKindMatchesSharedContract() {
        assertEquals(CursorType.PACKET_TYPE, fixture.getInt("packetType"))
        assertEquals(CursorType.PACKET_BYTES, fixture.getInt("packetBytes"))
        assertEquals(CursorKind.Arrow.wireId, fixture.getInt("fallback"))
        val types = fixture.getJSONArray("types")
        assertEquals(CursorKind.entries.size, types.length())
        for (i in 0 until types.length()) {
            val type = types.getJSONObject(i)
            val kind = CursorType.decode(byteArrayOf(CursorType.PACKET_TYPE.toByte(), type.getInt("id").toByte()))!!
            assertEquals(type.getInt("id"), kind.wireId)
            assertEquals(type.getString("name"), kind.name)
        }
    }

    @Test
    fun unknownUnsignedKindsFallBackToArrow() {
        for (id in 15..255) {
            assertEquals("kind $id", CursorKind.Arrow, CursorType.decode(byteArrayOf(129.toByte(), id.toByte())))
        }
    }

    @Test
    fun malformedLengthsAndTypesAreRejected() {
        for (size in 0..10) {
            if (size == 2) continue
            val packet = ByteArray(size) { if (it == 0) 129.toByte() else 1 }
            assertNull("length $size", CursorType.decode(packet))
        }
        for (type in 0..255) {
            if (type == CursorType.PACKET_TYPE) continue
            assertNull("type $type", CursorType.decode(byteArrayOf(type.toByte(), 1)))
        }
    }

    @Test
    fun positionPacketsRemainSeparateAndUnchanged() {
        val position = byteArrayOf(128.toByte(), 0x34, 0x12, 0x78, 0x56, 0xbc.toByte(), 0x9a.toByte(), 7)
        val state = CursorState.decode(position)!!
        assertEquals(CursorState(0x1234, 0x5678, 0x9abc, true, true, true), state)
        assertNull(CursorType.decode(position))
        assertNull(CursorState.decode(byteArrayOf(129.toByte(), 1)))
        assertNull(CursorState.decode(position.copyOf(9)))
        assertNotNull(CursorType.decode(byteArrayOf(129.toByte(), CursorKind.Hidden.wireId.toByte())))
    }
}
