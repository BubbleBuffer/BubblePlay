package de.bubblebuffer.bubbleplay.flutter

import de.bubblebuffer.bubbleplay.keys.DeviceKey
import de.bubblebuffer.bubbleplay.keys.KeyFileCodec

/**
 * The text of the key file that leaves the device. The stored copy lives under the Keystore wrap
 * without a passphrase layer of its own; with a [passphrase] the exported file is sealed under it
 * (fresh salt and nonce), without one it is the stored text as it is. [held] is the app's unlocked
 * key, which is not closed here.
 */
internal fun exportedKeyText(stored: String, held: DeviceKey?, passphrase: String?): String {
    if (passphrase.isNullOrEmpty()) return stored
    val file = KeyFileCodec.parse(stored)
    val own = held?.takeIf { it.id == file.id }
    val key = own ?: KeyFileCodec.unlock(file, null)
    try {
        return KeyFileCodec.serialize(KeyFileCodec.rewrap(file, key, passphrase))
    } finally {
        if (own == null) key.close()
    }
}
