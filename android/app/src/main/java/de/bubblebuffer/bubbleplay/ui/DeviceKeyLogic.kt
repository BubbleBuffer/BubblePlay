package de.bubblebuffer.bubbleplay.ui

import de.bubblebuffer.bubbleplay.R
import de.bubblebuffer.bubbleplay.keys.DeviceId
import de.bubblebuffer.bubbleplay.keys.DeviceKeyException
import de.bubblebuffer.bubbleplay.keys.DeviceKeyStore
import de.bubblebuffer.bubbleplay.keys.KeyFile
import de.bubblebuffer.bubbleplay.keys.KeyFileCodec
import java.io.ByteArrayOutputStream
import java.io.InputStream

/** What is stored as this phone's key, as read from [DeviceKeyStore]. */
internal sealed class StoredKey {
    object Missing : StoredKey()

    /** [text] is the portable file as stored, passphrase layer included; [file] is its parsed header. */
    class Present(val text: String, val file: KeyFile) : StoredKey()

    /** A file is there but cannot be read or parsed. [reason] is null for I/O and Keystore failures. */
    class Unreadable(val reason: DeviceKeyException.Reason?) : StoredKey()
}

/** The first screen of the app, from the stored key and whether this process holds it unlocked. */
internal enum class StartScreen { Setup, Unlock, PcList }

/** Non-visual device-key logic: what is stored, which screen starts, file names and error text. */
internal object DeviceKeyLogic {
    /** Reads the stored key. Does disk and Keystore work, so call it off the UI thread. */
    fun read(store: DeviceKeyStore): StoredKey = try {
        val text = store.loadPortable()
        if (text == null) StoredKey.Missing else StoredKey.Present(text, KeyFileCodec.parse(text))
    } catch (e: DeviceKeyException) {
        StoredKey.Unreadable(e.reason)
    } catch (e: Exception) {
        StoredKey.Unreadable(null)
    }

    /**
     * The app needs an unlocked key before it shows PCs. Without a key, or with one that cannot be
     * read, the setup card shows (an unreadable key is never silently replaced: it has its own text).
     */
    fun startScreen(stored: StoredKey, unlocked: Boolean): StartScreen = when (stored) {
        is StoredKey.Present -> if (unlocked) StartScreen.PcList else StartScreen.Unlock
        is StoredKey.Missing, is StoredKey.Unreadable -> StartScreen.Setup
    }

    /** Suggested name for an exported key file, e.g. `bubbleplay-B7QK-2M9X-H4PD-R8TN.bpkey`. */
    fun exportFileName(id: String): String {
        require(DeviceId.isCanonical(id)) { "not a canonical device ID" }
        return "bubbleplay-$id.bpkey"
    }

    /** The plain message for a refusal. Each [DeviceKeyException.Reason] has its own text; null means I/O. */
    fun reasonMessage(reason: DeviceKeyException.Reason?): Int = when (reason) {
        DeviceKeyException.Reason.MALFORMED -> R.string.dk_err_malformed
        DeviceKeyException.Reason.UNSUPPORTED_FORMAT -> R.string.dk_err_format
        DeviceKeyException.Reason.UNSUPPORTED_VERSION -> R.string.dk_err_version
        DeviceKeyException.Reason.UNSUPPORTED_TYPE -> R.string.dk_err_type
        DeviceKeyException.Reason.IDENTITY_MISMATCH -> R.string.dk_err_identity
        DeviceKeyException.Reason.AUTHENTICATION_FAILED -> R.string.dk_err_auth
        DeviceKeyException.Reason.CLOSED -> R.string.dk_err_closed
        null -> R.string.dk_err_io
    }

    /** The message for any failure from the key module or the file system. */
    fun messageFor(error: Throwable): Int = reasonMessage((error as? DeviceKeyException)?.reason)

    /**
     * Reads a whole stream, refusing anything longer than [max] bytes so that a picked file
     * cannot exhaust memory. Oversized input is refused as MALFORMED.
     */
    fun readBounded(input: InputStream, max: Int): ByteArray {
        val out = ByteArrayOutputStream()
        val chunk = ByteArray(8 * 1024)
        try {
            while (true) {
                val n = input.read(chunk)
                if (n < 0) break
                if (out.size() + n > max) {
                    throw DeviceKeyException(DeviceKeyException.Reason.MALFORMED, "key file is larger than $max bytes")
                }
                out.write(chunk, 0, n)
            }
            return out.toByteArray()
        } finally {
            chunk.fill(0)
        }
    }
}
