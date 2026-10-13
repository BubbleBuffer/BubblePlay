package de.bubblebuffer.bubbleplay.protocol

import de.bubblebuffer.bubbleplay.Fixtures
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/** fixtures/follow-window-contract.json: capability, the follow control and monitor-list state. */
class FollowWindowTest {
    private val signaling = Fixtures.json("follow-window-contract.json").getJSONObject("signaling")
    private fun case(name: String) = JSONObject(signaling.getJSONObject(name).getString("text"))

    @Test fun capabilityFromTheFixture() {
        assertTrue(FollowWindow.capable(case("hostCapabilities")))
    }

    @Test fun capabilityNeedsAnIntegerVersionOfAtLeastOne() {
        assertFalse(FollowWindow.capable(JSONObject("""{"type":"host-capabilities","fileTransfer":1}""")))
        assertFalse(FollowWindow.capable(JSONObject("""{"followWindow":0}""")))
        assertFalse(FollowWindow.capable(JSONObject("""{"followWindow":-1}""")))
        assertFalse(FollowWindow.capable(JSONObject("""{"followWindow":1.5}""")))
        assertFalse(FollowWindow.capable(JSONObject("""{"followWindow":"1"}""")))
        assertFalse(FollowWindow.capable(JSONObject("""{"followWindow":true}""")))
        assertFalse(FollowWindow.capable(JSONObject("""{"followWindow":null}""")))
        assertTrue(FollowWindow.capable(JSONObject("""{"followWindow":2}""")))
    }

    @Test fun controlMatchesTheFixture() {
        val expected = case("followControl")
        val actual = FollowWindow.control(true, expected.getString("epoch"))
        val keys = expected.keys().asSequence().toSet()
        assertEquals(keys, actual.keys().asSequence().toSet())
        for (key in keys) assertEquals(key, expected.get(key), actual.get(key))
        assertEquals(false, FollowWindow.control(false, expected.getString("epoch")).get("follow"))
    }

    @Test fun monitorListState() {
        assertEquals(true, FollowWindow.listState(case("monitorList")))
        val off = case("monitorList").put("follow", false)
        assertEquals(false, FollowWindow.listState(off))
        val absent = case("monitorList").apply { remove("follow") }
        assertNull(FollowWindow.listState(absent))
        assertNull(FollowWindow.listState(case("monitorList").put("follow", "true")))
    }
}
