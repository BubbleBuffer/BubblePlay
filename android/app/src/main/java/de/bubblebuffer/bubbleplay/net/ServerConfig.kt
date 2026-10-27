package de.bubblebuffer.bubbleplay.net

import okhttp3.HttpUrl
import okhttp3.HttpUrl.Companion.toHttpUrlOrNull

/** Relay origin and the paired PC addressed on every socket. */
data class ServerConfig(val base: HttpUrl, val pc: String) {
    fun signalingUrl(): HttpUrl = url("/ws")
    fun mediaUrl(epoch: String): HttpUrl = url("/media-ws", epoch)
    fun fileUrl(epoch: String): HttpUrl = url("/file-ws", epoch)

    private fun url(path: String, epoch: String? = null): HttpUrl =
        base.newBuilder().encodedPath(path).query(null).fragment(null).apply {
            if (epoch != null) addQueryParameter("epoch", epoch)
            addQueryParameter("pc", pc)
        }.build()

    companion object {
        fun parseServer(text: String): HttpUrl? {
            val trimmed = text.trim()
            val url = (if (trimmed.contains("://")) trimmed else "https://$trimmed").toHttpUrlOrNull() ?: return null
            return url.newBuilder().encodedPath("/").query(null).fragment(null).build()
        }
    }
}
