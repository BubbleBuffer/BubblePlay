package de.bubblebuffer.bubbleplay.net

import org.json.JSONObject

/** Additive input-status fields. Receipt-only messages leave the warning intact. */
data class InputStatusUpdate(val hasReason: Boolean, val reason: String?, val route: String?) {
    companion object {
        fun parse(msg: JSONObject): InputStatusUpdate? {
            if (msg.opt("protocol") != 1) return null
            val hasReason = msg.has("reason")
            val rawReason = msg.opt("reason")
            val reason = if (!hasReason || rawReason == JSONObject.NULL) null else
                (rawReason as? String)?.takeIf { it == "admin-required" || it == "admin-unavailable" } ?: return null
            val route = if (!msg.has("route")) null else
                (msg.opt("route") as? String)?.takeIf { it in setOf("local", "admin", "secure") } ?: return null
            return InputStatusUpdate(hasReason, reason, route)
        }
    }
}

/** Presentation only: this never changes input admission or secure-desktop policy. */
class InputStatusWarning {
    var reason: String? = null
        private set
    var route: String? = null
        private set

    val text: String? get() = when (reason) {
        "admin-required" -> "This window needs Admin mode — turn it on at the PC"
        "admin-unavailable" -> "Admin mode helper isn’t responding"
        else -> null
    }

    fun apply(update: InputStatusUpdate) {
        if (update.hasReason) reason = update.reason
        if (update.route != null) route = update.route
    }

    fun reset() { reason = null; route = null }
}
