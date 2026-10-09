package de.bubblebuffer.bubbleplay.net

import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Test

class InputStatusWarningTest {
    private fun message() = JSONObject().put("type", "input-status").put("protocol", 1)

    @Test fun olderReceiptMessagesHaveNoWarningUpdate() {
        assertEquals(InputStatusUpdate(false, null, null), InputStatusUpdate.parse(message().put("receivedPackets", 4)))
    }

    @Test fun parsesBothReasonsAndAllRoutes() {
        for (reason in listOf("admin-required", "admin-unavailable")) {
            for (route in listOf("local", "admin", "secure")) {
                assertEquals(InputStatusUpdate(true, reason, route),
                    InputStatusUpdate.parse(message().put("reason", reason).put("route", route)))
            }
        }
        assertEquals(InputStatusUpdate(true, null, "local"),
            InputStatusUpdate.parse(message().put("reason", JSONObject.NULL).put("route", "local")))
    }

    @Test fun rejectsUnknownValuesAndWrongTypes() {
        for (value in listOf("other", "", 1, true)) {
            assertNull(InputStatusUpdate.parse(message().put("reason", value)))
            assertNull(InputStatusUpdate.parse(message().put("route", value)))
        }
        assertNull(InputStatusUpdate.parse(message().put("route", JSONObject.NULL)))
        assertNull(InputStatusUpdate.parse(message().put("protocol", "1")))
        assertNull(InputStatusUpdate.parse(message().put("protocol", 2)))
        assertNull(InputStatusUpdate.parse(JSONObject()))
    }

    @Test fun warningAppearsSurvivesReceiptsAndClearsExplicitly() {
        val warning = InputStatusWarning()
        warning.apply(InputStatusUpdate.parse(message().put("reason", "admin-required").put("route", "local"))!!)
        assertEquals("This window needs Admin mode — turn it on at the PC", warning.text)
        assertEquals("local", warning.route)
        warning.apply(InputStatusUpdate.parse(message())!!)
        assertEquals("admin-required", warning.reason)
        warning.apply(InputStatusUpdate.parse(message().put("reason", "admin-unavailable").put("route", "admin"))!!)
        assertEquals("Admin mode helper isn’t responding", warning.text)
        warning.apply(InputStatusUpdate.parse(message().put("reason", JSONObject.NULL))!!)
        assertNull(warning.text)
        warning.apply(InputStatusUpdate(true, "admin-required", "local"))
        warning.reset()
        assertNull(warning.text)
        assertNull(warning.route)
    }
}
