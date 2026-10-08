package de.bubblebuffer.bubbleplay.net

import okhttp3.HttpUrl
import okhttp3.HttpUrl.Companion.toHttpUrlOrNull

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

    fun mediaUrl(epoch: String): HttpUrl = base.newBuilder().encodedPath(if (isLab) "/lab-media-ws" else "/media-ws").query(null).apply {
        addQueryParameter("epoch", epoch)
        if (isLab) { addQueryParameter("run", labRun); addQueryParameter("compat", labCompat) }
    }.build()

    /** File transfer v1 socket for the signaling epoch (the relay pairs it with the host's). */
    fun fileUrl(epoch: String): HttpUrl = base.newBuilder().encodedPath(if (isLab) "/lab-file-ws" else "/file-ws").query(null).apply {
        addQueryParameter("epoch", epoch)
        if (isLab) { addQueryParameter("run", labRun); addQueryParameter("compat", labCompat) }
    }.build()

    companion object {
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
