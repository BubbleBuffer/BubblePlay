package de.bubblebuffer.bubbleplay.log

import org.json.JSONObject
import java.util.concurrent.CopyOnWriteArrayList

/** What the owner of one authenticated connection supplies to [CtlPassThrough]. */
internal interface CtlLink {
    /** Sends one JSON message over the sealed ctl channel; false when it could not be queued. Never blocks. */
    fun send(json: String): Boolean

    /** Bytes waiting in the ctl send buffer. */
    val bufferedBytes: Long
}

/**
 * The session's door to the log/feedback upload of docs/architecture/client-log-upload.md, owned by one
 * `Session` (`Session.ctl`). It does two jobs:
 *
 *  1. For the Kotlin core it creates one [LogUploader] (stream `app-android-core`) per authenticated
 *     connection and closes it when the connection ends, so every reconnect gets a fresh one. It is a
 *     no-op when [BpLog] is not initialised.
 *  2. For the Dart process (stream `app-android`, feedback reports) it is a generic pass-through to the same
 *     ctl channel: [sendRaw] out, [Listener] in. The pigeon bridge attaches to this; the Dart queue gates
 *     itself (16 KiB buffer, 250 ms after input, one message per `minBatchIntervalMs`) using
 *     [bufferedBytes] and [sinceLastInputMs].
 *
 * Routing of incoming ctl messages: `logs-ack` of stream `app-android-core` goes to the Kotlin uploader only;
 * `logs-ack` of any other stream and `feedback-ack` go to the listeners only; `logs-state` (it carries the
 * cursors of every stream) goes to both.
 *
 * Threads: [Listener] callbacks run on the session thread and must not block; the other methods may be
 * called from any thread. Nothing here throws into the caller. A listener added while a connection is open
 * immediately gets `onOpened` and, if the host already sent one, the connection's `logs-state`.
 */
class CtlPassThrough internal constructor(
    private val inputClock: () -> Long,
    private val uploaderFactory: (LogUploadTransport, BpLog, String) -> LogUploader,
) {
    /** [inputClock] is the clock the owner stamps input with (the session's `now()`), in milliseconds. */
    constructor(inputClock: () -> Long) : this(inputClock, { transport, log, pcId -> LogUploader(transport, log, pcId) })

    interface Listener {
        /** The PC's caps arrived: [peerCaps] is the PC's full list (look for [CAP]). Messages follow. */
        fun onOpened(pcId: String, peerCaps: List<String>) {}

        /** A `logs-state`, `logs-ack` (stream other than `app-android-core`) or `feedback-ack` message. */
        fun onMessage(message: JSONObject) {}

        /** The connection ended (or was replaced); [sendRaw] refuses until the next one. */
        fun onClosed() {}
    }

    private class Connection(val pcId: String, val link: CtlLink, val uploader: LogUploader?, val transport: ListenerTransport) {
        @Volatile var peerCaps: List<String>? = null
        @Volatile var state: JSONObject? = null
    }

    private val listeners = CopyOnWriteArrayList<Listener>()
    @Volatile private var connection: Connection? = null
    @Volatile private var lastInputAt = -1L

    fun addListener(listener: Listener) {
        if (!listeners.addIfAbsent(listener)) return
        val current = connection ?: return
        val caps = current.peerCaps ?: return
        guard { listener.onOpened(current.pcId, caps) }
        current.state?.let { state -> guard { listener.onMessage(JSONObject(state.toString())) } }
    }

    fun removeListener(listener: Listener) {
        listeners.remove(listener)
    }

    /** Sends one JSON message (it must be an object with a `type`). False when not authenticated or not queued. */
    fun sendRaw(json: String): Boolean = try {
        connection?.link?.send(json) ?: false
    } catch (_: Throwable) {
        false
    }

    /** Bytes waiting in the ctl send buffer; 0 when not connected. */
    fun bufferedBytes(): Long = try {
        connection?.link?.bufferedBytes ?: 0L
    } catch (_: Throwable) {
        0L
    }

    /** Milliseconds since the last input event was sent; [Long.MAX_VALUE] when there was none. */
    fun sinceLastInputMs(): Long {
        val at = lastInputAt
        return if (at < 0) Long.MAX_VALUE else (inputClock() - at).coerceAtLeast(0)
    }

    /** Whether a connection is authenticated right now. */
    fun isOpen(): Boolean = connection != null

    // ---- the owner's hooks -------------------------------------------------------------------------

    /** Stamps "input was sent now" (one volatile store; call where input goes out). */
    internal fun inputSent() {
        lastInputAt = inputClock()
    }

    /** An authenticated connection starts: creates its uploader (when logging is on). */
    internal fun attach(pcId: String, link: CtlLink) {
        detach()
        val transport = ListenerTransport(link, this)
        val uploader = try {
            BpLog.current()?.let { uploaderFactory(transport, it, pcId) }
        } catch (_: Throwable) {
            null
        }
        connection = Connection(pcId, link, uploader, transport)
        try {
            uploader?.start()
        } catch (_: Throwable) {
        }
    }

    /**
     * Routes an incoming endpoint message: `caps` (the PC's first ctl message), `logs-state`, `logs-ack`,
     * `feedback-ack`. Other types are ignored.
     */
    internal fun onEndpoint(message: JSONObject) {
        try {
            val current = connection ?: return
            when (message.optString("type")) {
                "caps" -> {
                    if (current.peerCaps != null) return
                    val array = message.optJSONArray("caps")
                    val caps = (0 until (array?.length() ?: 0)).mapNotNull { array!!.optString(it).ifEmpty { null } }
                    current.peerCaps = caps
                    listeners.forEach { l -> guard { l.onOpened(current.pcId, caps) } }
                }
                "logs-state" -> {
                    current.state = JSONObject(message.toString())
                    current.transport.deliver(message)
                    listeners.forEach { l -> guard { l.onMessage(JSONObject(message.toString())) } }
                }
                "logs-ack" ->
                    if (message.optString("stream") == BpLog.SRC) current.transport.deliver(message)
                    else listeners.forEach { l -> guard { l.onMessage(JSONObject(message.toString())) } }
                "feedback-ack" -> listeners.forEach { l -> guard { l.onMessage(JSONObject(message.toString())) } }
            }
        } catch (_: Throwable) {
        }
    }

    /** The connection ends: closes its uploader and tells the listeners. */
    internal fun detach() {
        val old = connection ?: return
        connection = null
        try {
            old.uploader?.close()
        } catch (_: Throwable) {
        }
        if (old.peerCaps != null) listeners.forEach { l -> guard { l.onClosed() } }
    }

    private inline fun guard(block: () -> Unit) {
        try {
            block()
        } catch (_: Throwable) {
        }
    }

    /** The Kotlin uploader's view of the connection. Incoming messages arrive through [deliver]. */
    private class ListenerTransport(private val link: CtlLink, private val owner: CtlPassThrough) : LogUploadTransport {
        @Volatile private var listener: ((JSONObject) -> Unit)? = null

        override fun setListener(listener: ((JSONObject) -> Unit)?) {
            this.listener = listener
        }

        fun deliver(message: JSONObject) {
            listener?.invoke(JSONObject(message.toString()))
        }

        override fun send(message: JSONObject): Boolean = try {
            link.send(message.toString())
        } catch (_: Throwable) {
            false
        }

        override val bufferedBytes: Long get() = link.bufferedBytes
        override val sinceLastInputMs: Long get() = owner.sinceLastInputMs()
    }

    companion object {
        /** Capability string advertised in auth-hello and in the ctl caps message. */
        const val CAP = "client-logs-v1"
    }
}
