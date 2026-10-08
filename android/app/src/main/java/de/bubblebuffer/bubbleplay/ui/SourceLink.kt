package de.bubblebuffer.bubbleplay.ui

import java.net.URI

object SourceLink {
    const val DEFAULT_URL = "https://github.com/BubbleBuffer/BubblePlay"

    fun normalize(value: String?): String {
        val uri = try { URI(value ?: return DEFAULT_URL) } catch (_: Exception) { return DEFAULT_URL }
        return if (uri.scheme in listOf("https", "http") && !uri.host.isNullOrEmpty() && uri.userInfo == null) uri.toString()
        else DEFAULT_URL
    }
}
