package de.bubblebuffer.bubbleplay.keys

import android.content.Context
import android.util.AtomicFile
import java.io.File
import java.io.FileNotFoundException
import java.io.IOException
import java.nio.ByteBuffer
import java.nio.charset.CodingErrorAction

/** App-private, Keystore-wrapped storage; never writes portable JSON to disk.
 *
 * Load/export returns the original portable text, including its passphrase layer.
 * Unlocking and the lifetime of an unlocked key belong to the caller. All methods
 * perform disk/crypto work and should be called off the UI thread. Android-only:
 * the actual Keystore and AtomicFile path require device/instrumentation testing.
 */
class DeviceKeyStore internal constructor(private val path: File) {
    constructor(context: Context) : this(File(context.applicationContext.filesDir, FILE_NAME))

    // Both are created on first use, so remove() runs on the JVM without Android or Keystore.
    private val file by lazy { AtomicFile(path) }
    private val backup by lazy { File(path.path + BACKUP_SUFFIX) }
    private val wrapper by lazy { KeystoreKeyWrap() }

    /** Null only when no local file exists; corruption and missing keys propagate. */
    fun loadPortable(): String? = synchronized(fileLock) {
        val envelope = try {
            file.openRead().use { it.readBytes() }
        } catch (missing: FileNotFoundException) {
            // Do not mistake an existing but inaccessible file for a new install.
            if (path.exists() || backup.exists()) throw missing
            return@synchronized null
        }
        val portableBytes = wrapper.unwrap(envelope)
        try {
            val text = Charsets.UTF_8.newDecoder()
                .onMalformedInput(CodingErrorAction.REPORT)
                .onUnmappableCharacter(CodingErrorAction.REPORT)
                .decode(ByteBuffer.wrap(portableBytes)).toString()
            KeyFileCodec.parse(text)
            text
        } finally {
            portableBytes.fill(0)
        }
    }

    /** Also serves import: validates the portable file without unlocking it. */
    fun savePortable(text: String) = synchronized(fileLock) {
        KeyFileCodec.parse(text)
        val portableBytes = text.toByteArray(Charsets.UTF_8)
        val envelope = try {
            wrapper.wrap(portableBytes)
        } finally {
            portableBytes.fill(0)
        }
        val output = file.startWrite()
        try {
            output.write(envelope)
            file.finishWrite(output)
        } catch (failure: Throwable) {
            file.failWrite(output)
            throw failure
        }
    }

    /** Deletes the stored key and AtomicFile's backup. Throws if either file survives. */
    fun remove() = synchronized(fileLock) {
        val stuck = listOf(path, backup).filter { it.exists() && !it.delete() }
        if (stuck.isNotEmpty()) throw IOException("could not delete the device key file")
    }

    companion object {
        internal const val FILE_NAME = "device-key.bpkey"
        private const val BACKUP_SUFFIX = ".bak"

        // AtomicFile does not provide its own synchronization. Cover all instances.
        private val fileLock = Any()
    }
}
