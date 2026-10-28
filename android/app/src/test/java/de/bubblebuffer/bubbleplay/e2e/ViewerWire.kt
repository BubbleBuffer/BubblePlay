package de.bubblebuffer.bubbleplay.e2e

import de.bubblebuffer.bubbleplay.auth.AuthJson
import de.bubblebuffer.bubbleplay.auth.ViewerAuth
import de.bubblebuffer.bubbleplay.keys.DeviceKey
import de.bubblebuffer.bubbleplay.net.ServerConfig
import de.bubblebuffer.bubbleplay.net.SessionProblem
import de.bubblebuffer.bubbleplay.net.SessionWire
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import okio.ByteString
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.Executors
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit

internal fun die(message: String): Nothing = throw AssertionError(message)

/**
 * The viewer's signalling socket as Session runs it, on the JVM: the real [SessionWire] (signed
 * hello, register-proof, hello-ok, handshake with the pinned key, caps-with-name first, ctl) over an
 * OkHttp WebSocket. The Android-only glue in Session.openSignaling / handleSignal / signalingDown is
 * mirrored one to one: the 10 s auth-host watchdog armed on Admitted calls
 * [SessionWire.authHostTimedOut], and a close maps through [SessionProblem.closed] (non-null =
 * Session.endSession, which stops without reconnecting).
 */
internal class ViewerWire(
    relay: RelayServer,
    private val http: OkHttpClient,
    private val key: DeviceKey,
    private val pcId: String,
    private val pcPublic: ByteArray,
    private val name: String,
) : AutoCloseable {
    /** A socket close as Session.signalingDown sees it. [problem] non-null means the session ends. */
    data class Closed(val code: Int, val problem: SessionProblem?)

    data class Failure(val error: Throwable)

    private val origin = relay.origin
    private val url = ServerConfig(ServerConfig.parseServer(origin)!!, pcId).signalingUrl()
    val events = LinkedBlockingQueue<Any>()
    /** Every text the relay delivered to this viewer, in order. */
    val received = CopyOnWriteArrayList<String>()
    @Volatile lateinit var wire: SessionWire
        private set
    @Volatile private var socket: WebSocket? = null
    private val timers = Executors.newSingleThreadScheduledExecutor { Thread(it, "e2e-auth-watchdog").apply { isDaemon = true } }

    fun start() {
        socket = http.newWebSocket(Request.Builder().url(url).build(), object : WebSocketListener() {
            override fun onOpen(webSocket: WebSocket, response: Response) {
                val current = SessionWire(key, origin, pcId, pcPublic, name, send = { webSocket.send(it) })
                wire = current
                if (!current.start()) webSocket.cancel()
            }

            override fun onMessage(webSocket: WebSocket, text: String) {
                received.add(text)
                val current = wire
                val event = current.receive(text)
                events.add(event)
                when (event) {
                    is SessionWire.Event.Admitted -> timers.schedule(Runnable {
                        val timedOut = current.authHostTimedOut()
                        if (timedOut is SessionWire.Event.Failed) {
                            events.add(timedOut)
                            webSocket.close(1000, "session-ended")
                        }
                    }, ViewerAuth.AUTH_HOST_TIMEOUT_MS, TimeUnit.MILLISECONDS)
                    is SessionWire.Event.Failed -> webSocket.close(1000, "session-ended")
                    SessionWire.Event.Disconnected -> webSocket.cancel()
                    else -> Unit
                }
            }

            // Session ignores binary signalling.
            override fun onMessage(webSocket: WebSocket, bytes: ByteString) = Unit

            override fun onClosing(webSocket: WebSocket, code: Int, reason: String) {
                events.add(Closed(code, SessionProblem.closed(code, wire.admitted)))
                webSocket.close(code, null)
            }

            override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
                events.add(Failure(t))
            }
        })
    }

    /** The next event of type [T]; relay bookkeeping and ignored messages in between are skipped. */
    inline fun <reified T : Any> expect(timeoutMs: Long = FakePc.WAIT_MS): T {
        val deadline = System.currentTimeMillis() + timeoutMs
        while (true) {
            val left = deadline - System.currentTimeMillis()
            val event = (if (left > 0) events.poll(left, TimeUnit.MILLISECONDS) else null)
                ?: die("viewer: no ${T::class.java.simpleName} within $timeoutMs ms")
            if (event is T) return event
            if (event is SessionWire.Event.Relay || event === SessionWire.Event.Ignored) continue
            die("viewer: expected ${T::class.java.simpleName}, got $event")
        }
    }

    /** Types of the messages received after the first one of type [after]. */
    fun typesAfter(after: String): List<String?> {
        val all = received.map { AuthJson.parseObject(it)["type"] as? String }
        val index = all.indexOf(after)
        check(index >= 0) { "viewer never received $after" }
        return all.drop(index + 1)
    }

    override fun close() {
        timers.shutdownNow()
        socket?.cancel()
        socket = null
    }
}
