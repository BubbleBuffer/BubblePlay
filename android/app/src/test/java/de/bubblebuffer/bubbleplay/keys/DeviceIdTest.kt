package de.bubblebuffer.bubbleplay.keys

import java.security.MessageDigest
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class DeviceIdTest {
    @Test
    fun fixtureIdsMatchSeedsPublicKeysAndHashes() {
        for (vector in contractIds()) {
            val publicKey = StrictBase64Url.decode(vector.getString("public"), 32)
            DeviceKey.fromSeed(hex(vector.getString("seedHex"))).useKey { key ->
                assertArrayEquals(publicKey, key.publicKey)
                assertEquals(vector.getString("id"), key.id)
            }
            assertEquals(vector.getString("sha256Hex"), MessageDigest.getInstance("SHA-256").digest(publicKey).toHex())
            assertEquals(vector.getString("id"), DeviceId.fromPublicKey(publicKey))
        }
    }

    @Test
    fun parseAcceptsCaseHyphensAndCrockfordAliases() {
        for (vector in contractIds()) {
            val id = vector.getString("id")
            assertEquals(id, DeviceId.parse(id))
            assertEquals(id, DeviceId.parse(id.lowercase()))
            assertEquals(id, DeviceId.parse(id.replace("-", "")))
            if (vector.has("parse")) {
                val parse = vector.getJSONObject("parse")
                assertEquals(parse.getString("canonical"), DeviceId.parse(parse.getString("input")))
                assertEquals(id, DeviceId.parse(parse.getString("input")))
            }
        }
        assertEquals("HN0B-W2W6-0QQ1-1M47", DeviceId.parse("hn0b-w2w6-0qq1-1m47"))
        assertEquals("HN0B-W2W6-0QQ1-1M47", DeviceId.parse("hnobw2w6oqqilm47"))
    }

    @Test
    fun parseRefusesWrongLengthAndOutsideAlphabet() {
        for (input in listOf("HN0BW2W60QQ11M4", "HN0BW2W60QQ11M477", "HN0BW2W60QQ11MU7", "HN0BW2W60QQ11M4!", "")) {
            refusedWith(DeviceKeyException.Reason.MALFORMED) { DeviceId.parse(input) }
        }
    }

    @Test
    fun isCanonicalNeedsTheExactSpelling() {
        assertTrue(DeviceId.isCanonical("HN0B-W2W6-0QQ1-1M47"))
        assertFalse(DeviceId.isCanonical("hn0b-w2w6-0qq1-1m47"))
        assertFalse(DeviceId.isCanonical("HN0BW2W60QQ11M47"))
        assertFalse(DeviceId.isCanonical("HNOB-W2W6-0QQ1-1M47"))
    }
}
