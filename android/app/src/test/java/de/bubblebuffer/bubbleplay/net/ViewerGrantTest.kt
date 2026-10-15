package de.bubblebuffer.bubbleplay.net

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test

class ViewerGrantTest {
    private val pc = "B7QK-2M9X-H4PD-R8TN"
    private val ticket = "A".repeat(42) + "_"
    private val mediaHello = "{\"type\":\"hello\",\"role\":\"media-client\"}"
    private val fileHello = "{\"type\":\"hello\",\"role\":\"file-client\"}"

    private fun helloOk(json: String) = JSONObject(json)

    @Test fun absentFieldsKeepTheLegacyHelloBytes() {
        val grant = ViewerGrant.fromHelloOk(helloOk("{\"type\":\"hello-ok\"}"))
        assertSame(ViewerGrant.NONE, grant)
        assertNull(grant.pc)
        assertEquals(mediaHello, grant.hello("media-client"))
        assertEquals(fileHello, grant.hello("file-client"))
    }

    @Test fun emptyStringsCountAsAbsent() {
        val grant = ViewerGrant.fromHelloOk(helloOk("{\"pc\":\"\",\"ticket\":\"\"}"))
        assertSame(ViewerGrant.NONE, grant)
        assertEquals(mediaHello, grant.hello("media-client"))
    }

    @Test fun nonStringValuesCountAsAbsent() {
        // optString would have coerced these to text; typed extraction does not.
        listOf(
            "{\"pc\":123,\"ticket\":456}",
            "{\"pc\":null,\"ticket\":null}",
            "{\"pc\":true,\"ticket\":false}",
            "{\"pc\":[\"B7QK-2M9X-H4PD-R8TN\"],\"ticket\":{\"t\":\"x\"}}",
        ).forEach { json ->
            val grant = ViewerGrant.fromHelloOk(helloOk(json))
            assertNull(json, grant.pc)
            assertEquals(json, mediaHello, grant.hello("media-client"))
        }
    }

    @Test fun pcWithoutTicketKeepsTheLegacyHello() {
        val grant = ViewerGrant.fromHelloOk(helloOk("{\"pc\":\"$pc\"}"))
        assertEquals(pc, grant.pc)
        assertEquals(mediaHello, grant.hello("media-client"))
        assertEquals(fileHello, grant.hello("file-client"))
    }

    @Test fun pcAndTicketGoIntoTheHelloJson() {
        val grant = ViewerGrant.fromHelloOk(JSONObject().put("pc", pc).put("ticket", ticket))
        assertEquals(pc, grant.pc)
        val json = JSONObject(grant.hello("media-client"))
        assertEquals(3, json.length())
        assertEquals("hello", json.getString("type"))
        assertEquals("media-client", json.getString("role"))
        assertEquals(ticket, json.getString("ticket"))
        assertEquals("file-client", JSONObject(grant.hello("file-client")).getString("role"))
    }

    @Test fun pcValuesAreOpaque() {
        // The client does not validate the relay's values beyond "nonempty string".
        val grant = ViewerGrant.from("not-a-canonical-id", "short")
        assertEquals("not-a-canonical-id", grant.pc)
        assertEquals("short", JSONObject(grant.hello("file-client")).getString("ticket"))
    }

    @Test fun ticketWithSpecialCharactersRoundTripsThroughTheHello() {
        listOf(
            "quote\"inside",
            "back\\slash",
            "new\nline\ttab",
            "control\u0001char",
            "non-ascii é 日本 🎲",
            "</script>&amp;",
            "end}\"],{\"role\":\"viewer",
        ).forEach { raw ->
            val hello = ViewerGrant.from(pc, raw).hello("media-client")
            val parsed = JSONObject(hello)
            assertEquals(raw, parsed.getString("ticket"))
            assertEquals("hello", parsed.getString("type"))
            assertEquals("media-client", parsed.getString("role"))
            assertEquals(3, parsed.length())
        }
    }

    @Test fun ticketNeverAppearsInToStringEvenWhenItIsOddText() {
        val odd = "secret-\"quoted\"-\\é"
        val grant = ViewerGrant.from(pc, odd)
        assertFalse(grant.toString().contains(odd))
        assertFalse(grant.toString().contains("secret"))
        assertTrue(grant.toString().contains(pc))
        assertEquals("ViewerGrant(pc=$pc, ticket=<redacted>)", grant.toString())
    }

    @Test fun ticketOnlyGoesIntoTheHelloOfThatGrant() {
        val grant = ViewerGrant.from(null, ticket)
        assertNull(grant.pc)
        assertEquals(ticket, JSONObject(grant.hello("file-client")).getString("ticket"))
        assertEquals("ViewerGrant(pc=null, ticket=<redacted>)", grant.toString())
    }

    @Test fun slotReplacesOnEveryHelloOk() {
        val slot = ViewerGrantSlot()
        assertSame(ViewerGrant.NONE, slot.current)

        slot.helloOk(JSONObject().put("pc", pc).put("ticket", ticket))
        assertEquals(pc, slot.current.pc)

        val other = "C7QK-2M9X-H4PD-R8TN"
        val otherTicket = "B".repeat(42) + "-"
        slot.helloOk(JSONObject().put("pc", other).put("ticket", otherTicket))
        assertEquals(other, slot.current.pc)
        assertEquals(otherTicket, JSONObject(slot.current.hello("media-client")).getString("ticket"))

        // A hello-ok without the optional fields clears what the previous one set.
        slot.helloOk(JSONObject().put("type", "hello-ok"))
        assertSame(ViewerGrant.NONE, slot.current)
        assertEquals(mediaHello, slot.current.hello("media-client"))
    }

    @Test fun slotClearsOnceWhenSignalingCloses() {
        val slot = ViewerGrantSlot()
        assertFalse(slot.clear())
        slot.helloOk(JSONObject().put("pc", pc).put("ticket", ticket))
        assertTrue(slot.clear())
        assertSame(ViewerGrant.NONE, slot.current)
        assertFalse(slot.clear())
        slot.helloOk(JSONObject())
        assertNull(slot.current.pc)
    }

    @Test fun replacedGrantIsADifferentObjectEvenWithEqualValues() {
        // Session compares grants by identity to drop a socket whose ticket was replaced.
        val slot = ViewerGrantSlot()
        val first = slot.helloOk(JSONObject().put("pc", pc).put("ticket", ticket))
        val second = slot.helloOk(JSONObject().put("pc", pc).put("ticket", ticket))
        assertFalse(first === second)
        assertSame(second, slot.current)
    }
}
