package de.bubblebuffer.bubbleplay.keys

import de.bubblebuffer.bubbleplay.log.BpLog

/** A key or key file was refused. [reason] is for the caller's message; [message] is for logs. */
class DeviceKeyException(val reason: Reason, message: String) : Exception(message) {
    enum class Reason {
        /** Bad JSON, unknown or missing fields, or a field that breaks its format or size. */
        MALFORMED,
        UNSUPPORTED_FORMAT,
        UNSUPPORTED_VERSION,
        UNSUPPORTED_TYPE,
        /** The seed, the public key and the device ID do not belong together. */
        IDENTITY_MISMATCH,
        /** Wrong passphrase, or the file was altered (AES-GCM tag did not verify). */
        AUTHENTICATION_FAILED,
        /** The key was closed before the operation. */
        CLOSED,
    }
}

internal fun refuse(reason: DeviceKeyException.Reason, message: String): Nothing {
    BpLog.w("DeviceKey", "key or key file refused", null, mapOf("refusal" to reason.name, "detail" to message))
    throw DeviceKeyException(reason, message)
}

internal fun malformed(message: String): Nothing = refuse(DeviceKeyException.Reason.MALFORMED, message)
