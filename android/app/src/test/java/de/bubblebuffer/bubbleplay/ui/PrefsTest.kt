package de.bubblebuffer.bubbleplay.ui

import de.bubblebuffer.bubbleplay.input.PointerSpeed
import de.bubblebuffer.bubbleplay.net.ServerConfig
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class PrefsTest {
    /** SharedPreferences stand-in: typed reads return null for a value of another type, like SharedPrefStore. */
    private class MapStore(val map: HashMap<String, Any> = HashMap()) : PrefStore {
        var writes = 0
        override fun getString(key: String) = map[key] as? String
        override fun getBoolean(key: String) = map[key] as? Boolean
        override fun getInt(key: String) = map[key] as? Int
        override fun edit(block: PrefStore.Editor.() -> Unit) {
            writes++
            object : PrefStore.Editor {
                override fun putString(key: String, value: String) { map[key] = value }
                override fun putBoolean(key: String, value: Boolean) { map[key] = value }
                override fun putInt(key: String, value: Int) { map[key] = value }
                override fun remove(key: String) { map.remove(key) }
            }.block()
        }
    }

    @Test
    fun defaultsOnAFreshInstall() {
        val store = MapStore()
        val prefs = Prefs(store)
        assertEquals("", prefs.server)
        assertEquals("", prefs.labRun)
        assertEquals(Prefs.CODEC_AUTO, prefs.codec)
        assertFalse(prefs.directTouch)
        assertEquals(PointerSpeed.DEFAULT, prefs.pointerSpeed)
        assertTrue(prefs.tapToClick)
        assertTrue(prefs.startFullscreen)
        assertTrue(prefs.keepScreenOn)
        assertEquals("", prefs.defaultMonitor)
        assertEquals(Prefs.VERSION, store.map[Prefs.KEY_VERSION])
        assertNull("a fresh install has no relay to contact", prefs.serverConfig())
    }

    @Test
    fun migratesVersionOneKeepingTheConnectionAndTouchMode() {
        val store = MapStore(hashMapOf(
            Prefs.KEY_SERVER to "https://relay.example.dev/",
            Prefs.KEY_LAB_RUN to "",
            Prefs.KEY_CODEC to "H.265",
            Prefs.KEY_DIRECT_TOUCH to true,
        ))
        val prefs = Prefs(store)
        assertEquals("https://relay.example.dev/", prefs.server)
        assertEquals(Prefs.CODEC_HEVC, store.map[Prefs.KEY_CODEC])
        assertTrue("the last used touch mode becomes the default", prefs.directTouch)
        assertEquals(PointerSpeed.DEFAULT, prefs.pointerSpeed)
        assertTrue(prefs.tapToClick)
        assertEquals(Prefs.VERSION, store.map[Prefs.KEY_VERSION])
        // Runs once: a second instance writes nothing.
        val before = store.writes
        Prefs(store)
        assertEquals(before, store.writes)
    }

    @Test
    fun codecSpellingsNormalize() {
        for ((old, new) in listOf("hevc" to "hevc", "HEVC" to "hevc", "h265" to "hevc", "H.264" to "h264", "avc" to "h264", "auto" to "auto", "vp9" to "auto", "" to "auto")) {
            val store = MapStore(hashMapOf(Prefs.KEY_CODEC to old))
            assertEquals(old, new, Prefs(store).codec)
        }
    }

    @Test
    fun malformedValuesReadAsDefaults() {
        val store = MapStore(hashMapOf(
            Prefs.KEY_VERSION to Prefs.VERSION,
            Prefs.KEY_SERVER to "  ",
            Prefs.KEY_CODEC to "av1",
            Prefs.KEY_POINTER_SPEED to 42,
            Prefs.KEY_TAP_TO_CLICK to "yes", // wrong type
            Prefs.KEY_KEEP_SCREEN_ON to 0,
        ))
        val prefs = Prefs(store)
        assertEquals(ServerConfig.DEFAULT_SERVER, prefs.server)
        assertEquals(Prefs.CODEC_AUTO, prefs.codec)
        assertEquals(PointerSpeed.MAX, prefs.pointerSpeed)
        assertTrue(prefs.tapToClick)
        assertTrue(prefs.keepScreenOn)
    }

    @Test
    fun writesRoundTripAndClamp() {
        val store = MapStore()
        val prefs = Prefs(store)
        prefs.pointerSpeed = 0
        assertEquals(PointerSpeed.MIN, store.map[Prefs.KEY_POINTER_SPEED])
        prefs.codec = "nonsense"
        assertEquals(Prefs.CODEC_AUTO, prefs.codec)
        prefs.codec = Prefs.CODEC_H264
        assertEquals(Prefs.CODEC_H264, Prefs(store).codec)
        prefs.tapToClick = false; prefs.startFullscreen = false; prefs.keepScreenOn = false; prefs.directTouch = true
        val again = Prefs(store)
        assertFalse(again.tapToClick); assertFalse(again.startFullscreen); assertFalse(again.keepScreenOn); assertTrue(again.directTouch)
    }

    @Test
    fun defaultMonitorKeepsItsNameAndClears() {
        val store = MapStore()
        val prefs = Prefs(store)
        prefs.setDefaultMonitor("\\\\.\\DISPLAY2", "DELL U2720Q")
        assertEquals("\\\\.\\DISPLAY2", prefs.defaultMonitor)
        assertEquals("DELL U2720Q", prefs.defaultMonitorName)
        prefs.setDefaultMonitor("\\\\.\\DISPLAY3", "")
        assertEquals("\\\\.\\DISPLAY3", prefs.defaultMonitorName) // falls back to the selector
        prefs.setDefaultMonitor("", "")
        assertEquals("", prefs.defaultMonitor)
        assertNull(store.map[Prefs.KEY_DEFAULT_MONITOR_NAME])
    }

    @Test
    fun automaticRemoteClipboardDefaultsOnAndPersistsBothChoices() {
        val store = MapStore(hashMapOf(Prefs.KEY_VERSION to Prefs.VERSION))
        val prefs = Prefs(store)
        assertTrue(prefs.autoCopyRemoteClipboard)
        prefs.autoCopyRemoteClipboard = false
        assertEquals(false, store.map[Prefs.KEY_AUTO_COPY_REMOTE_CLIPBOARD])
        assertFalse(Prefs(store).autoCopyRemoteClipboard)
        prefs.autoCopyRemoteClipboard = true
        assertTrue(Prefs(store).autoCopyRemoteClipboard)
    }

    @Test
    fun malformedAutomaticRemoteClipboardPreferenceDefaultsOn() {
        val store = MapStore(hashMapOf(Prefs.KEY_VERSION to Prefs.VERSION, Prefs.KEY_AUTO_COPY_REMOTE_CLIPBOARD to "off"))
        assertTrue(Prefs(store).autoCopyRemoteClipboard)
    }
}
