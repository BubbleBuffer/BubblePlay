package de.bubblebuffer.bubbleplay.keys

import android.content.Context
import android.util.AtomicFile
import android.util.Log
import java.io.File
import java.io.FileNotFoundException
import java.io.IOException
import java.nio.ByteBuffer
import java.nio.charset.CodingErrorAction
import java.security.MessageDigest

/** Wraps and unwraps the stored bytes. Production is [KeystoreKeyWrap]; tests substitute a fake. */
internal interface EnvelopeWrap {
    fun wrap(portableBytes: ByteArray): ByteArray
    fun unwrap(envelope: ByteArray): ByteArray
}

/** The stored envelope on disk. Production is [AtomicEnvelopeFile]; tests substitute a fake that can fail. */
internal interface EnvelopeFile {
    /** Throws [FileNotFoundException] when there is no file. */
    fun read(): ByteArray

    /** Replaces the file. A failure leaves the previous bytes in place. */
    fun write(envelope: ByteArray)
}

internal class AtomicEnvelopeFile(path: File) : EnvelopeFile {
    private val file by lazy { AtomicFile(path) }

    override fun read(): ByteArray = file.openRead().use { it.readBytes() }

    override fun write(envelope: ByteArray) {
        val output = file.startWrite()
        try {
            output.write(envelope)
            file.finishWrite(output)
        } catch (failure: Throwable) {
            file.failWrite(output)
            throw failure
        }
    }
}

/** App-private, Keystore-wrapped storage; never writes portable JSON to disk.
 *
 * Load/export returns the original portable text, including its passphrase layer.
 * Unlocking and the lifetime of an unlocked key belong to the caller. All methods
 * perform disk/crypto work and should be called off the UI thread. Android-only:
 * the actual Keystore and AtomicFile path require device/instrumentation testing.
 */
class DeviceKeyStore internal constructor(
    private val path: File,
    private val wrapper: EnvelopeWrap = KeystoreKeyWrap(),
    private val envelopes: EnvelopeFile = AtomicEnvelopeFile(path),
    private val report: (String) -> Unit = { Log.w(TAG, it) },
) {
    constructor(context: Context) : this(File(context.applicationContext.filesDir, FILE_NAME))

    // AtomicFile's own backup, and the copy of a legacy envelope kept during migration. Both are
    // created on first use, so remove() runs on the JVM without Android or Keystore.
    private val atomicBackup by lazy { File(path.path + BACKUP_SUFFIX) }
    private val legacyBackup by lazy { File(path.path + LEGACY_BACKUP_SUFFIX) }

    /** Null only when no local file exists; corruption and missing keys propagate. */
    fun loadPortable(): String? = synchronized(fileLock) {
        val envelope = try {
            envelopes.read()
        } catch (missing: FileNotFoundException) {
            // Do not mistake an existing but inaccessible file for a new install.
            if (path.exists() || atomicBackup.exists() || legacyBackup.exists()) throw missing
            return@synchronized null
        }
        decode(envelope)
    }

    /** Also serves import. Only the shared format is stored: a legacy file is converted by the caller first. */
    fun savePortable(text: String) = synchronized(fileLock) {
        require(!KeyFileCodec.parse(text).isLegacyFormat) { "convert a legacy key file before storing it" }
        writeEnvelope(text.toByteArray(Charsets.UTF_8))
    }

    /**
     * Converts the stored legacy file to the shared format. [passphrase] unlocks a protected file and
     * is null for a plain one. Returns true once the stored file is converted and reads back as the
     * same key. On any failure the old envelope stays (it is put back if it was already replaced),
     * one line without secrets is logged, and the caller carries on with the old file.
     */
    fun migrateLegacy(passphrase: String?): Boolean = synchronized(fileLock) {
        val old = try {
            envelopes.read()
        } catch (missing: FileNotFoundException) {
            return@synchronized false
        } catch (failure: Exception) {
            report("device key file could not be read for conversion")
            return@synchronized false
        }
        var replaced = false
        return@synchronized try {
            val file = KeyFileCodec.parse(decode(old))
            if (!file.isLegacyFormat) {
                dropBackupOfSameKey(file)
                return@synchronized false
            }
            val converted = KeyFileCodec.convertLegacy(file, passphrase).toByteArray(Charsets.UTF_8)
            legacyBackup.writeBytes(old)
            replaced = true
            writeEnvelope(converted)
            val stored = loadPortable() ?: throw IOException("converted key file is missing")
            val reread = KeyFileCodec.parse(stored)
            if (reread.isLegacyFormat || !sameKey(reread, file)) throw IOException("converted key file names another key")
            legacyBackup.delete()
            true
        } catch (failure: Exception) {
            report("device key conversion failed; keeping the old key file")
            if (replaced) restore(old)
            false
        }
    }

    /** Deletes the stored key and its backups. Throws if any of those files survives. */
    fun remove() = synchronized(fileLock) {
        val stuck = listOf(path, atomicBackup, legacyBackup).filter { it.exists() && !it.delete() }
        if (stuck.isNotEmpty()) throw IOException("could not delete the device key file")
    }

    private fun decode(envelope: ByteArray): String {
        val portableBytes = wrapper.unwrap(envelope)
        try {
            val text = Charsets.UTF_8.newDecoder()
                .onMalformedInput(CodingErrorAction.REPORT)
                .onUnmappableCharacter(CodingErrorAction.REPORT)
                .decode(ByteBuffer.wrap(portableBytes)).toString()
            KeyFileCodec.parse(text)
            return text
        } finally {
            portableBytes.fill(0)
        }
    }

    private fun writeEnvelope(portableBytes: ByteArray) {
        val envelope = try {
            wrapper.wrap(portableBytes)
        } finally {
            portableBytes.fill(0)
        }
        envelopes.write(envelope)
    }

    /** Puts the old envelope back. The backup goes only once that has worked; otherwise it stays on disk. */
    private fun restore(old: ByteArray) {
        try {
            envelopes.write(old)
            legacyBackup.delete()
        } catch (failure: Exception) {
            report("could not restore the old device key file; its backup is kept")
        }
    }

    /** A backup left by an interrupted run goes only when the stored key is the one it holds. */
    private fun dropBackupOfSameKey(file: KeyFile) {
        if (!legacyBackup.exists()) return
        try {
            if (sameKey(KeyFileCodec.parse(decode(legacyBackup.readBytes())), file)) legacyBackup.delete()
        } catch (failure: Exception) {
            report("kept a backup of the old device key file")
        }
    }

    private fun sameKey(a: KeyFile, b: KeyFile): Boolean =
        a.id == b.id && MessageDigest.isEqual(a.publicKey, b.publicKey)

    companion object {
        internal const val FILE_NAME = "device-key.bpkey"
        private const val TAG = "DeviceKeyStore"
        private const val BACKUP_SUFFIX = ".bak"
        private const val LEGACY_BACKUP_SUFFIX = ".legacy"

        // AtomicFile does not provide its own synchronization. Cover all instances.
        private val fileLock = Any()
    }
}
