package de.bubblebuffer.bubbleplay.net

/**
 * Which signaling socket may carry input. Free of Android and OkHttp types so
 * it is unit-testable; [Session] drives it from its own thread, while
 * [inputSocket] is read from any thread.
 *
 * Input goes out only on the socket that is current, has authenticated for
 * this signaling epoch, and has presented a first frame for that epoch (the
 * host gates input on the first-frame handshake). Every reconnect starts a new
 * generation, so callbacks and references from an older socket are ignored.
 */
class InputLink<S : Any> {
    private var generation = 0
    private var socket: S? = null
    private var authenticated = false
    private var ready = false
    private var epoch: String? = null

    /** The media socket is delivering (set by the session). */
    @get:Synchronized @set:Synchronized
    var mediaUp = false

    /** A new signaling socket is being opened; the previous one (if any) is forgotten. */
    @Synchronized
    fun connecting(socket: S): Int {
        generation++
        this.socket = socket
        authenticated = false; ready = false; epoch = null
        return generation
    }

    @Synchronized
    fun isCurrent(gen: Int): Boolean = gen == generation && socket != null

    /** Authentication completed on [gen]; false (ignored) for a stale socket. Input stays off until [presented]. */
    @Synchronized
    fun authenticated(gen: Int, mediaEpoch: String?): Boolean {
        if (!isCurrent(gen)) return false
        authenticated = true; ready = false; epoch = mediaEpoch
        return true
    }

    /** The first frame for this epoch was presented: input may flow. */
    @Synchronized
    fun presented(gen: Int = generation): Boolean {
        if (!isCurrent(gen) || !authenticated) return false
        ready = true
        return true
    }

    /** A desktop transition or a new handshake: hold input until the next [presented]. */
    @Synchronized
    fun awaitFrame() { ready = false }

    /**
     * The socket of [gen] closed or failed. True when it was the current one:
     * the caller releases held input and reconnects. A stale socket's loss is ignored.
     */
    @Synchronized
    fun lost(gen: Int): Boolean {
        if (!isCurrent(gen)) return false
        socket = null; authenticated = false; ready = false; epoch = null
        return true
    }

    /** Deliberate teardown (stop, peer-left): forget the socket; callbacks from it become stale. */
    @Synchronized
    fun reset() { generation++; socket = null; authenticated = false; ready = false; epoch = null }

    /** The socket input may be written to right now, or null. */
    @Synchronized
    fun inputSocket(): S? = if (authenticated && ready) socket else null

    @get:Synchronized
    val signalingUp: Boolean get() = socket != null && authenticated

    @get:Synchronized
    val currentEpoch: String? get() = if (authenticated) epoch else null

    /** Video still flows but input has no usable signaling socket. */
    @get:Synchronized
    val inputReconnecting: Boolean get() = mediaUp && !signalingUp
}

/**
 * Half-open detection for the signaling socket. The relay answers every ping
 * with a pong, so an outstanding ping that stays unanswered means the socket
 * is dead even though TCP writes still succeed. While the user is sending
 * input, a quiet socket is probed early instead of waiting for the regular
 * heartbeat.
 */
class SignalLiveness(
    private val heartbeatMs: Long,
    private val probeAfterMs: Long = 4_000,
    private val pongTimeoutMs: Long = 6_000,
) {
    enum class Action { NONE, PING, DEAD }

    private var lastInbound = 0L
    private var lastSend = 0L
    private var pingSentAt: Long? = null
    private var inputSince: Long? = null

    @Synchronized
    fun opened(now: Long) { lastInbound = now; lastSend = now; pingSentAt = null; inputSince = null }

    @Synchronized
    fun inbound(now: Long) { lastInbound = now; pingSentAt = null }

    @Synchronized
    fun sent(now: Long) { lastSend = now }

    /** Input went out at [now] (any thread). */
    @Synchronized
    fun input(now: Long) { if (inputSince == null) inputSince = now }

    @Synchronized
    fun check(now: Long): Action {
        val outstanding = pingSentAt
        if (outstanding != null) {
            return if (now - outstanding > pongTimeoutMs) Action.DEAD else Action.NONE
        }
        val probe = inputSince?.let { now - lastInbound >= probeAfterMs } ?: false
        inputSince = null
        if (probe || now - lastSend >= heartbeatMs) {
            pingSentAt = now
            lastSend = now
            return Action.PING
        }
        return Action.NONE
    }
}

/**
 * Whether an outstanding first-frame request still covers the current media
 * epoch. The host arms input per signaling session, so a request from an older
 * epoch never counts (the browser receiver compares the epoch the same way).
 */
fun firstFrameRequestReusable(requestId: String?, requestEpoch: String?, currentEpoch: String?, failed: Boolean): Boolean =
    requestId != null && !failed && requestEpoch == currentEpoch
