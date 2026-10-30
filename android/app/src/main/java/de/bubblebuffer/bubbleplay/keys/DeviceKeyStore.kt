package de.bubblebuffer.bubbleplay.keys

import android.content.Context
import android.util.AtomicFile
import java.io.File
import java.io.FileNotFoundException
import java.io.IOException
import java.nio.ByteBuffer
import java.nio.charset.CodingErrorAction

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
) {
    constructor(context: Context) : this(File(context.applicationContext.filesDir, FILE_NAME))

    // AtomicFile's own backup. Created on first use, so remove() runs on the JVM without Android or Keystore.
    private val atomicBackup by lazy { File(path.path + BACKUP_SUFFIX) }

    /** Null only when no local file exists; corruption and missing keys propagate. */
    fun loadPortable(): String? = synchronized(fileLock) {
        val envelope = try {
            envelopes.read()
        } catch (missing: FileNotFoundException) {
            // Do not mistake an existing but inaccessible file for a new install.
            if (path.exists() || atomicBackup.exists()) throw missing
            return@synchronized null
        }
        decode(envelope)
    }

    /** Also serves import. Replaces whatever is stored; text that is not a shared-format key file is refused. */
    fun savePortable(text: String) = synchronized(fileLock) {
        KeyFileCodec.parse(text)
        writeEnvelope(text.toByteArray(Charsets.UTF_8))
    }

    /** Deletes the stored key and its backups. Throws if any of those files survives. */
    fun remove() = synchronized(fileLock) {
        val stuck = listOf(path, atomicBackup).filter { it.exists() && !it.delete() }
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

    companion object {
        internal const val FILE_NAME = "device-key.bpkey"
        private const val BACKUP_SUFFIX = ".bak"

        // AtomicFile does not provide its own synchronization. Cover all instances.
        private val fileLock = Any()
    }
}
