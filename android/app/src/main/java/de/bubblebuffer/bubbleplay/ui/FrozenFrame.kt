package de.bubblebuffer.bubbleplay.ui

/**
 * The last video frame, kept while the app is in the background and shown
 * (dimmed, "Reconnecting…") over the stream on return until the first live
 * frame is presented. [T] is the image type (a Bitmap in the app).
 *
 * Capture starts when the activity pauses and finishes asynchronously; the
 * video surface is usually destroyed in between. A failed capture leaves no
 * snapshot, so nothing is shown (plain black, as before). The snapshot lives
 * with the activity: across the 30 s background session stop, dropped on an
 * explicit disconnect or sign-out.
 */
internal class FrozenFrame<T : Any> {
    var snapshot: T? = null
        private set
    /** The video surface is gone (or a new session has not presented yet): no live frame on screen. */
    private var videoLost = false
    private var capturing = 0

    /** Visible when there is a snapshot and no live frame since the video was lost. */
    val showing: Boolean get() = snapshot != null && videoLost

    /**
     * The activity is pausing. Returns a capture token, or null when the
     * snapshot is still on screen (no live frame since: the surface holds
     * nothing newer), so it is kept. Otherwise the old snapshot is stale.
     */
    fun captureStarted(): Int? {
        if (showing) return null
        snapshot = null
        return ++capturing
    }

    /** The capture [token] finished with [image] (null: failed). Returns [showing]. */
    fun captured(token: Int, image: T?): Boolean {
        if (token == capturing) snapshot = image
        return showing
    }

    /** The video surface was destroyed. Returns [showing]. */
    fun surfaceLost(): Boolean {
        videoLost = true
        return showing
    }

    /** A live frame was presented. True when the overlay was showing and should now fade out. */
    fun livePresented(): Boolean {
        val wasShowing = showing
        videoLost = false
        return wasShowing
    }

    /** Explicit disconnect or another host: forget the frame. */
    fun clear() {
        snapshot = null
        capturing++
        videoLost = false
    }
}
