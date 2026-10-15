package de.bubblebuffer.bubbleplay.ui

import de.bubblebuffer.bubbleplay.keys.DeviceKeyException
import de.bubblebuffer.bubbleplay.keys.KeyFileCodec
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.ByteArrayInputStream

class DeviceKeyLogicTest {
    private val present = StoredKey.Present("", KeyFileCodec.create("Test phone", null).file)

    /** SharedPreferences stand-in, as in PrefsTest. */
    private class MapStore(val map: HashMap<String, Any> = HashMap()) : PrefStore {
        override fun getString(key: String) = map[key] as? String
        override fun getBoolean(key: String) = map[key] as? Boolean
        override fun getInt(key: String) = map[key] as? Int
        override fun edit(block: PrefStore.Editor.() -> Unit) {
            object : PrefStore.Editor {
                override fun putString(key: String, value: String) { map[key] = value }
                override fun putBoolean(key: String, value: Boolean) { map[key] = value }
                override fun putInt(key: String, value: Int) { map[key] = value }
                override fun remove(key: String) { map.remove(key) }
            }.block()
        }
    }

    @Test
    fun offerShowsOnlyWithoutAKeyAndUntilNotNow() {
        assertTrue(DeviceKeyLogic.offerVisible(StoredKey.Missing, dismissed = false))
        assertFalse("Not now hides it for good", DeviceKeyLogic.offerVisible(StoredKey.Missing, dismissed = true))
        assertFalse("a key is stored", DeviceKeyLogic.offerVisible(present, dismissed = false))
        assertFalse("an unreadable key is not 'no key': Create would replace it", DeviceKeyLogic.offerVisible(StoredKey.Unreadable(null), dismissed = false))
    }

    @Test
    fun notNowIsStoredInPrefsAndDefaultsToShowing() {
        val store = MapStore()
        val prefs = Prefs(store)
        assertFalse(prefs.deviceKeyOfferDismissed)
        prefs.deviceKeyOfferDismissed = true

        assertTrue(Prefs(store).deviceKeyOfferDismissed)
        assertFalse(DeviceKeyLogic.offerVisible(StoredKey.Missing, Prefs(store).deviceKeyOfferDismissed))
    }

    @Test
    fun exportFileNameCarriesTheCanonicalId() {
        assertEquals(
            "bubbleplay-B7QK-2M9X-H4PD-R8TN.bpkey",
            DeviceKeyLogic.exportFileName("B7QK-2M9X-H4PD-R8TN"),
        )
    }

    @Test
    fun exportFileNameRefusesAnythingButACanonicalId() {
        assertThrows(IllegalArgumentException::class.java) { DeviceKeyLogic.exportFileName("b7qk-2m9x") }
        assertThrows(IllegalArgumentException::class.java) { DeviceKeyLogic.exportFileName("B7QK2M9XH4PDR8TN") }
    }

    @Test
    fun everyReasonHasItsOwnMessage() {
        val reasons: List<DeviceKeyException.Reason?> = DeviceKeyException.Reason.values().toList() + null
        val messages = reasons.map { DeviceKeyLogic.reasonMessage(it) }
        assertEquals(reasons.size, messages.toSet().size)
    }

    @Test
    fun errorsMapToTheirReasonAndOtherFailuresToIo() {
        assertEquals(
            DeviceKeyLogic.reasonMessage(DeviceKeyException.Reason.AUTHENTICATION_FAILED),
            DeviceKeyLogic.messageFor(DeviceKeyException(DeviceKeyException.Reason.AUTHENTICATION_FAILED, "wrong")),
        )
        assertEquals(
            DeviceKeyLogic.reasonMessage(null),
            DeviceKeyLogic.messageFor(IllegalStateException("disk full")),
        )
    }

    @Test
    fun readBoundedKeepsSmallInputAndRefusesLargeInput() {
        val bytes = byteArrayOf(1, 2, 3, 4, 5)
        assertArrayEquals(bytes, DeviceKeyLogic.readBounded(ByteArrayInputStream(bytes), 5))

        val refused = assertThrows(DeviceKeyException::class.java) {
            DeviceKeyLogic.readBounded(ByteArrayInputStream(ByteArray(6)), 5)
        }
        assertEquals(DeviceKeyException.Reason.MALFORMED, refused.reason)
    }
}
