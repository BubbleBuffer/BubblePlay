package de.bubblebuffer.bubbleplay.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class ImeTextSenderTest {
    private val events = ArrayList<String>()
    private val latches = linkedSetOf<String>()
    private val active = hashMapOf<String, Set<String>>()
    private val keys = LatchedKeySender(latches, active,
        { code, up -> events += "$code:${if (up) "up" else "down"}"; true }, {})
    private var acceptsText = true
    private val sender = ImeTextSender(
        text = { if (acceptsText) events += "text:$it"; acceptsText },
        rawTap = { code -> events += "$code:down"; events += "$code:up"; true },
        keyTap = { keys.down(it) && keys.up(it) },
        hasLatch = { latches.isNotEmpty() },
        requestReset = { events += "requestReset" },
    )

    @Test fun committedNewlineSendsTextThenExactlyOneEnterTapThenRequestsReset() {
        assertTrue(sender.send(0, "hello\n"))
        assertEquals(listOf("text:hello", "Enter:down", "Enter:up", "requestReset"), events)
    }

    @Test fun enterWithLatchedModifiersReleasesThemBeforeRequestingReset() {
        latches += listOf("ControlLeft", "ShiftLeft")
        assertTrue(sender.send(0, "hello\n"))
        assertEquals(listOf("text:hello", "ControlLeft:down", "ShiftLeft:down", "Enter:down",
            "Enter:up", "ShiftLeft:up", "ControlLeft:up", "requestReset"), events)
        assertTrue(latches.isEmpty())
        assertTrue(active.isEmpty())
    }

    @Test fun loneNewlineWithLatchUsesOneCompleteEnterTap() {
        latches += "ControlLeft"
        assertTrue(sender.send(0, "\n"))
        assertEquals(listOf("ControlLeft:down", "Enter:down", "Enter:up", "ControlLeft:up", "requestReset"), events)
    }

    @Test fun emptyEditorNewlineSendsOneTapWithoutEmptyTextPacket() {
        assertTrue(sender.send(0, "\n"))
        assertEquals(listOf("Enter:down", "Enter:up", "requestReset"), events)
    }

    @Test fun failedPendingTextCannotSendEnterAheadOfIt() {
        acceptsText = false
        assertFalse(sender.send(0, "hello\n"))
        assertEquals(listOf("requestReset"), events)
    }

    @Test fun ordinaryTextKeepsContextAndBackspacesDoNotConsumeAnEnterLatch() {
        latches += "ControlLeft"
        assertTrue(sender.send(1, "hello"))
        assertEquals(listOf("Backspace:down", "Backspace:up", "text:hello"), events)
        assertEquals(setOf("ControlLeft"), latches)
    }
}
