package de.bubblebuffer.bubbleplay.keys

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File
import java.io.FileNotFoundException
import java.io.IOException

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
    fun removeAlsoDeletesTheLegacyBackup() {
        val legacyBackup = File(folder.root, DeviceKeyStore.FILE_NAME + ".legacy").apply { writeText("old key") }
        DeviceKeyStore(File(folder.root, DeviceKeyStore.FILE_NAME)).remove()
        assertFalse("legacy backup", legacyBackup.exists())
    }

    @Test
    fun aLegacyPlainFileIsConvertedOnceAndReadsBackAsTheSameKey() {
        val before = legacyText()
        val legacy = KeyFileCodec.parse(before)
        writeEnvelope(before.toByteArray())
        val store = newStore()

        assertTrue(store.migrateLegacy(null))

        val after = KeyFileCodec.parse(store.loadPortable()!!)
        assertFalse(after.isLegacyFormat)
        assertEquals(legacy.id, after.id)
        assertArrayEquals(legacy.publicKey, after.publicKey)
        assertFalse("legacy backup", legacyBackup().exists())
        assertTrue("no failure reported", notes.isEmpty())

        // Converted already: nothing changes on a second run.
        val converted = path.readBytes()
        assertFalse(store.migrateLegacy(null))
        assertArrayEquals(converted, path.readBytes())
    }

    @Test
    fun aFailedWriteKeepsTheOldFileAndLeavesNoBackup() {
        val before = legacyText().toByteArray()
        writeEnvelope(before)
        var failOnce = true
        val store = newStore { bytes ->
            if (failOnce) {
                failOnce = false
                throw IOException("disk full")
            }
            bytes
        }

        assertFalse(store.migrateLegacy(null))

        assertArrayEquals(before, path.readBytes())
        assertFalse("legacy backup", legacyBackup().exists())
        assertEquals(1, notes.size)
    }

    @Test
    fun aFailedWriteThatCannotBeRolledBackKeepsTheBackup() {
        val before = legacyText().toByteArray()
        writeEnvelope(before)
        val store = newStore { throw IOException("disk full") }

        assertFalse(store.migrateLegacy(null))

        assertArrayEquals(before, path.readBytes())
        assertTrue("legacy backup kept", legacyBackup().exists())
        assertArrayEquals(before, legacyBackup().readBytes())
    }

    @Test
    fun aConvertedFileThatReadsBackAsAnotherKeyIsRolledBack() {
        val before = legacyText().toByteArray()
        writeEnvelope(before)
        val other = KeyFileCodec.create("Other", null).also { it.key.close() }
        val otherText = KeyFileCodec.serialize(other.file).toByteArray()
        // The first write stores another key's file; the rollback write stores what it is given.
        var firstWrite = true
        val store = newStore { bytes ->
            if (firstWrite) {
                firstWrite = false
                otherText
            } else {
                bytes
            }
        }

        assertFalse(store.migrateLegacy(null))

        assertArrayEquals(before, path.readBytes())
        assertFalse("legacy backup", legacyBackup().exists())
        assertEquals(1, notes.size)
    }

    @Test
    fun aStaleBackupIsDroppedOnceTheStoredKeyIsTheSameKey() {
        val before = legacyText().toByteArray()
        writeEnvelope(before)
        val store = newStore()
        assertTrue(store.migrateLegacy(null))

        legacyBackup().writeBytes(before)

        assertFalse(store.migrateLegacy(null))
        assertFalse("stale backup", legacyBackup().exists())
    }

    @Test
    fun savePortableStoresOnlyTheSharedFormat() {
        val store = newStore()
        illegalArgument { store.savePortable(legacyText()) }
        assertFalse("nothing stored", path.exists())
    }

    // ---- migration fixtures: the Keystore is not available on the JVM, so the wrap is the identity and the
    // envelope is the file itself -----------------------------------------------------------------------------

    private val path get() = File(folder.root, DeviceKeyStore.FILE_NAME)
    private fun legacyBackup() = File(path.path + ".legacy")
    private val notes = mutableListOf<String>()

    private fun legacyText(): String = legacyKeyFiles().getJSONObject("plain").getJSONObject("file").toString()

    private fun writeEnvelope(bytes: ByteArray) = DiskEnvelope(path).write(bytes)

    private fun newStore(storing: (ByteArray) -> ByteArray = { it }): DeviceKeyStore =
        DeviceKeyStore(path, IdentityWrap, DiskEnvelope(path, storing)) { notes += it }

    private object IdentityWrap : EnvelopeWrap {
        override fun wrap(portableBytes: ByteArray): ByteArray = portableBytes.copyOf()
        override fun unwrap(envelope: ByteArray): ByteArray = envelope.copyOf()
    }

    /** A file-backed envelope. [storing] decides what a write puts on disk; by default, what it is given. */
    private class DiskEnvelope(
        private val file: File,
        private val storing: (ByteArray) -> ByteArray = { it },
    ) : EnvelopeFile {
        override fun read(): ByteArray {
            if (!file.exists()) throw FileNotFoundException(file.name)
            return file.readBytes()
        }

        override fun write(envelope: ByteArray) {
            file.writeBytes(storing(envelope))
        }
    }
}
