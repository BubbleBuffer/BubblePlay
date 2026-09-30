package de.bubblebuffer.bubbleplay.protocol

import de.bubblebuffer.bubbleplay.Fixtures
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test

class KeyMapTest {
    private val contract = Fixtures.json("input-contract.json")

    @Test
    fun scanCodesMatchContractExactly() {
        val expected = contract.getJSONObject("scanCodes")
        val names = expected.keys().asSequence().toSet()
        assertEquals("same set of codes as input-contract.json", names, KeyMap.SCAN_CODES.keys)
        for (code in names) {
            val e = expected.getJSONObject(code)
            assertEquals(code, e.getInt("scan") to e.getBoolean("extended"), KeyMap.SCAN_CODES[code])
        }
    }

    @Test
    fun mustBeExtendedKeysCarryTheE0Flag() {
        val list = contract.getJSONArray("mustBeExtended")
        for (i in 0 until list.length()) {
            val code = list.getString(i)
            val key = KeyMap.descriptor(code)
            assertNotNull(code, key)
            assertTrue("$code must be extended", key!!.extended)
            val packet = KeyMap.packet(code, up = false, scancodeHost = true)!!
            assertEquals("$code packet flag", 2, packet[1].toInt() and 2)
        }
    }

    @Test
    fun vkOnlyKeysUseTheVkFallback() {
        val vkOnly = contract.getJSONObject("vkOnly")
        for (code in vkOnly.keys()) {
            val vk = vkOnly.getInt(code)
            val key = KeyMap.descriptor(code)!!
            assertEquals("$code scan", 0, key.scan)
            assertEquals("$code vk", vk, key.vk)
            assertArrayEquals(code, byteArrayOf(13, 0, 0, 0, vk.toByte(), (vk ushr 8).toByte()), KeyMap.packet(code, false, true))
        }
    }

    @Test
    fun contractPacketsAreProducedByteForByte() {
        val layout = contract.getJSONObject("scanKeyPacket")
        assertEquals(13, layout.getInt("type"))
        for (p in Fixtures.objects(contract.getJSONArray("packets"))) {
            val name = p.getString("name")
            val bytes = Fixtures.bytes(p.getJSONArray("bytes"))
            if (p.has("clipboard")) {
                val c = p.getJSONObject("clipboard")
                val packets = Clipboard.packets(c.getString("text"), c.getInt("id"), c.getBoolean("paste"))!!
                assertEquals(name, c.getInt("count"), packets.size)
                assertArrayEquals(name, bytes, packets[c.getInt("index")])
                continue
            }
            val key = p.getJSONObject("key")
            val up = p.getBoolean("up")
            val candidates = if (key.has("vk")) {
                val vkOnly = contract.getJSONObject("vkOnly")
                vkOnly.keys().asSequence().filter { vkOnly.getInt(it) == key.getInt("vk") }.toList()
            } else {
                KeyMap.SCAN_CODES.filterValues { it == (key.getInt("scan") to key.getBoolean("extended")) }.keys.toList()
            }
            assertTrue("$name has a code", candidates.isNotEmpty())
            val produced = candidates.mapNotNull { KeyMap.packet(it, up, scancodeHost = true) }
            assertTrue("$name: one of $candidates encodes to the contract bytes", produced.any { it.contentEquals(bytes) })
        }
    }

    @Test
    fun noEncoderProducesAnInvalidPacket() {
        val invalid = (0 until contract.getJSONArray("invalidPackets").length())
            .map { Fixtures.bytes(contract.getJSONArray("invalidPackets").getJSONArray(it)) }
        val produced = ArrayList<ByteArray>()
        for (code in KeyMap.SCAN_CODES.keys + KeyMap.VK_CODES.keys) for (up in listOf(false, true)) {
            KeyMap.packet(code, up, scancodeHost = true)?.let { produced += it }
        }
        for (text in listOf("a", "hé", "x".repeat(Clipboard.CHUNK_BYTES * 2 + 1))) for (paste in listOf(false, true)) {
            produced += Clipboard.packets(text, 0, paste)!!
        }
        for (p in produced) for (bad in invalid) assertFalse("produced invalid ${bad.toList()}", p.contentEquals(bad))
        // Every scan-key packet is well formed: 6 bytes, known flags, scan or vk set.
        for (p in produced.filter { it[0].toInt() == 13 }) {
            assertEquals(6, p.size)
            assertEquals(0, p[1].toInt() and 3.inv())
            assertTrue((p[2].toInt() or p[3].toInt() or p[4].toInt() or p[5].toInt()) != 0)
        }
    }

    @Test
    fun clipboardLimitsMatchContract() {
        val c = contract.getJSONObject("clipboardPacket")
        assertEquals(c.getInt("chunkBytes"), Clipboard.CHUNK_BYTES)
        assertEquals(c.getInt("maxChunks"), Clipboard.MAX_CHUNKS)
        assertEquals(c.getInt("maxBytes"), Clipboard.MAX_BYTES)
        assertEquals(contract.getJSONObject("hostClipboardMessage").getInt("maxChunks"), Clipboard.HOST_MAX_CHUNKS)
        assertEquals(null, Clipboard.packets("", 1))
        assertEquals(null, Clipboard.packets("x".repeat(Clipboard.MAX_BYTES + 1), 1))
        val max = Clipboard.packets("x".repeat(Clipboard.MAX_BYTES), 65537)!!
        assertEquals(Clipboard.MAX_CHUNKS, max.size)
        assertEquals(1, max[0][2].toInt()) // id is u16
        assertEquals(0, max[0][3].toInt())
    }

    @Test
    fun hostClipboardReassembly() {
        val r = ClipboardReceiver()
        assertEquals(null, r.push(1, 5, 0, 2, "hel"))
        assertEquals("hello", r.push(1, 5, 1, 2, "lo"))
        assertEquals("one", r.push(1, 6, 0, 1, "one"))
        // Out-of-order chunk or wrong protocol aborts the transfer.
        assertEquals(null, r.push(1, 7, 0, 3, "a"))
        assertEquals(null, r.push(1, 7, 2, 3, "c"))
        assertEquals(null, r.push(1, 7, 1, 3, "b"))
        assertEquals(null, r.push(2, 8, 0, 1, "x"))
        assertEquals(null, r.push(1, 8, 0, Clipboard.HOST_MAX_CHUNKS + 1, "x"))
    }

    @Test
    fun androidKeysAllHaveDescriptors() {
        for (code in KeyMap.ANDROID_TO_CODE.values) assertNotNull("no descriptor for $code", KeyMap.descriptor(code))
        assertEquals("KeyC", KeyMap.codeForShortcutChar('c'))
        assertEquals("Digit5", KeyMap.codeForShortcutChar('5'))
    }

    @Test
    fun legacyHostsGetVkPackets() {
        assertArrayEquals(byteArrayOf(0, 0x10, 0), KeyMap.packet("ShiftLeft", false, scancodeHost = false))
        assertArrayEquals(byteArrayOf(1, 0x25, 0), KeyMap.packet("ArrowLeft", true, scancodeHost = false))
    }

    @Test
    fun cursorStateDecodes() {
        val s = CursorState.decode(byteArrayOf(128.toByte(), 3, 0, 0xff.toByte(), 0x7f, 1, 0, 6))!!
        assertEquals(3, s.ackSequence)
        assertEquals(0x7fff, s.x)
        assertEquals(1, s.y)
        assertFalse(s.hasAck)
        assertTrue(s.inBounds)
        assertTrue(s.visible)
        assertEquals(null, CursorState.decode(byteArrayOf(127, 0, 0, 0, 0, 0, 0, 0)))
    }
}
