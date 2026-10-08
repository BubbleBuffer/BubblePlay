package de.bubblebuffer.bubbleplay.ui

import android.content.Context
import android.content.SharedPreferences
import android.webkit.CookieManager
import de.bubblebuffer.bubbleplay.input.PointerSpeed
import de.bubblebuffer.bubbleplay.net.ServerConfig

/** The key-value store behind [Prefs]; SharedPreferences on the phone, a map in the JVM tests. */
interface PrefStore {
    fun getString(key: String): String?
    fun getBoolean(key: String): Boolean?
    fun getInt(key: String): Int?
    fun edit(block: Editor.() -> Unit)

    interface Editor {
        fun putString(key: String, value: String)
        fun putBoolean(key: String, value: Boolean)
        fun putInt(key: String, value: Int)
        fun remove(key: String)
    }
}

private class SharedPrefStore(private val prefs: SharedPreferences) : PrefStore {
    // A key written with another type reads as absent (the default) instead of throwing.
    override fun getString(key: String) = prefs.all[key] as? String
    override fun getBoolean(key: String) = prefs.all[key] as? Boolean
    override fun getInt(key: String) = prefs.all[key] as? Int

    override fun edit(block: PrefStore.Editor.() -> Unit) {
        val editor = prefs.edit()
        object : PrefStore.Editor {
            override fun putString(key: String, value: String) { editor.putString(key, value) }
            override fun putBoolean(key: String, value: Boolean) { editor.putBoolean(key, value) }
            override fun putInt(key: String, value: Int) { editor.putInt(key, value) }
            override fun remove(key: String) { editor.remove(key) }
        }.block()
        editor.apply()
    }
}

/**
 * Persisted settings: the connection (server URL, optional Stream Lab run)
 * and the settings view (touch, display, video, clipboard). Reads fall back to the
 * defaults for missing or malformed values; [migrate] runs once per instance.
 */
class Prefs(private val store: PrefStore) {
    constructor(context: Context) : this(SharedPrefStore(context.getSharedPreferences("bubbleplay", Context.MODE_PRIVATE)))

    init { migrate() }

    var server: String
        get() = store.getString(KEY_SERVER)?.takeIf { it.isNotBlank() } ?: ServerConfig.DEFAULT_SERVER
        set(value) = store.edit { putString(KEY_SERVER, value) }

    /** Stream Lab run id (UUID) for the local direct relay; blank for production. */
    var labRun: String
        get() = store.getString(KEY_LAB_RUN) ?: ""
        set(value) = store.edit { putString(KEY_LAB_RUN, value) }

    /** [CODEC_AUTO], [CODEC_HEVC] or [CODEC_H264]. */
    var codec: String
        get() = store.getString(KEY_CODEC)?.takeIf { it in CODECS } ?: CODEC_AUTO
        set(value) = store.edit { putString(KEY_CODEC, if (value in CODECS) value else CODEC_AUTO) }

    /** Default touch mode for new sessions: direct touch, else trackpad. */
    var directTouch: Boolean
        get() = store.getBoolean(KEY_DIRECT_TOUCH) ?: false
        set(value) = store.edit { putBoolean(KEY_DIRECT_TOUCH, value) }

    /** Trackpad pointer speed level, [PointerSpeed.MIN]..[PointerSpeed.MAX]. */
    var pointerSpeed: Int
        get() = PointerSpeed.clamp(store.getInt(KEY_POINTER_SPEED) ?: PointerSpeed.DEFAULT)
        set(value) = store.edit { putInt(KEY_POINTER_SPEED, PointerSpeed.clamp(value)) }

    /** Trackpad: a one-finger tap clicks. */
    var tapToClick: Boolean
        get() = store.getBoolean(KEY_TAP_TO_CLICK) ?: true
        set(value) = store.edit { putBoolean(KEY_TAP_TO_CLICK, value) }

    var startFullscreen: Boolean
        get() = store.getBoolean(KEY_START_FULLSCREEN) ?: true
        set(value) = store.edit { putBoolean(KEY_START_FULLSCREEN, value) }

    var keepScreenOn: Boolean
        get() = store.getBoolean(KEY_KEEP_SCREEN_ON) ?: true
        set(value) = store.edit { putBoolean(KEY_KEEP_SCREEN_ON, value) }

    var autoCopyRemoteClipboard: Boolean
        get() = store.getBoolean(KEY_AUTO_COPY_REMOTE_CLIPBOARD) ?: true
        set(value) = store.edit { putBoolean(KEY_AUTO_COPY_REMOTE_CLIPBOARD, value) }

    /** The host monitor selector requested when a session starts; blank = the host's choice. */
    val defaultMonitor: String get() = store.getString(KEY_DEFAULT_MONITOR) ?: ""

    /** The name shown for [defaultMonitor] while no host monitor list is at hand. */
    val defaultMonitorName: String get() = store.getString(KEY_DEFAULT_MONITOR_NAME)?.ifBlank { null } ?: defaultMonitor

    fun setDefaultMonitor(selector: String, name: String) = store.edit {
        if (selector.isBlank()) { remove(KEY_DEFAULT_MONITOR); remove(KEY_DEFAULT_MONITOR_NAME) }
        else { putString(KEY_DEFAULT_MONITOR, selector); putString(KEY_DEFAULT_MONITOR_NAME, name) }
    }

    fun serverConfig(): ServerConfig? {
        val base = ServerConfig.parseServer(server) ?: return null
        val run = labRun.trim()
        return if (ServerConfig.validLabRun(run)) ServerConfig(base, run.lowercase()) else ServerConfig(base)
    }

    /**
     * Version 1 (unversioned) stored only server, labRun, codec and the last
     * used touch mode. Version 2 normalizes the codec (older spellings and
     * case) and keeps the last used touch mode as the default touch mode;
     * every new setting starts at its default.
     */
    private fun migrate() {
        val version = store.getInt(KEY_VERSION) ?: 1
        if (version >= VERSION) return
        store.edit {
            val codec = store.getString(KEY_CODEC)
            if (codec != null) putString(KEY_CODEC, normalizeCodec(codec))
            putInt(KEY_VERSION, VERSION)
        }
    }

    companion object {
        const val VERSION = 2
        const val CODEC_AUTO = "auto"
        const val CODEC_HEVC = "hevc"
        const val CODEC_H264 = "h264"
        private val CODECS = setOf(CODEC_AUTO, CODEC_HEVC, CODEC_H264)

        const val KEY_VERSION = "prefsVersion"
        const val KEY_SERVER = "server"
        const val KEY_LAB_RUN = "labRun"
        const val KEY_CODEC = "codec"
        const val KEY_DIRECT_TOUCH = "directTouch"
        const val KEY_POINTER_SPEED = "pointerSpeed"
        const val KEY_TAP_TO_CLICK = "tapToClick"
        const val KEY_START_FULLSCREEN = "startFullscreen"
        const val KEY_KEEP_SCREEN_ON = "keepScreenOn"
        const val KEY_AUTO_COPY_REMOTE_CLIPBOARD = "autoCopyRemoteClipboard"
        const val KEY_DEFAULT_MONITOR = "defaultMonitor"
        const val KEY_DEFAULT_MONITOR_NAME = "defaultMonitorName"

        fun normalizeCodec(value: String): String = when (value.trim().lowercase().replace(".", "").replace("-", "")) {
            "hevc", "h265" -> CODEC_HEVC
            "h264", "avc" -> CODEC_H264
            else -> CODEC_AUTO
        }

        fun cookieHeader(config: ServerConfig): String? = CookieManager.getInstance().getCookie(config.base.toString())

        /**
         * Sign out of this server: expires the gate and session cookies the
         * relay set (both are `__Host-` cookies: Path=/, Secure, no Domain), so
         * the next connection goes through sign-in again.
         */
        fun signOut(config: ServerConfig, done: () -> Unit) {
            val cookies = CookieManager.getInstance()
            val url = config.base.toString()
            val names = listOf(ServerConfig.GATE_COOKIE, ServerConfig.AUTH_COOKIE)
            var left = names.size
            for (name in names) {
                cookies.setCookie(url, "$name=; Max-Age=0; Expires=Thu, 01 Jan 1970 00:00:00 GMT; Path=/; Secure") {
                    if (--left == 0) { cookies.flush(); done() }
                }
            }
        }

        fun signedIn(config: ServerConfig): Boolean = !config.isLab && ServerConfig.hasSessionCookies(cookieHeader(config))
    }
}
