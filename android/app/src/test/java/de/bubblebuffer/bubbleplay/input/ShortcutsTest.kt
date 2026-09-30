package de.bubblebuffer.bubbleplay.input

import de.bubblebuffer.bubbleplay.protocol.KeyMap
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test

class ShortcutsTest {
    @Test
    fun everyChordKeyIsSendableOnBothHostKinds() {
        for (chord in Shortcuts.ALL) for (code in chord.mods + chord.key) {
            assertNotNull("${chord.keys}: $code (scancode)", KeyMap.packet(code, false, true))
            assertNotNull("${chord.keys}: $code (vk)", KeyMap.packet(code, false, false))
        }
        for (code in Shortcuts.MODIFIERS) assertTrue(code in KeyMap.MODIFIERS)
    }

    @Test
    fun chordLabels() {
        val keys = Shortcuts.ALL.map { it.keys }
        assertTrue("Ctrl+C" in keys)
        assertTrue("Ctrl+Shift+T" in keys)
        assertTrue("Ctrl+Shift+Esc" in keys)
        assertTrue("Alt+F4" in keys)
        assertTrue("Win" in keys)
        assertTrue("Win+D" in keys)
        assertEquals(19, keys.size)
    }

    @Test
    fun noSecureAttentionSequence() {
        assertFalse(Shortcuts.ALL.any { "ControlLeft" in it.mods && "AltLeft" in it.mods && it.key == "Delete" })
    }
}
