package de.bubblebuffer.bubbleplay.flutter

/**
 * Keeps the decoder's surface in step with the Flutter texture's surface, the way
 * StreamActivity does with surfaceCreated / surfaceDestroyed: a lost surface detaches the
 * decoder (blocking, so the codec is released before the surface goes), a new one attaches it.
 * [S] is `Surface` in production; the logic never looks inside it.
 *
 * Main thread only.
 */
internal class SurfaceHandoff<S : Any>(
    private val sink: Sink<S>,
    private val resizeProducer: (width: Int, height: Int) -> Unit,
) {
    interface Sink<S> {
        fun attach(surface: S)
        fun detachBlocking()
    }

    private var current: S? = null
    private var attached: S? = null
    private var bound = false
    private var width = 0
    private var height = 0

    /** A session is there to take the surface. */
    fun bind() {
        bound = true
        attachCurrent()
    }

    /** The session is going away: let the codec go before anything else. */
    fun unbind() {
        bound = false
        detach()
    }

    /** The producer has a (new) surface. Safe to call again with the same one. */
    fun available(surface: S) {
        current = surface
        attachCurrent()
    }

    /** The producer is about to take its surface away. */
    fun cleanup() {
        current = null
        detach()
    }

    /** The stream size changed; the producer follows it. */
    fun resize(newWidth: Int, newHeight: Int) {
        if (newWidth <= 0 || newHeight <= 0) return
        if (newWidth == width && newHeight == height) return
        width = newWidth
        height = newHeight
        resizeProducer(newWidth, newHeight)
    }

    private fun attachCurrent() {
        val surface = current ?: return
        if (!bound || attached === surface) return
        if (attached != null) sink.detachBlocking()
        attached = surface
        sink.attach(surface)
    }

    private fun detach() {
        if (attached == null) return
        attached = null
        sink.detachBlocking()
    }
}
