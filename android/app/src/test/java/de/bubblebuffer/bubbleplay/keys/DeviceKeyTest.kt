package de.bubblebuffer.bubbleplay.keys

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotSame
import org.junit.Assert.assertTrue
import org.junit.Test

class DeviceKeyTest {
    @Test
    fun closedKeyKeepsItsPublicIdentityAndRefusesSeedOperations() {
        val created = KeyFileCodec.create("Phone", null)
        val key = created.key
        key.close()
        key.close()
        assertTrue(key.isClosed)
        assertEquals(created.file.id, key.id)
        assertArrayEquals(created.file.publicKey, key.publicKey)
        refusedWith(DeviceKeyException.Reason.CLOSED) { key.withSeed { it.size } }
        refusedWith(DeviceKeyException.Reason.CLOSED) { key.sign(ByteArray(3)) }
        refusedWith(DeviceKeyException.Reason.CLOSED) { KeyFileCodec.rewrap(created.file, key, null) }
        refusedWith(DeviceKeyException.Reason.CLOSED) { KeyFileCodec.rewrap(created.file, key, "passphrase") }
    }

    @Test
    fun seedBoxZeroesItsBufferOnCloseAndRefusesReads() {
        val buffer = ByteArray(32) { 7 }
        val box = SeedBox(buffer)
        assertEquals(7, box.read { it[0].toInt() })
        box.close()
        assertTrue(buffer.all { it == 0.toByte() })
        assertTrue(box.isClosed)
        refusedWith(DeviceKeyException.Reason.CLOSED) { box.read { it.size } }
        box.close()
    }

    @Test
    fun keyHoldsItsOwnCopyOfTheSeed() {
        val vector = contractIds().first { it.getString("name") == "viewer" }
        val seed = hex(vector.getString("seedHex"))
        val key = DeviceKey.fromSeed(seed)
        seed.fill(0)
        key.useKey { held ->
            assertTrue(held.withSeed { it.contentEquals(hex(vector.getString("seedHex"))) })
            assertEquals(vector.getString("id"), held.id)
        }
    }

    @Test
    fun publicKeyReturnsACopy() {
        val key = DeviceKey.fromSeed(hex(contractIds().first { it.getString("name") == "pc" }.getString("seedHex")))
        key.useKey { held ->
            val first = held.publicKey
            first.fill(0)
            assertFalse(held.publicKey.all { it == 0.toByte() })
            assertNotSame(first, held.publicKey)
        }
    }

    @Test
    fun signMatchesFixtureHandshakeSignatures() {
        val handshake = contractHandshake()
        val pc = handshake.getJSONObject("pc")
        val viewer = handshake.getJSONObject("viewer")
        val dtlsPc = handshake.getJSONObject("dtls").getJSONObject("pc")
        val dtlsViewer = handshake.getJSONObject("dtls").getJSONObject("viewer")

        DeviceKey.fromSeed(hex(pc.getString("seedHex"))).useKey { key ->
            assertArrayEquals(StrictBase64Url.decode(handshake.getString("pcSignature"), 64), key.sign(hex(handshake.getString("pcSignedBytesHex"))))
            assertArrayEquals(StrictBase64Url.decode(dtlsPc.getString("signature"), 64), key.sign(hex(dtlsPc.getString("signedBytesHex"))))
        }
        DeviceKey.fromSeed(hex(viewer.getString("seedHex"))).useKey { key ->
            assertArrayEquals(StrictBase64Url.decode(handshake.getString("viewerSignature"), 64), key.sign(hex(handshake.getString("viewerSignedBytesHex"))))
            assertArrayEquals(StrictBase64Url.decode(dtlsViewer.getString("signature"), 64), key.sign(hex(dtlsViewer.getString("signedBytesHex"))))
        }
    }

    @Test
    fun signWithTheWrongSeedDoesNotMatchTheFixture() {
        val handshake = contractHandshake()
        DeviceKey.fromSeed(hex(handshake.getJSONObject("viewer").getString("seedHex"))).useKey { key ->
            assertFalse(
                StrictBase64Url.decode(handshake.getString("pcSignature"), 64)
                    .contentEquals(key.sign(hex(handshake.getString("pcSignedBytesHex")))),
            )
        }
    }
}
