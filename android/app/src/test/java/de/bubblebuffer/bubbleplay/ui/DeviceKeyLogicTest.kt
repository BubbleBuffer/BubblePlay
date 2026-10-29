package de.bubblebuffer.bubbleplay.ui

import de.bubblebuffer.bubbleplay.keys.DeviceKeyException
import de.bubblebuffer.bubbleplay.keys.KeyFileCodec
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Test
import java.io.ByteArrayInputStream

class DeviceKeyLogicTest {
    private val present = StoredKey.Present("", KeyFileCodec.create("Test phone", null).file)

    @Test
    fun startScreenNeedsAnUnlockedKeyForThePcList() {
        assertEquals(StartScreen.Setup, DeviceKeyLogic.startScreen(StoredKey.Missing, unlocked = false))
        assertEquals(StartScreen.Setup, DeviceKeyLogic.startScreen(StoredKey.Unreadable(null), unlocked = false))
        assertEquals(StartScreen.Unlock, DeviceKeyLogic.startScreen(present, unlocked = false))
        assertEquals(StartScreen.PcList, DeviceKeyLogic.startScreen(present, unlocked = true))
    }

    @Test
    fun anUnreadableKeyNeverOpensThePcListEvenIfAnotherIsUnlocked() {
        assertEquals(StartScreen.Setup, DeviceKeyLogic.startScreen(StoredKey.Unreadable(DeviceKeyException.Reason.MALFORMED), unlocked = true))
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
