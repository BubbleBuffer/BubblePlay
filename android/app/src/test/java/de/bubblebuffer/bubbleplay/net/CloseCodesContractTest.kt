package de.bubblebuffer.bubbleplay.net

import de.bubblebuffer.bubbleplay.Fixtures
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/** fixtures/close-codes-contract.json: what the Android viewer does and shows on each close code of its signaling socket. */
class CloseCodesContractTest {
    private val contract = Fixtures.json("close-codes-contract.json")
    private val policyCode = 1008

    @Test
    fun policyCloseIsTerminalWithTheContractMessage() {
        val actions = contract.getJSONObject("clientActions")
        assertEquals("terminal", actions.getJSONObject("policy1008").getJSONObject("kotlin").getString("signaling"))
        val messages = contract.getJSONObject("userMessages").getJSONObject("kotlin")
        assertEquals(messages.getString("admittedKey"), SessionProblem.closed(policyCode, admitted = true)?.resource)
        assertEquals(messages.getString("notAdmittedKey"), SessionProblem.closed(policyCode, admitted = false)?.resource)
    }

    @Test
    fun everyOtherCloseCodeRetries() {
        val others = contract.getJSONObject("clientActions").getJSONObject("otherCodes")
        assertEquals("retry", others.getJSONObject("kotlin").getString("signaling"))
        val codes = others.getJSONArray("codes")
        for (i in 0 until codes.length()) assertNull("close ${codes.getInt(i)}", SessionProblem.closed(codes.getInt(i), admitted = true))
    }
}
