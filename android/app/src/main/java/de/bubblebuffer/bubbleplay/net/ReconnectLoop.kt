package de.bubblebuffer.bubbleplay.net

import de.bubblebuffer.bubbleplay.protocol.V4

/**
 * One reconnect loop (signaling or media): exponential backoff, held while the
 * app is in the background. A retry owed while backgrounded is attempted once,
 * immediately, when the app returns; returning also resets the backoff.
 */
internal class ReconnectLoop {
    var backoffMs = 0L
        private set
    var foreground = true
        private set
    /** A retry is owed but held until the app returns to the foreground. */
    var held = false
        private set
    /** A retry timer is posted. */
    var scheduled = false
        private set

    /** A retry is needed. Returns the delay to post, or null when it is held (background). */
    fun schedule(): Long? {
        if (!foreground) { held = true; scheduled = false; return null }
        backoffMs = V4.nextBackoffMs(backoffMs)
        scheduled = true
        return backoffMs
    }

    /** The posted timer fired. True: attempt now. False: the app went to the background meanwhile. */
    fun fire(): Boolean {
        scheduled = false
        if (!foreground) { held = true; return false }
        return true
    }

    /** The connection came up. */
    fun succeeded() { backoffMs = 0; held = false; scheduled = false }

    /** Next retry starts at the shortest delay (e.g. the host left, not a failure). */
    fun resetBackoff() { backoffMs = 0 }

    /** Forget any owed or posted retry (the caller cancels its timer). */
    fun cancel() { held = false; scheduled = false }

    fun background() { foreground = false }

    /**
     * Back in the foreground: reset the backoff. True when a retry was owed or
     * waiting on a timer; the caller cancels that timer and attempts now.
     */
    fun foreground(): Boolean {
        foreground = true
        backoffMs = 0
        val owed = held || scheduled
        held = false; scheduled = false
        return owed
    }
}
