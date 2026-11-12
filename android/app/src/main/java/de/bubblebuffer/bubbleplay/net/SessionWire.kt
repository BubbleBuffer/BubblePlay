package de.bubblebuffer.bubbleplay.net

import de.bubblebuffer.bubbleplay.auth.AuthEvent
import de.bubblebuffer.bubbleplay.auth.AuthFailure
import de.bubblebuffer.bubbleplay.auth.AuthJson
import de.bubblebuffer.bubbleplay.auth.AuthRandom
import de.bubblebuffer.bubbleplay.auth.CtlChannel
import de.bubblebuffer.bubbleplay.auth.CtlException
import de.bubblebuffer.bubbleplay.auth.Handshake
import de.bubblebuffer.bubbleplay.auth.HelloOk
import de.bubblebuffer.bubbleplay.auth.ViewerAuth
import de.bubblebuffer.bubbleplay.auth.ViewerHello
import de.bubblebuffer.bubbleplay.keys.DeviceKey
import de.bubblebuffer.bubbleplay.log.CtlPassThrough

/** Session-ending failures, with persistent resource names and an English fallback. */
internal enum class SessionProblem(val resource: String, val text: String) {
    RELAY_REFUSED("session_relay_refused", "The relay refused this device. It may have been removed on the PC; pair again at the PC."),
    DEVICE_REMOVED("session_device_removed", "The PC removed this device. Pair again at the PC to reconnect."),
    PC_KEY_CHANGED("session_pc_key_changed", "This PC's key changed. BubblePlay won't connect until you pair with it again."),
    AUTH_TIMEOUT("session_auth_timeout", "This PC does not support device keys or is not reachable."),
    CONTROL_FAILED("session_control_failed", "The encrypted control channel failed a check, so the session ended."),
    CAP_REFUSED("session_cap_refused", "The PC refused this viewer: %s"),
    AUTH_FAILED("session_auth_failed", "The PC's device-key authentication failed. Pair again at the PC to reconnect."),
    RELAY_ERROR("session_relay_error", "The relay refused the connection: %s"),
    ;

    companion object {
        /** Every policy close is terminal, whether admission finished or not. Other closes retry. */
        fun closed(code: Int?, admitted: Boolean): SessionProblem? =
            if (code == 1008) if (admitted) DEVICE_REMOVED else RELAY_REFUSED else null
    }
}

/**
 * Retry policy for a recycled session (one that already authenticated in this app run): a silent
 * host (auth-host timeout) is retried with a short, backing-off wait until [Handshake.RECYCLED_RETRY_BUDGET_MS]
 * after the loss. First connections keep the 10 s wait and the terminal timeout. Pure: the clock is injected.
 * Also tracks the loss-to-first-frame recovery for one log line.
 */
internal class RecycledRetry(private val clock: () -> Long) {
    var everAuthenticated = false
        private set
    private var budgetStart: Long? = null
    private var waits = 0
    private var recoveryStart: Long? = null
    private var attempts = 0

    /** The session was lost after being established; the first loss starts the clocks. */
    fun lost() {
        if (!everAuthenticated) return
        if (budgetStart == null) budgetStart = clock()
        if (recoveryStart == null) { recoveryStart = clock(); attempts = 0 }
    }

    /** A signaling connection is being opened. */
    fun connecting() { if (recoveryStart != null) attempts++ }

    /** The auth-host wait for the connection that was just admitted, in ms. */
    fun authHostTimeoutMs(): Long {
        if (!everAuthenticated) return Handshake.AUTH_HOST_TIMEOUT_MS
        var wait = Handshake.RECYCLED_AUTH_HOST_TIMEOUT_MS
        repeat(waits) { wait = minOf(Handshake.RECYCLED_AUTH_HOST_TIMEOUT_MAX_MS, wait * 3 / 2) }
        waits++
        return minOf(wait, Handshake.RECYCLED_AUTH_HOST_TIMEOUT_MAX_MS)
    }

    /** An auth-host timeout happened: may the session retry instead of ending? */
    fun canRetryTimeout(): Boolean {
        if (!everAuthenticated) return false
        lost()
        return clock() - budgetStart!! < Handshake.RECYCLED_RETRY_BUDGET_MS
    }

    /** The session is established again: the budget and backoff reset. */
    fun authenticated() {
        everAuthenticated = true
        budgetStart = null
        waits = 0
    }

    /** First frame presented. Returns (ms since the loss, attempts) once per recovery, else null. */
    fun recovered(): Pair<Long, Int>? {
        val start = recoveryStart ?: return null
        recoveryStart = null
        return (clock() - start) to attempts
    }

    companion object {
        /** Only the no-reply timeout is retryable; every refusal stays terminal. */
        fun retryable(problem: SessionProblem): Boolean = problem == SessionProblem.AUTH_TIMEOUT
    }
}

/**
 * One signaling socket's phase-2 protocol. No Android or socket types: the session supplies the
 * actual send function. The lock includes sealing AND sending, so concurrent input cannot put
 * seq N+1 on the socket before seq N. Each reconnect creates a new instance.
 */
internal class SessionWire(
    private val key: DeviceKey,
    private val relayOrigin: String,
    private val pcId: String,
    private val pcPublic: ByteArray,
    private val deviceName: String,
    private val send: (String) -> Boolean,
    private val random: AuthRandom = AuthRandom.secure(),
) : AutoCloseable {
    private enum class State { NEW, CHALLENGE, ADMISSION, AUTH, READY, CLOSED }
    private var state = State.NEW
    private var auth: ViewerAuth? = null
    private var channel: CtlChannel? = null
    private var firstPcControl = true
    var admitted = false
        private set

    sealed class Event {
        data class Admitted(val hello: HelloOk) : Event()
        object Authenticated : Event()
        data class Endpoint(val text: String) : Event()
        data class Cursor(val bytes: ByteArray) : Event()
        data class Relay(val text: String) : Event()
        data class Failed(val problem: SessionProblem, val detail: String? = null) : Event()
        object Disconnected : Event()
        object Ignored : Event()
    }

    @Synchronized
    fun start(): Boolean {
        check(state == State.NEW)
        state = State.CHALLENGE
        return send(ViewerHello.hello(key))
    }

    /** Relay heartbeat is the only ordinary plaintext sender. Endpoint JSON always uses ctl. */
    @Synchronized
    fun sendJson(text: String): Boolean {
        val type = AuthJson.parseObject(text)["type"] as? String ?: return false
        if (state != State.READY) return false
        if (type == "ping" || type == "pong") return send(text)
        return send(channel!!.seal(type, text.toByteArray(Charsets.UTF_8)))
    }

    @Synchronized
    fun sendInput(bytes: ByteArray): Boolean =
        state == State.READY && send(channel!!.seal(CtlChannel.KIND_INPUT, bytes))

    /** Called by the runner's 10-second auth-host timer; late timers cannot end a ready session. */
    @Synchronized
    fun authHostTimedOut(): Event = if (state == State.AUTH) fail(SessionProblem.AUTH_TIMEOUT) else Event.Ignored

    @Synchronized
    fun receive(text: String): Event {
        if (state == State.CLOSED) return Event.Ignored
        val type = try { AuthJson.parseObject(text)["type"] as? String } catch (_: Exception) {
            return fail(if (state == State.READY) SessionProblem.CONTROL_FAILED else SessionProblem.AUTH_FAILED)
        }
        return try {
            when (type) {
                "register-challenge" -> {
                    if (state != State.CHALLENGE) return fail(SessionProblem.AUTH_FAILED)
                    val nonce = ViewerHello.parseRegisterChallenge(text)
                    state = State.ADMISSION
                    if (!send(ViewerHello.registerProof(key, relayOrigin, nonce))) return disconnected()
                    Event.Ignored
                }
                "hello-ok" -> {
                    if (state != State.ADMISSION) return fail(SessionProblem.AUTH_FAILED)
                    val hello = ViewerHello.parseHelloOk(text, pcId)
                    if (hello.pairing) return fail(SessionProblem.AUTH_FAILED)
                    admitted = true
                    auth = ViewerAuth(Handshake.start(key, relayOrigin, hello.mediaEpoch,
                        Handshake.Mode.Normal(pcId, pcPublic), random, extraCaps = listOf(CtlPassThrough.CAP, MonitorPreviews.CAP)))
                    state = State.AUTH
                    for (message in auth!!.start().send) if (!send(message)) return disconnected()
                    Event.Admitted(hello)
                }
                "auth-host", "auth-finish", "auth-hello", "auth-bind", "pair-proof", "pair-done", "pair-denied" -> {
                    if (state != State.AUTH) return fail(SessionProblem.AUTH_FAILED)
                    val step = auth!!.onMessage(text)
                    for (message in step.send) if (!send(message)) {
                        (step.event as? AuthEvent.Authenticated)?.channel?.close()
                        return disconnected()
                    }
                    when (val event = step.event) {
                        is AuthEvent.Authenticated -> {
                            channel = event.channel
                            auth = null
                            state = State.READY
                            // The caps/name message must precede all application control and input.
                            if (!sendJson(AuthJson.write(linkedMapOf("type" to "caps", "caps" to listOf(CtlChannel.CAP, CtlPassThrough.CAP, MonitorPreviews.CAP),
                                "name" to cleanDeviceName(deviceName))))) return disconnected()
                            Event.Authenticated
                        }
                        is AuthEvent.Failed -> fail(if (event.reason == AuthFailure.PC_KEY_CHANGED)
                            SessionProblem.PC_KEY_CHANGED else SessionProblem.AUTH_FAILED)
                        else -> fail(SessionProblem.AUTH_FAILED)
                    }
                }
                "ctl" -> {
                    // No ctl key exists before auth-host, so an earlier ctl was sealed for the previous
                    // session of this pair (the PC had not yet seen the reconnect): drop it.
                    if (state != State.READY) return Event.Ignored
                    val opened = channel!!.open(text)
                    // A refusal may be the first ctl: it must remain visible to the user.
                    if (opened.kind == "error") {
                        val error = AuthJson.parseObject(opened.text())
                        val code = error["code"] as? String
                        if (code == "dtls-bind-required" || code == "cap-missing") {
                            return fail(SessionProblem.CAP_REFUSED, (error["message"] as? String)?.take(180) ?: code)
                        }
                    }
                    if (firstPcControl && opened.kind != CtlChannel.KIND_CAPS) return fail(SessionProblem.CONTROL_FAILED)
                    firstPcControl = false
                    if (opened.kind == CtlChannel.KIND_CURSOR) Event.Cursor(opened.plaintext)
                    else if (opened.kind == CtlChannel.KIND_INPUT) fail(SessionProblem.CONTROL_FAILED)
                    else Event.Endpoint(opened.text())
                }
                // These are generated by the relay, never treated as endpoint control.
                "ping", "pong", "peer-joined", "peer-left", "peer", "error", "pair-closed" -> Event.Relay(text)
                // Drop plaintext endpoint messages, including capability replay from older relays.
                else -> Event.Ignored
            }
        } catch (_: CtlException) {
            fail(SessionProblem.CONTROL_FAILED)
        } catch (_: Exception) {
            fail(if (state == State.READY) SessionProblem.CONTROL_FAILED else SessionProblem.AUTH_FAILED)
        }
    }

    @Synchronized
    private fun fail(problem: SessionProblem, detail: String? = null): Event.Failed {
        close()
        return Event.Failed(problem, detail)
    }

    private fun disconnected(): Event.Disconnected {
        close()
        return Event.Disconnected
    }

    @Synchronized
    override fun close() {
        channel?.close()
        channel = null
        auth = null
        state = State.CLOSED
    }

    companion object {
        fun cleanDeviceName(name: String): String = name.filterNot { Character.isISOControl(it) }.take(64)
            .let { if (it.lastOrNull()?.isHighSurrogate() == true) it.dropLast(1) else it }
    }
}
