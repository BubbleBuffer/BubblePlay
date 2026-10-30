package de.bubblebuffer.bubbleplay.keys

import de.bubblebuffer.bubbleplay.ui.DeviceKeyLogic
import de.bubblebuffer.bubbleplay.ui.StoredKey
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File
import java.io.FileNotFoundException

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

    @Test
    fun savePortableRefusesAFileInAnotherFormatAndStoresNothing() {
        val store = newStore()
        refusedWith(DeviceKeyException.Reason.UNSUPPORTED_FORMAT) { store.savePortable(foreignText()) }
        assertFalse("nothing stored", path.exists())
    }

    @Test
    fun aStoredFileInAnotherFormatIsNoKeyAndNewKeySetupReplacesIt() {
        val foreign = foreignText().toByteArray()
        writeEnvelope(foreign)
        val store = newStore()

        assertTrue(DeviceKeyLogic.read(store) is StoredKey.Missing)

        val created = KeyFileCodec.create("New phone", null).also { it.key.close() }
        store.savePortable(KeyFileCodec.serialize(created.file))

        val stored = DeviceKeyLogic.read(store)
        assertTrue(stored is StoredKey.Present)
        assertEquals(created.file.id, (stored as StoredKey.Present).file.id)
        assertEquals(listOf(path.name), folder.root.list()!!.toList())
    }

    @Test
    fun aCorruptStoredFileIsStillUnreadableNotMissing() {
        writeEnvelope("not json".toByteArray())
        assertTrue(DeviceKeyLogic.read(newStore()) is StoredKey.Unreadable)
    }

    // ---- fixtures: the Keystore is not available on the JVM, so the wrap is the identity and the
    // envelope is the file itself -----------------------------------------------------------------------------

    private val path get() = File(folder.root, DeviceKeyStore.FILE_NAME)

    /** A current-format key file with its format name changed. */
    private fun foreignText(): String =
        mutatedText(sharedKeyFiles().getJSONObject("plain").getJSONObject("file")) { put("format", "other-format") }

    private fun writeEnvelope(bytes: ByteArray) = DiskEnvelope(path).write(bytes)

    private fun newStore(): DeviceKeyStore = DeviceKeyStore(path, IdentityWrap, DiskEnvelope(path))

    private object IdentityWrap : EnvelopeWrap {
        override fun wrap(portableBytes: ByteArray): ByteArray = portableBytes.copyOf()
        override fun unwrap(envelope: ByteArray): ByteArray = envelope.copyOf()
    }

    /** A file-backed envelope. */
    private class DiskEnvelope(private val file: File) : EnvelopeFile {
        override fun read(): ByteArray {
            if (!file.exists()) throw FileNotFoundException(file.name)
            return file.readBytes()
        }

        override fun write(envelope: ByteArray) {
            file.writeBytes(envelope)
        }
    }
}
