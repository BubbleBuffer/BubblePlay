package de.bubblebuffer.bubbleplay.net

import org.json.JSONObject

/**
 * What the relay grants one signaling session (device keys phase 1): a PC value and a ticket for
 * the media and file sockets. Both are opaque strings to this client. Session memory only: the
 * ticket never goes into a URL, a log line or an error, and only [hello] writes it, as a JSON string.
 */
class ViewerGrant private constructor(val pc: String?, private val ticket: String?) {
    /**
     * The socket hello. Without a ticket it is the exact legacy message. With one, JSONObject
     * escapes the value, so quotes, backslashes and non-ASCII text round-trip unchanged.
     */
    fun hello(role: String): String =
        if (ticket == null) "{\"type\":\"hello\",\"role\":\"$role\"}"
        else JSONObject().put("type", "hello").put("role", role).put("ticket", ticket).toString()

    override fun toString() = "ViewerGrant(pc=$pc, ticket=${if (ticket == null) "none" else "<redacted>"})"

    companion object {
        val NONE = ViewerGrant(null, null)

        /** A nonempty value is kept as it is; an empty one counts as absent. */
        fun from(pc: String?, ticket: String?): ViewerGrant {
            val id = pc?.takeIf { it.isNotEmpty() }
            val token = ticket?.takeIf { it.isNotEmpty() }
            return if (id == null && token == null) NONE else ViewerGrant(id, token)
        }

        /** The grant a hello-ok names in its optional `pc` and `ticket` fields. */
        fun fromHelloOk(msg: JSONObject): ViewerGrant = from(stringField(msg, "pc"), stringField(msg, "ticket"))

        /** Only a JSON string counts: a missing field, JSON null, a number, an object or an empty string is absent. */
        fun stringField(msg: JSONObject, key: String): String? = (msg.opt(key) as? String)?.takeIf { it.isNotEmpty() }
    }
}

/** The grant of the current signaling session: every hello-ok replaces it, a close clears it. */
class ViewerGrantSlot {
    var current: ViewerGrant = ViewerGrant.NONE
        private set

    /** Replaces the grant from a hello-ok; absent fields leave nothing held. */
    fun helloOk(msg: JSONObject): ViewerGrant = ViewerGrant.fromHelloOk(msg).also { current = it }

    /** True when a grant was held and is now cleared. */
    fun clear(): Boolean {
        if (current === ViewerGrant.NONE) return false
        current = ViewerGrant.NONE
        return true
    }
}
