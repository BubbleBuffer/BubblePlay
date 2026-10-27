package de.bubblebuffer.bubbleplay.net

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class ViewerGrantTest {
    @Test fun dataHelloCarriesTicketAndGrantDoesNotExposeItInLogs() {
        val ticket = "A".repeat(43)
        val grant = ViewerGrant.from("B7QK-2M9X-H4PD-R8TN", ticket)
        assertTrue(grant.hello("file-viewer").contains(ticket))
        assertFalse(grant.toString().contains(ticket))
    }
}
