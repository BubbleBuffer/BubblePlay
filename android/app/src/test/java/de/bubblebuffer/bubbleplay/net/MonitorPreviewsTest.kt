package de.bubblebuffer.bubbleplay.net

import de.bubblebuffer.bubbleplay.Fixtures
import org.json.JSONObject
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.util.Base64

class MonitorPreviewsTest {
    private val D2 = """\\.\DISPLAY2"""
    private val D3 = """\\.\DISPLAY3"""
    private val contract = Fixtures.json("monitor-previews-contract.json")
    private val valid: JSONObject get() = copy(vector("accept", "valid"))

    private fun vector(group: String, name: String): JSONObject {
        val array = contract.getJSONArray(group)
        return Fixtures.objects(array).first { it.getString("name") == name }.getJSONObject("json")
    }

    private fun copy(json: JSONObject) = JSONObject(json.toString())

    @Test fun capabilityMatchesTheContract() {
        assertEquals(contract.getString("capability"), MonitorPreviews.CAP)
    }

    @Test fun validMessageIsHeldAsTheLatestPictureOfItsMonitor() {
        val previews = MonitorPreviews()
        assertTrue(previews.onMessage(valid))
        val picture = previews.take(D2)
        assertNotNull(picture)
        assertEquals(7L, picture!!.seq)
        assertEquals(16, picture.width)
        assertEquals(9, picture.height)
        assertArrayEquals(Base64.getDecoder().decode(valid.getString("jpeg")), picture.jpeg)
        assertNull(previews.take(D2)) // taking removes it
    }

    @Test fun unknownExtraFieldIsIgnored() {
        val previews = MonitorPreviews()
        val json = Fixtures.objects(contract.getJSONArray("accept")).first { it.getString("name") == "unknown-extra-field-ignored" }
        assertTrue(previews.onMessage(json.getJSONObject("json")))
        assertTrue(previews.has(D3))
    }

    @Test fun everyIgnoreVectorIsRefused() {
        for (entry in Fixtures.objects(contract.getJSONArray("ignore"))) {
            val name = entry.getString("name")
            val previews = MonitorPreviews()
            val after = entry.optString("after")
            if (after.isNotEmpty()) assertTrue(name, previews.onMessage(vector("accept", after)))
            assertFalse(name, previews.onMessage(entry.getJSONObject("json")))
            // Nothing new was held: only the earlier picture (if the vector has one) is still there.
            val held = previews.take(D2)
            if (after.isEmpty()) assertNull(name, held) else assertEquals(name, 7L, held!!.seq)
        }
    }

    @Test fun olderSeqKeepsTheHeldPictureAndNewerReplacesIt() {
        val previews = MonitorPreviews()
        assertTrue(previews.onMessage(valid))
        assertFalse(previews.onMessage(valid.put("seq", 6)))
        assertTrue(previews.onMessage(valid.put("seq", 9)))
        assertEquals(9L, previews.take(D2)!!.seq)
    }

    @Test fun aTakenPictureCannotBeBroughtBackByAStaleMessage() {
        val previews = MonitorPreviews()
        assertTrue(previews.onMessage(valid))
        previews.take(D2)
        assertFalse(previews.onMessage(valid.put("seq", 5)))
        assertFalse(previews.has(D2))
    }

    @Test fun oneMonitorDoesNotAffectAnother() {
        val previews = MonitorPreviews()
        assertTrue(previews.onMessage(valid))
        assertTrue(previews.onMessage(valid.put("monitor", D3).put("seq", 1)))
        assertEquals(1L, previews.take(D3)!!.seq)
        assertEquals(7L, previews.take(D2)!!.seq)
    }

    @Test fun clearDropsEverythingAndRestartsTheSeqRule() {
        val previews = MonitorPreviews()
        assertTrue(previews.onMessage(valid))
        previews.clear()
        assertFalse(previews.has(D2))
        assertTrue(previews.onMessage(valid.put("seq", 1))) // a new session starts at a low seq again
    }

    @Test fun oversizeAndMalformedPicturesAreIgnored() {
        val previews = MonitorPreviews()
        val big = ByteArray(contract.getJSONObject("limits").getInt("maxJpegBytes") + 1)
        big[0] = 0xFF.toByte(); big[1] = 0xD8.toByte(); big[big.size - 2] = 0xFF.toByte(); big[big.size - 1] = 0xD9.toByte()
        assertFalse(previews.onMessage(valid.put("jpeg", Base64.getEncoder().encodeToString(big))))
        assertFalse(previews.onMessage(valid.put("w", 961)))
        assertFalse(previews.onMessage(valid.put("v", "1")))
        assertFalse(previews.onMessage(valid.put("seq", -1)))
        assertFalse(previews.onMessage(valid.put("type", "other")))
        // SOI present but no EOI.
        val truncated = Base64.getDecoder().decode(valid.getString("jpeg")).copyOf(40)
        assertFalse(previews.onMessage(valid.put("jpeg", Base64.getEncoder().encodeToString(truncated))))
        assertFalse(previews.has(D2))
    }
}
