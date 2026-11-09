package de.bubblebuffer.bubbleplay.net

/**
 * What a session does while its app is in the background (phone app only): media stops at once and
 * signaling stays up for a grace period; back within it, media reopens; after it, the session ends.
 * Session thread only. [timer] runs a task once after a delay and returns its cancel function.
 */
internal class BackgroundGrace(
    private val graceMs: Long,
    private val clock: () -> Long,
    private val timer: (delayMs: Long, task: () -> Unit) -> (() -> Unit),
    private val host: Host,
) {
    interface Host {
        /** Close the media socket, decoder and audio, and release what they hold. */
        fun stopMedia()

        /** Reopen media over the live signaling session. [backgroundMs]: how long the app was away. */
        fun resumeMedia(backgroundMs: Long)

        /** The grace ran out: end the session as the app's own Disconnect does. */
        fun expired(backgroundMs: Long)
    }

    private var since = -1L
    private var cancelTimer: (() -> Unit)? = null

    /** True from the app leaving the foreground until it returns or the grace ends. */
    val inBackground get() = since >= 0

    fun background() {
        if (inBackground) return
        since = clock()
        host.stopMedia()
        cancelTimer = timer(graceMs) { expire() }
    }

    /** The app returned. False when the grace had already run out: the session is ended, nothing resumes. */
    fun foreground(): Boolean {
        if (!inBackground) return true
        val away = clock() - since
        // The timer runs on uptime, which stops in deep sleep: the clock decides, not the timer.
        if (away >= graceMs) { expire(); return false }
        clear()
        host.resumeMedia(away)
        return true
    }

    /** The session ended some other way (Disconnect, error): nothing is left to fire. */
    fun cancel() = clear()

    private fun expire() {
        if (!inBackground) return
        val away = clock() - since
        clear()
        host.expired(away)
    }

    private fun clear() {
        cancelTimer?.invoke()
        cancelTimer = null
        since = -1
    }

    companion object {
        const val GRACE_MS = 5 * 60_000L
    }
}
