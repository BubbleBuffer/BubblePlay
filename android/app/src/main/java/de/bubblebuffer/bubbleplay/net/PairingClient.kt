package de.bubblebuffer.bubbleplay.net

import android.os.Handler
import android.os.Looper
import de.bubblebuffer.bubbleplay.auth.AuthCrypto
import de.bubblebuffer.bubbleplay.auth.AuthEvent
import de.bubblebuffer.bubbleplay.auth.AuthFailure
import de.bubblebuffer.bubbleplay.auth.AuthJson
import de.bubblebuffer.bubbleplay.auth.AuthRandom
import de.bubblebuffer.bubbleplay.auth.AuthStep
import de.bubblebuffer.bubbleplay.auth.Handshake
import de.bubblebuffer.bubbleplay.auth.PairAdmit
import de.bubblebuffer.bubbleplay.auth.PairLink
import de.bubblebuffer.bubbleplay.auth.ViewerAuth
import de.bubblebuffer.bubbleplay.auth.ViewerHello
import de.bubblebuffer.bubbleplay.keys.DeviceKey
import de.bubblebuffer.bubbleplay.keys.DeviceKeyException
import de.bubblebuffer.bubbleplay.log.BpLog
import de.bubblebuffer.bubbleplay.pcs.PcEntry
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import okio.ByteString
import java.util.concurrent.Executor
import java.util.concurrent.Executors
import java.util.concurrent.ScheduledExecutorService
import java.util.concurrent.ScheduledFuture
import java.util.concurrent.TimeUnit

/** Progress of one pairing attempt, for the pairing screen. */
enum class PairingStage {
    /** Opening /ws and proving this device's key to the relay. */
    Connecting,
    /** Admitted in pairing mode; exchanging keys and pairing proofs with the PC. */
    Proving,
    /** Proofs verified; the comparison number is shown and the PC owner decides. */
    WaitingForPc,
}

enum class PairingFailure {
    /** Policy close (1008), or a close before admission: wrong/expired code or no pairing window. */
    Refused,
    /** The PC's pairing proof did not verify against the code. */
    ProofMismatch,
    /** The PC's ID, key or handshake signature is invalid. */
    PcKeyInvalid,
    /** The socket dropped after admission. */
    ConnectionLost,
    /** No answer from the relay or the PC in time. */
    Timeout,
}

/** Callbacks run on the main thread. One terminal callback ends an attempt, unless cancelled. */
interface PairingListener {
    fun onStage(stage: PairingStage)
    /** The comparison number, display form "482 913". */
    fun onNumber(display: String)
    /** The PC sent pair-done. [pc] carries the pinned key; the caller stores it. */
    fun onPaired(pc: PcEntry)
    fun onDenied()
    fun onFailed(reason: PairingFailure)
}

/**
 * Runs one pairing at the PC. One instance owns one socket and never reconnects automatically.
 * [name] becomes [PcEntry.name] of the result.
 */
class PairingClient(
    private val key: DeviceKey,
    private val link: PairLink,
    private val name: String,
    private val listener: PairingListener,
) {
    private enum class State { New, Opening, Challenge, Admission, AuthHost, PcProof, Decision, Done }

    private val lock = Any()
    private var state = State.New
    private var cancelled = false
    private var admitted = false
    private var socket: WebSocket? = null
    private var auth: ViewerAuth? = null
    private var sockets: WebSocket.Factory? = null
    private var callbacks: Executor? = null
    private var scheduler: ScheduledExecutorService? = null
    private var ownedHttp: OkHttpClient? = null
    private var ownsScheduler = false
    private var timeout: ScheduledFuture<*>? = null
    private var heartbeat: ScheduledFuture<*>? = null
    private var timeoutGeneration = 0L
    private var random = AuthRandom.secure()
    private var now: () -> Long = System::currentTimeMillis

    /** Transport and time seams keep JVM tests independent of Android's main looper. */
    internal constructor(
        key: DeviceKey,
        link: PairLink,
        name: String,
        listener: PairingListener,
        sockets: WebSocket.Factory,
        callbacks: Executor,
        scheduler: ScheduledExecutorService,
        random: AuthRandom = AuthRandom.secure(),
        now: () -> Long = System::currentTimeMillis,
    ) : this(key, link, name, listener) {
        this.sockets = sockets
        this.callbacks = callbacks
        this.scheduler = scheduler
        this.random = random
        this.now = now
    }

    fun start() {
        synchronized(lock) {
            if (state != State.New) return
            if (callbacks == null) {
                val main = Handler(Looper.getMainLooper())
                callbacks = Executor { main.post(it) }
            }
            if (scheduler == null) {
                scheduler = Executors.newSingleThreadScheduledExecutor { task ->
                    Thread(task, "bp-pairing-timeout").apply { isDaemon = true }
                }
                ownsScheduler = true
            }
            state = State.Opening
            BpLog.i(TAG, "pairing started", link.pcId, mapOf("relay" to link.relay))
            emit { onStage(PairingStage.Connecting) }
            // A synchronous injected callback may have cancelled the attempt.
            if (state == State.Done) return
            val base = ServerConfig.parseServer(link.relay)
            if (base == null || PairLink.parse(link.toUri()) != link) {
                fail(PairingFailure.Refused, "the relay address or the pairing link is not valid")
                return
            }
            val request = Request.Builder().url(ServerConfig(base, link.pcId).signalingUrl()).build()
            if (sockets == null) {
                ownedHttp = OkHttpClient.Builder().readTimeout(0, TimeUnit.MILLISECONDS).build()
                sockets = ownedHttp
            }
            armTimeout(RELAY_TIMEOUT_MS)
            val opened = sockets!!.newWebSocket(request, wireListener)
            if (state == State.Done) opened.cancel() else socket = opened
        }
    }

    /** Ends the attempt; no listener call follows. */
    fun cancel() {
        synchronized(lock) {
            if (state != State.Done) BpLog.i(TAG, "pairing cancelled by the user", link.pcId, mapOf("state" to state.name))
            cancelled = true
            stop(abort = true)
        }
    }

    private val wireListener = object : WebSocketListener() {
        override fun onOpen(webSocket: WebSocket, response: Response) {
            synchronized(lock) {
                if (state != State.Opening) return
                socket = webSocket
                state = State.Challenge
                BpLog.i(TAG, "relay socket open", link.pcId)
                try {
                    send(ViewerHello.hello(key, PairAdmit.hash(link.pcId, link.code)))
                } catch (e: IllegalArgumentException) {
                    fail(PairingFailure.Refused, "the relay hello could not be built", e)
                }
            }
        }

        override fun onMessage(webSocket: WebSocket, text: String) {
            synchronized(lock) {
                if (state == State.Done || state == State.New) return
                try {
                    receive(text)
                } catch (e: AuthJson.Malformed) {
                    fail(if (admitted) PairingFailure.ConnectionLost else PairingFailure.Refused, "malformed message (${e.message}) in state ${state.name}")
                } catch (e: DeviceKeyException) {
                    fail(PairingFailure.ConnectionLost, "device key refused in state ${state.name}", e, mapOf("refusal" to e.reason.name))
                }
            }
        }

        override fun onMessage(webSocket: WebSocket, bytes: ByteString) {
            synchronized(lock) {
                if (state != State.Done) {
                    fail(if (admitted) PairingFailure.ConnectionLost else PairingFailure.Refused, "unexpected binary frame")
                }
            }
        }

        override fun onClosing(webSocket: WebSocket, code: Int, reason: String) {
            synchronized(lock) {
                if (state == State.Done) return
                webSocket.close(code, "")
                fail(closeFailure(code), "socket closing", null, closeFields(code, reason))
            }
        }

        override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
            synchronized(lock) {
                if (state != State.Done) fail(closeFailure(code), "socket closed", null, closeFields(code, reason))
            }
        }

        override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
            synchronized(lock) {
                if (state != State.Done) {
                    fail(PairingFailure.ConnectionLost, "socket failure", t, mapOf("http" to response?.code))
                }
            }
        }
    }

    private fun closeFields(code: Int, reason: String): Map<String, Any?> =
        mapOf("wsClose" to code, "closeReason" to reason.take(120))

    private fun receive(text: String) {
        val type = AuthJson.string(AuthJson.parseObject(text), "type")
        when (state) {
            State.Challenge -> {
                val nonce = ViewerHello.parseRegisterChallenge(text)
                state = State.Admission
                BpLog.i(TAG, "relay challenge answered", link.pcId)
                send(ViewerHello.registerProof(key, link.relay, nonce))
            }
            State.Admission -> {
                val hello = ViewerHello.parseHelloOk(text, link.pcId)
                if (!hello.pairing) {
                    fail(PairingFailure.Refused, "the relay admitted this device but not in pairing mode")
                    return
                }
                admitted = true
                BpLog.i(TAG, "admitted by the relay in pairing mode", link.pcId)
                // Relay idle expiry is 90 s; the owner may use the full five-minute decision window.
                heartbeat = scheduler!!.scheduleWithFixedDelay({
                    synchronized(lock) {
                        if (state != State.Done) send("{\"type\":\"ping\"}")
                    }
                }, 30_000L, 30_000L, TimeUnit.MILLISECONDS)
                auth = ViewerAuth(Handshake.start(
                    key, link.relay, hello.mediaEpoch,
                    Handshake.Mode.Pairing(link.pcId, link.code), random,
                ))
                state = State.AuthHost
                armTimeout(ViewerAuth.AUTH_HOST_TIMEOUT_MS)
                emit { onStage(PairingStage.Proving) }
                if (state != State.Done) apply(auth!!.start())
            }
            State.AuthHost, State.PcProof, State.Decision -> when (type) {
                "pong", "peer-joined" -> Unit // Relay bookkeeping never advances the handshake.
                "peer-left", "error" -> fail(PairingFailure.ConnectionLost, "the relay sent $type in state ${state.name}")
                "pair-closed" -> fail(PairingFailure.Refused, "the relay sent pair-closed (pairing window closed or used)")
                else -> {
                    val waitingForHost = state == State.AuthHost
                    val step = auth!!.onMessage(text)
                    if (waitingForHost && type == "auth-host" && step.event !is AuthEvent.Failed) {
                        state = State.PcProof
                        armTimeout(PC_DECISION_TIMEOUT_MS)
                    }
                    apply(step)
                }
            }
            else -> fail(PairingFailure.ConnectionLost, "message in unexpected state ${state.name}")
        }
    }

    private fun apply(step: AuthStep) {
        for (message in step.send) if (!send(message)) return
        when (val event = step.event) {
            null -> Unit
            is AuthEvent.PairNumber -> {
                state = State.Decision
                armTimeout(PC_DECISION_TIMEOUT_MS)
                BpLog.i(TAG, "comparison number shown, waiting for the PC owner", link.pcId)
                emit { onStage(PairingStage.WaitingForPc) }
                emit { onNumber(event.display) }
            }
            is AuthEvent.Paired -> {
                val pc = PcEntry(event.pcId, AuthCrypto.b64(event.pcPublic), link.relay, name, now())
                BpLog.i(TAG, "pairing complete, handing the PC to the caller", link.pcId)
                stop()
                emit { onPaired(pc) }
            }
            AuthEvent.Denied -> {
                BpLog.i(TAG, "pairing denied by the PC owner", link.pcId)
                stop()
                emit { onDenied() }
            }
            is AuthEvent.Failed -> fail(authFailure(event.reason), "authentication failed: ${event.reason.name}")
            is AuthEvent.Authenticated -> fail(PairingFailure.ConnectionLost, "unexpected normal-mode authentication")
        }
    }

    private fun authFailure(reason: AuthFailure): PairingFailure = when (reason) {
        AuthFailure.PAIR_MAC_MISMATCH -> PairingFailure.ProofMismatch
        AuthFailure.ID_MISMATCH, AuthFailure.WRONG_PC, AuthFailure.PC_KEY_CHANGED,
        AuthFailure.BAD_SIGNATURE, AuthFailure.KEY_AGREEMENT -> PairingFailure.PcKeyInvalid
        AuthFailure.MALFORMED -> if (state == State.AuthHost) PairingFailure.PcKeyInvalid else PairingFailure.ConnectionLost
        AuthFailure.UNEXPECTED_MESSAGE -> PairingFailure.ConnectionLost
    }

    private fun send(text: String): Boolean {
        if (state == State.Done) return false
        if (socket?.send(text) == true) return true
        fail(PairingFailure.ConnectionLost, "sending to the relay failed in state ${state.name}")
        return false
    }

    private fun closeFailure(code: Int): PairingFailure =
        if (!admitted || code == 1008) PairingFailure.Refused else PairingFailure.ConnectionLost

    private fun fail(
        reason: PairingFailure,
        why: String,
        error: Throwable? = null,
        extra: Map<String, Any?> = emptyMap(),
    ) {
        if (state == State.Done) return
        val fields = linkedMapOf<String, Any?>(
            "failure" to reason.name, "state" to state.name, "admitted" to admitted, "why" to why,
        )
        fields.putAll(extra)
        BpLog.w(TAG, "pairing failed", link.pcId, fields, error)
        stop(abort = true)
        emit { onFailed(reason) }
    }

    private fun armTimeout(delayMs: Long) {
        timeout?.cancel(false)
        val generation = ++timeoutGeneration
        timeout = scheduler!!.schedule({
            synchronized(lock) {
                if (state != State.Done && generation == timeoutGeneration) fail(PairingFailure.Timeout, "no answer in state ${state.name} within $delayMs ms")
            }
        }, delayMs, TimeUnit.MILLISECONDS)
    }

    private fun stop(abort: Boolean = false) {
        state = State.Done
        timeoutGeneration++
        timeout?.cancel(false)
        timeout = null
        heartbeat?.cancel(false)
        heartbeat = null
        auth = null
        val oldSocket = socket
        socket = null
        if (abort) oldSocket?.cancel() else oldSocket?.close(1000, "")
        if (ownsScheduler) scheduler?.shutdownNow()
        ownedHttp?.let {
            it.dispatcher.executorService.shutdown()
            it.connectionPool.evictAll()
        }
        ownedHttp = null
    }

    private fun emit(call: PairingListener.() -> Unit) {
        callbacks!!.execute {
            synchronized(lock) {
                if (!cancelled) listener.call()
            }
        }
    }

    companion object {
        private const val TAG = "PairingClient"
        private const val RELAY_TIMEOUT_MS = 30_000L
        internal const val PC_DECISION_TIMEOUT_MS = 300_000L
    }
}
