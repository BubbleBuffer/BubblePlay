package de.bubblebuffer.bubbleplay.net

import org.json.JSONObject

/**
 * The PC is on a secure desktop (UAC prompt, lock screen). [kind] is "uac",
 * "lock" or "other"; [capabilities] is what the host's SYSTEM helper allows
 * this viewer there: "view", "deny" (cancel the UAC prompt) and "input".
 */
data class SecureDesktop(val kind: String, val capabilities: Set<String>) {
    companion object {
        private val KINDS = setOf("uac", "lock", "other")
        private val CAPABILITIES = setOf("view", "deny", "input")

        /** From a `desktop-state` message whose desktop is "secure". */
        fun parse(msg: JSONObject): SecureDesktop {
            val kind = msg.optString("secureKind").takeIf { it in KINDS } ?: "other"
            val array = msg.optJSONArray("secureCapabilities")
            val caps = if (array == null) emptySet() else
                (0 until minOf(array.length(), 8)).mapNotNull { array.optString(it).takeIf { c -> c in CAPABILITIES } }.toSet()
            return SecureDesktop(kind, caps)
        }
    }
}
