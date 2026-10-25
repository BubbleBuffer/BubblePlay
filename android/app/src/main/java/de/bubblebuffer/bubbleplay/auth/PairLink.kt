package de.bubblebuffer.bubbleplay.auth

/**
 * A pairing request: the relay origin URL, the canonical PC ID and the normalised 10-character
 * pairing code (lane dk2-android contract; the core run implements the bodies).
 * Link form: bubbleplay://pair?relay=<url>&pc=<id>&code=<code> (decisions item 14).
 */
data class PairLink(val relay: String, val pcId: String, val code: String) {
    fun toUri(): String = throw NotImplementedError("PairLink.toUri: core run")

    companion object {
        /** Parses a bubbleplay://pair link; null when it is not a valid pairing link. */
        fun parse(uri: String): PairLink? = throw NotImplementedError("PairLink.parse: core run")
    }
}

object PairingCode {
    /** Uppercase, O->0, I/L->1, dashes and spaces dropped; null unless 10 Crockford base32 chars. */
    fun normalise(input: String): String? = throw NotImplementedError("PairingCode.normalise: core run")

    /** "XXXXX-XXXXX" for a normalised code. */
    fun display(code: String): String = throw NotImplementedError("PairingCode.display: core run")
}
