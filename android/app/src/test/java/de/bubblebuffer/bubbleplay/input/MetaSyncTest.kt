package de.bubblebuffer.bubbleplay.input

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/** Keyboard lock: a synthetic Win press around keys that arrive with META_ON. */
class MetaSyncTest {
    @Test fun synthesizesWinAroundAMetaOnKey() {
        val m = MetaSync()
        assertTrue(m.beforeDown(metaOn = true))
        assertTrue(m.afterUp())
        assertFalse("released once", m.afterUp())
    }

    @Test fun plainKeysNeedNoWin() {
        val m = MetaSync()
        assertFalse(m.beforeDown(metaOn = false))
        assertFalse(m.afterUp())
    }

    @Test fun oneSyntheticPressForOverlappingKeys() {
        val m = MetaSync()
        assertTrue(m.beforeDown(true))
        assertFalse("already held", m.beforeDown(true))
        assertTrue(m.afterUp())
        assertFalse(m.afterUp())
    }

    @Test fun aRealMetaKeySuppressesTheSyntheticOne() {
        val m = MetaSync()
        m.observeMeta("MetaLeft", up = false)
        assertFalse(m.beforeDown(true))
        assertFalse(m.afterUp())
        m.observeMeta("MetaLeft", up = true)
        assertTrue(m.beforeDown(true))
    }

    @Test fun resetDropsHeldState() {
        val m = MetaSync()
        m.observeMeta("MetaRight", up = false)
        m.beforeDown(true)
        m.reset()
        assertFalse(m.afterUp())
        assertTrue(m.beforeDown(true))
    }
}
