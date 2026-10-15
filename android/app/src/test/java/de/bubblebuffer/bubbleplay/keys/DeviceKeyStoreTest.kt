package de.bubblebuffer.bubbleplay.keys

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File

class DeviceKeyStoreTest {
    @get:Rule
    val folder = TemporaryFolder()

    @Test
    fun removeDeletesTheKeyFileAndItsBackupOnly() {
        val key = File(folder.root, DeviceKeyStore.FILE_NAME).apply { writeText("key") }
        val backup = File(folder.root, DeviceKeyStore.FILE_NAME + ".bak").apply { writeText("old key") }
        val other = File(folder.root, "notes.txt").apply { writeText("keep") }

        DeviceKeyStore(key).remove()

        assertFalse("key file", key.exists())
        assertFalse("backup", backup.exists())
        assertTrue("unrelated file", other.exists())
    }

    @Test
    fun removeWithNothingStoredDoesNotThrow() {
        DeviceKeyStore(File(folder.root, DeviceKeyStore.FILE_NAME)).remove()
    }
}
