package de.bubblebuffer.bubbleplay.net

import okhttp3.HttpUrl
import okhttp3.HttpUrl.Companion.toHttpUrlOrNull
import org.json.JSONObject

/**
 * Where the relay lives. Production uses the gate-cookie protected /ws and
 * /media-ws. Lab mode (a Stream Lab run id) uses the loopback direct relay's
 * /lab-ws and /lab-media-ws with run + compat and needs no login.
 */
data class ServerConfig(
    val base: HttpUrl,
    val labRun: String? = null,
    val labCompat: String? = null,
) {
    val isLab: Boolean get() = labRun != null

    fun signalingUrl(): HttpUrl = base.newBuilder().encodedPath(if (isLab) "/lab-ws" else "/ws").query(null).apply {
        if (isLab) { addQueryParameter("run", labRun); addQueryParameter("compat", labCompat) }
    }.build()

    /** [pc] (device keys phase 1) follows the epoch when the relay named one; absent, the URL is unchanged. */
    fun mediaUrl(epoch: String, pc: String? = null): HttpUrl = base.newBuilder().encodedPath(if (isLab) "/lab-media-ws" else "/media-ws").query(null).apply {
        addQueryParameter("epoch", epoch)
        pc?.let { addQueryParameter("pc", it) }
        if (isLab) { addQueryParameter("run", labRun); addQueryParameter("compat", labCompat) }
    }.build()

    /** File transfer v1 socket for the signaling epoch (the relay pairs it with the host's). */
    fun fileUrl(epoch: String, pc: String? = null): HttpUrl = base.newBuilder().encodedPath(if (isLab) "/lab-file-ws" else "/file-ws").query(null).apply {
        addQueryParameter("epoch", epoch)
        pc?.let { addQueryParameter("pc", it) }
        if (isLab) { addQueryParameter("run", labRun); addQueryParameter("compat", labCompat) }
    }.build()

    companion object {
        /**
         * The signaling hello, unchanged from before device keys: /ws stays the same in phase 1.
         */
        fun signalingHello(): JSONObject = JSONObject().put("type", "hello").put("role", "client")

        /**
         * Sets the PC of a file URL built by [fileUrl]: any existing `pc` is replaced, and the new one
         * goes right after `epoch` (appended when there is no epoch). Other parameters, lab run and
         * compat included, keep their order. Values are decoded and re-encoded, which is exact for the
         * relay's values.
         */
        fun withPc(url: HttpUrl, pc: String?): HttpUrl {
            if (pc == null) return url
            val builder = url.newBuilder().query(null)
            var placed = false
            for (i in 0 until url.querySize) {
                val name = url.queryParameterName(i)
                if (name == "pc") continue
                builder.addQueryParameter(name, url.queryParameterValue(i))
                if (name == "epoch") { builder.addQueryParameter("pc", pc); placed = true }
            }
            if (!placed) builder.addQueryParameter("pc", pc)
            return builder.build()
        }

        // A fresh install must use the operator's relay; saved addresses stay in Prefs.
        const val DEFAULT_SERVER = ""
        const val GATE_COOKIE = "__Host-gate"
        const val AUTH_COOKIE = "__Host-bp-session"
        private val UUID = Regex("^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$", RegexOption.IGNORE_CASE)

        fun parseServer(text: String): HttpUrl? {
            val trimmed = text.trim()
            val url = (if (trimmed.contains("://")) trimmed else "https://$trimmed").toHttpUrlOrNull() ?: return null
            return url.newBuilder().encodedPath("/").query(null).fragment(null).build()
        }

        fun validLabRun(value: String?) = value != null && UUID.matches(value)

        /** True when a Cookie header carries both the approval-gate and the password session cookie. */
        fun hasSessionCookies(cookieHeader: String?): Boolean {
            if (cookieHeader.isNullOrEmpty()) return false
            val names = cookieHeader.split(';').mapNotNull { part ->
                val eq = part.indexOf('=')
                if (eq <= 0) null else part.substring(0, eq).trim() to part.substring(eq + 1).trim()
            }.filter { it.second.isNotEmpty() }.map { it.first }.toSet()
            return GATE_COOKIE in names && AUTH_COOKIE in names
        }
    }
}
