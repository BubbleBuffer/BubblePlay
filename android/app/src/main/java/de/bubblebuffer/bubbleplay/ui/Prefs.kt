package de.bubblebuffer.bubbleplay.ui

import android.content.Context
import android.webkit.CookieManager
import de.bubblebuffer.bubbleplay.net.ServerConfig

/** Small persisted settings: server URL, optional Stream Lab run, codec, touch mode. */
class Prefs(context: Context) {
    private val prefs = context.getSharedPreferences("bubbleplay", Context.MODE_PRIVATE)

    var server: String
        get() = prefs.getString("server", ServerConfig.DEFAULT_SERVER) ?: ServerConfig.DEFAULT_SERVER
        set(value) = prefs.edit().putString("server", value).apply()

    /** Stream Lab run id (UUID) for the local direct relay; blank for production. */
    var labRun: String
        get() = prefs.getString("labRun", "") ?: ""
        set(value) = prefs.edit().putString("labRun", value).apply()

    /** "auto", "hevc" or "h264". */
    var codec: String
        get() = prefs.getString("codec", "auto") ?: "auto"
        set(value) = prefs.edit().putString("codec", value).apply()

    var directTouch: Boolean
        get() = prefs.getBoolean("directTouch", false)
        set(value) = prefs.edit().putBoolean("directTouch", value).apply()

    fun serverConfig(): ServerConfig? {
        val base = ServerConfig.parseServer(server) ?: return null
        val run = labRun.trim()
        return if (ServerConfig.validLabRun(run)) ServerConfig(base, run.lowercase()) else ServerConfig(base)
    }

    companion object {
        fun cookieHeader(config: ServerConfig): String? = CookieManager.getInstance().getCookie(config.base.toString())
    }
}
