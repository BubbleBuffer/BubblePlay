package de.bubblebuffer.bubbleplay.ui

import de.bubblebuffer.bubbleplay.ui.ModifierKeys.State
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class ModifierKeysTest {
    private val sent = ArrayList<String>()
    private val mods = ModifierKeys { code, up -> sent += (if (up) "-" else "+") + code; true }
    private val keys = LatchedKeySender(mods.latched, mods.active, { code, up -> sent += (if (up) "-" else "+") + code; true }, {})
    private val ctrl = "ControlLeft"
    private val shift = "ShiftLeft"

    @Test fun tapLatchesThenSecondTapLocksThenTapUnlocks() {
        mods.tap(ctrl)
        assertEquals(State.LATCHED, mods.state(ctrl))
        assertTrue("a latch is held back until a key", sent.isEmpty())
        mods.tap(ctrl)
        assertEquals(State.LOCKED, mods.state(ctrl))
        assertEquals(listOf("+ControlLeft"), sent)
        mods.tap(ctrl)
        assertEquals(State.OFF, mods.state(ctrl))
        assertEquals(listOf("+ControlLeft", "-ControlLeft"), sent)
    }

    @Test fun longPressLocksFromOffAndFromLatched() {
        mods.longPress(ctrl)
        mods.tap(shift)
        mods.longPress(shift)
        assertEquals(State.LOCKED, mods.state(ctrl))
        assertEquals(State.LOCKED, mods.state(shift))
        assertTrue(mods.latched.isEmpty())
    }

    @Test fun aKeyConsumesTheLatchAndKeepsTheLock() {
        mods.tap(shift)
        mods.longPress(ctrl)
        sent.clear()
        assertTrue(keys.down("KeyA")); assertTrue(keys.up("KeyA"))
        assertEquals(listOf("+ShiftLeft", "+KeyA", "-KeyA", "-ShiftLeft"), sent)
        assertEquals(State.OFF, mods.state(shift))
        assertEquals(State.LOCKED, mods.state(ctrl))
        sent.clear()
        keys.down("KeyB"); keys.up("KeyB")
        assertEquals(listOf("+KeyB", "-KeyB"), sent)
    }

    @Test fun aPresetChordIgnoresLatchesAndNeverReleasesALock() {
        mods.tap(shift)
        mods.longPress(ctrl)
        sent.clear()
        mods.chord(listOf(ctrl, "AltLeft"), "Tab")
        assertEquals(listOf("+AltLeft", "+Tab", "-Tab", "-AltLeft"), sent)
        assertEquals(State.LATCHED, mods.state(shift))
        assertEquals(State.LOCKED, mods.state(ctrl))
    }

    @Test fun releaseAllLetsGoOfLocksAndForgetsEverything() {
        mods.longPress(ctrl); mods.tap(shift)
        sent.clear()
        mods.releaseAll(release = true)
        assertEquals(listOf("-ControlLeft"), sent)
        assertTrue(!mods.any)
        mods.longPress(ctrl); sent.clear()
        mods.releaseAll(release = false)
        assertTrue("the PC already dropped its input", sent.isEmpty())
        assertTrue(!mods.any)
    }
}
