package de.bubblebuffer.bubbleplay.net

import android.os.Handler
import android.os.HandlerThread
import android.os.Looper
import android.os.SystemClock
import android.util.Log
import de.bubblebuffer.bubbleplay.input.RemoteInput
import de.bubblebuffer.bubbleplay.media.AudioPlayer
import de.bubblebuffer.bubbleplay.media.VideoDecoder
import de.bubblebuffer.bubbleplay.protocol.ClipboardReceiver
import de.bubblebuffer.bubbleplay.protocol.CursorState
import de.bubblebuffer.bubbleplay.protocol.AudioWire
import de.bubblebuffer.bubbleplay.protocol.CursorKind
import de.bubblebuffer.bubbleplay.protocol.FileWire
import de.bubblebuffer.bubbleplay.protocol.FollowWindow
import de.bubblebuffer.bubbleplay.protocol.V4
import de.bubblebuffer.bubbleplay.protocol.V4ReceiveGate
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import okio.ByteString
import okio.ByteString.Companion.toByteString
import org.json.JSONObject
import java.security.SecureRandom
import java.util.Base64
import java.util.concurrent.TimeUnit

/**
 * One remote-desktop session: the authenticated signaling socket (/ws), the
 * single v4 media socket (/media-ws) and the decoder. Mirrors the browser
 * receiver's WSS flow (worker/public/receiver.js):
 *
 *  1. /ws with the gate cookie, hello role=client -> hello-ok {mediaEpoch}.
 *  2. media-control mode, then /media-ws?epoch=..., hello role=media-client.
 *  3. On media hello-ok: media-control start {codec} + first-frame request.
 *  4. Binary frames: ACK each, gate, decode; keyframe requests on gaps and
 *     decoder errors; 15 s heartbeats on both sockets.
 *  5. Input and cursor state stay on /ws as small binary packets.
 *
 * All state lives on one session thread; listener calls arrive on the main thread.
 */
class Session(
    config: ServerConfig,
    private val cookieProvider: () -> String?,
    val preferredCodec: String,
    private val listener: Listener,
) : RemoteInput.Transport, VideoDecoder.Listener {

    interface Listener {
        fun onStatus(status: Status)
        fun onAuthRequired()
        fun onCursor(state: CursorState)
        fun onCursorType(kind: CursorKind)
        fun onRemoteClipboard(text: String)
        fun onMonitors(monitors: List<Monitor>, error: String?)
        fun onVideoSize(width: Int, height: Int)
        /** A live frame reached the surface: the first of this session, or the first since the surface changed. */
        fun onLiveFrame()
        /** Input became usable after a presented frame (sync the cursor). */
        fun onInputReady()
        /** The secure desktop (UAC prompt, lock screen) and what the viewer may do there; null when it closes. */
        fun onSecureDesktop(state: SecureDesktop?)
        /** Answer to [denySecurePrompt]. */
        fun onSecureActionResult(ok: Boolean, reason: String)
        /** The desktop/caret context changed; discard the IME's known remote tail. */
        fun onDesktopChanged()
        /** The signaling socket carrying input was lost: drop held keys/buttons locally. */
        fun onInputLost()
        /** File transfer v1: true/false from host-capabilities, null while no host is known. */
        fun onFileCapability(enabled: Boolean?)
        /** The signaling epoch the file socket pairs on (null: none). */
        fun onFileEpoch(epoch: String?)
        /** The grant of the current signaling session (PC and ticket, session memory only); empty once signaling closed. */
        fun onViewerGrant(grant: ViewerGrant) {}
        /** Host sound (BPA1) from host-capabilities; false when the host does not stream sound. */
        fun onSoundCapability(enabled: Boolean)
        /** A host `file-offer`; an empty item list withdraws it. */
        fun onFileOffer(offer: FileWire.Offer)
        /** Follow active window: whether the host can follow (false while no capable host is known). */
        fun onFollowCapability(enabled: Boolean)
        /** The host's follow state from `monitor-list.follow`. */
        fun onFollowState(follow: Boolean)
    }

    enum class State { CONNECTING, CONNECTED, STATIC, RECONNECTING, ERROR }

    data class Status(val state: State, val text: String, val inputWarning: Boolean = false)

    /** Live values for the settings view's Diagnostics (null: not known yet). */
    data class Diagnostics(
        val decoder: String?,
        val codec: String?,
        val width: Int,
        val height: Int,
        val fps: Double?,
        val kbps: Double?,
        val rttMs: Long?,
    )

    data class Monitor(val selector: String, val name: String, val width: Int, val height: Int, val selected: Boolean)

    private var config = config
    private val thread = HandlerThread("bp-session").apply { start() }
    private val handler = Handler(thread.looper)
    private val main = Handler(Looper.getMainLooper())
    val decoder = VideoDecoder(this)
    // Host sound: created on the first BPA1 packet and released with the media leg.
    private var audio: AudioPlayer? = null
    /** Local mute of the host's sound. Not persisted: each session starts unmuted. */
    @Volatile var soundMuted = false
        private set
    private var soundForeground = true
    private val http = OkHttpClient.Builder()
        .connectTimeout(10, TimeUnit.SECONDS)
        .readTimeout(0, TimeUnit.MILLISECONDS)
        .writeTimeout(10, TimeUnit.SECONDS)
        .build()
    private val random = SecureRandom()
    private val startedAt = SystemClock.elapsedRealtimeNanos()

    private var running = false
    @Volatile private var cursorCallbacksEnabled = true
    private var terminal: String? = null

    // Signaling (/ws).
    @Volatile private var sig: WebSocket? = null
    private val pendingCursor = PendingCursor<WebSocket>()
    /** Owns the current signaling generation and decides which socket may carry input. */
    private val link = InputLink<WebSocket>()
    private val liveness = SignalLiveness(V4.HEARTBEAT_MS)
    @Volatile private var sigOpen = false
    @Volatile private var registered = false
    private val sigRetry = ReconnectLoop()
    private var sigRetryTimer: Runnable? = null
    private var sigReconnecting = false
    private var mediaEpoch: String? = null
    private val grant = ViewerGrantSlot()

    // Host state.
    @Volatile private var scancode = false
    @Volatile private var clipboardEnabled = false
    @Volatile private var desktopAvailable = true
    @Volatile private var inputEnabled = true
    @Volatile private var awaitingFrame = true
    private var desktopKind = "default"
    private var secureDesktop: SecureDesktop? = null
    // A same-source monitor switch gates input and keeps held input (RemoteInput.Transport.inputGated).
    @Volatile private var monitorHold = false
    private val inputWarning = InputStatusWarning()
    // Follow active window: the wanted state (sent on every capable connect) and the host's capability.
    @Volatile private var followWanted = false
    /** "work" (absolute pointer, desktop frame rate) or "game" (captured mouse); the host caps work at 30 fps. */
    @Volatile private var streamMode = MODE_WORK
    private var followCapable = false
    private var desktopGeneration: Int? = null
    private var hasPresented = false
    private val clipboardReceiver = ClipboardReceiver()

    // Media (/media-ws, v4).
    private var media: WebSocket? = null
    private val mediaLeg = MediaLeg()
    private val mediaId get() = mediaLeg.id
    private var mediaOpen = false
    private var mediaEverOpened = false
    private var mediaReconnecting = false
    private val mediaRetry = ReconnectLoop()
    private var mediaRetryTimer: Runnable? = null
    private var hostPresent = false
    private val surfaceRecovery = SurfaceRecovery()
    private var surfaceWatchdog: Runnable? = null
    /** Report the next presented frame (first one of this session or on a new surface). */
    private var reportLiveFrame = true
    private var mediaLastSend = 0L
    private var mediaLastInbound = 0L
    private val gate = V4ReceiveGate()
    private var lastKeyframeRequestAt = -1_000L
    private var framesReceived = 0L
    private var lastFrameAt: Long? = null
    @Volatile private var framesPresented = 0L
    private var videoWidth = 0
    private var videoHeight = 0
    private var activeCodec = 0
    private var decoderLabel = ""

    // First-frame handshake (host gates input on "presented").
    private var ffRequestId: String? = null
    private var ffEpoch: String? = null
    private val firstFramePresentation = FirstFramePresentation()
    private val ffGeneration get() = firstFramePresentation.generation
    private val ffAdmitted get() = firstFramePresentation.admitted
    private val ffPresented get() = firstFramePresentation.presented
    private var ffRetries = 0
    private var ffFailed = false
    private var ffWatchdog: Runnable? = null

    private var lastStatus: Status? = null
    private val tick = object : Runnable {
        override fun run() {
            if (!running) return
            heartbeat()
            publishStatus()
            logStats()
            updateDiagnostics()
            handler.postDelayed(this, 250)
        }
    }

    // Diagnostics: one logcat line every 5 s while frames flow.
    private var statsAt = 0L
    private var statsReceived = 0L
    private var statsDecoded = 0L
    private var statsPresented = 0L
    private var statsSkipped = 0L

    // Diagnostics: presented fps and media bitrate over ~1 s windows; relay round trip from ping/pong.
    @Volatile var diagnostics = Diagnostics(null, null, 0, 0, null, null, null)
        private set
    private var mediaBytes = 0L
    private var diagAt = 0L
    private var diagBytes = 0L
    private var diagPresented = 0L
    private var decoderName: String? = null
    private var pingSentAt = 0L
    private var rttMs: Long? = null

    private fun updateDiagnostics() {
        val now = now()
        if (diagAt == 0L) { diagAt = now; diagBytes = mediaBytes; diagPresented = framesPresented; return }
        val elapsed = now - diagAt
        if (elapsed < 1_000) return
        val flowing = mediaOpen && lastFrameAt?.let { now - it < 2_000 } == true
        val fps = if (flowing) (framesPresented - diagPresented) * 1000.0 / elapsed else null
        val kbps = if (mediaOpen) (mediaBytes - diagBytes) * 8.0 / elapsed else null
        val codec = when (activeCodec) { V4.CODEC_HEVC -> "HEVC"; V4.CODEC_H264 -> "H.264"; else -> null }
        diagnostics = Diagnostics(decoderName, codec, videoWidth, videoHeight, fps, kbps, rttMs)
        diagAt = now; diagBytes = mediaBytes; diagPresented = framesPresented
    }

    /** Sends an extra signaling ping so the round trip is fresh (diagnostics only; the relay answers pong). */
    fun probeRoundTrip() = handler.post {
        if (pingSentAt == 0L || now() - pingSentAt > PING_STALE_MS) sendPing()
    }

    private fun sendPing() {
        if (sendSig(JSONObject().put("type", "ping"))) pingSentAt = now()
    }

    private fun logStats() {
        val now = now()
        if (statsAt == 0L) { statsAt = now; return }
        val elapsed = now - statsAt
        if (elapsed < 5_000) return
        val received = framesReceived
        val decoded = decoder.decodedFrames
        val presented = decoder.presentedFrames
        val skipped = decoder.skippedFrames
        if (received != statsReceived) {
            fun fps(n: Long) = "%.1f".format(java.util.Locale.US, n * 1000.0 / elapsed)
            Log.i(TAG, "stats ${streamLabel()} recv=${fps(received - statsReceived)}fps " +
                "decoded=${fps(decoded - statsDecoded)}fps presented=${fps(presented - statsPresented)}fps " +
                "skipped=${skipped - statsSkipped} pendingInput=${queuedBytes()}B")
        }
        statsAt = now; statsReceived = received; statsDecoded = decoded; statsPresented = presented; statsSkipped = skipped
    }

    fun start() = handler.post {
        if (running) return@post
        running = true
        terminal = null
        openSignaling()
        if (surfaceWatchdog == null) surfaceRecovery.waitingGeneration()?.let { armSurfaceWatchdog(it) }
        handler.post(tick)
    }

    /** Closes both sockets and the decoder; the session cannot be restarted. */
    fun stop() {
        cursorCallbacksEnabled = false
        handler.post {
            running = false
            handler.removeCallbacksAndMessages(null)
            closeMedia()
            link.reset()
            try { sig?.close(1000, "client-stop") } catch (_: Exception) {}
            sig = null; sigOpen = false; registered = false
            clearGrant()
            thread.quitSafely()
        }
        decoder.release()
    }

    fun requestMonitor(selector: String) = handler.post {
        sendSig(JSONObject().put("type", "media-control").put("action", "monitor").put("protocol", V4.VERSION)
            .put("epoch", mediaEpoch).put("monitor", selector).put("reason", "user-monitor"))
    }

    /** Ask the host's SYSTEM helper to cancel the open UAC prompt (never approves it). */
    fun denySecurePrompt() = handler.post {
        if ("deny" in (secureDesktop?.capabilities ?: emptySet())) sendSig(JSONObject().put("type", "secure-action").put("action", "deny"))
    }

    /** Pointer mode for the host's stream policy; sent now when signaling is up, and on every connect. */
    fun setStreamMode(game: Boolean) = handler.post {
        val mode = if (game) MODE_GAME else MODE_WORK
        if (mode == streamMode) return@post
        streamMode = mode
        if (mediaEpoch != null) sendSig(JSONObject().put("type", "media-control").put("action", "mode").put("protocol", V4.VERSION)
            .put("epoch", mediaEpoch).put("streamMode", mode).put("reason", "user-mode"))
    }

    /** Follow active window on or off; sent now when the host can follow, and again on every connect. */
    fun setFollow(enabled: Boolean) = handler.post {
        followWanted = enabled
        if (followCapable) sendSig(FollowWindow.control(enabled, mediaEpoch))
    }

    fun requestMonitorList() = handler.post {
        sendSig(JSONObject().put("type", "media-control").put("action", "monitors").put("protocol", V4.VERSION)
            .put("epoch", mediaEpoch).put("reason", "user-refresh"))
    }

    // ---- RemoteInput.Transport (any thread) ----------------------------------
    // Input only ever goes out on the current, hello-completed, first-frame-ready socket.
    override fun inputOpen() = link.inputSocket() != null && inputEnabled && desktopAvailable && !awaitingFrame
    override fun inputGated() = monitorHold && sigOpen && link.inputSocket() != null && inputEnabled && desktopAvailable
    override fun send(bytes: ByteArray): Boolean {
        val socket = link.inputSocket() ?: return false
        liveness.input(now())
        return socket.send(bytes.toByteString())
    }
    override fun queuedBytes(): Long = sig?.queueSize() ?: 0L
    override fun scancodeHost() = scancode
    fun clipboardEnabled() = clipboardEnabled && desktopKind != "secure"

    // ---- signaling ------------------------------------------------------------
    private fun openSignaling() {
        // A new signaling socket starts without the previous one's credentials.
        clearGrant()
        if (!running || terminal != null) return
        if (config.isLab && config.labCompat == null) {
            val compat = fetchLabCompat()
            if (compat == null) { scheduleSignalingReconnect("lab-compat"); return }
            config = config.copy(labCompat = compat)
        }
        val cookie = if (config.isLab) null else cookieProvider()
        if (!config.isLab && !ServerConfig.hasSessionCookies(cookie)) { authRequired(); return }
        // The generation is assigned right after newWebSocket returns, on this
        // thread; callbacks read it on this thread too (via handler.post).
        var id = 0
        val request = Request.Builder().url(config.signalingUrl()).apply { if (cookie != null) header("Cookie", cookie) }.build()
        val socket = http.newWebSocket(request, object : WebSocketListener() {
            override fun onOpen(webSocket: WebSocket, response: Response) = post({ id }) {
                sigOpen = true
                liveness.opened(now())
                sendSig(ServerConfig.signalingHello())
            }

            override fun onMessage(webSocket: WebSocket, text: String) = post({ id }) {
                liveness.inbound(now())
                handleSignal(text, id)
            }

            override fun onMessage(webSocket: WebSocket, bytes: ByteString) {
                if (webSocket !== sig || !PendingCursor.accepts(bytes)) return
                liveness.inbound(now())
                if (!pendingCursor.offer(webSocket, bytes)) return
                main.post deliver@{
                    val update = pendingCursor.take() ?: return@deliver
                    // A queued packet must not restore old appearance after a reset/disconnect.
                    if (!cursorCallbacksEnabled || update.source !== sig) return@deliver
                    update.kind?.let { listener.onCursorType(it) }
                    update.state?.let { listener.onCursor(it) }
                }
            }

            override fun onClosing(webSocket: WebSocket, code: Int, reason: String) {
                webSocket.close(1000, null)
            }

            override fun onClosed(webSocket: WebSocket, code: Int, reason: String) = post({ id }) { signalingDown(id, null, "closed $code") }

            override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
                val code = response?.code
                post({ id }) { signalingDown(id, code, t.message ?: "failure") }
            }
        })
        sig = socket
        id = link.connecting(socket)
    }

    /** Signaling closed: forget the PC and ticket (session memory only) and tell the listener. */
    private fun clearGrant() {
        if (!grant.clear()) return
        main.post { listener.onViewerGrant(ViewerGrant.NONE) }
    }

    /** Runs [block] on the session thread only if that socket generation is still current. */
    private inline fun post(crossinline gen: () -> Int, crossinline block: () -> Unit) {
        handler.post { if (running && link.isCurrent(gen())) block() }
    }

    private fun signalingDown(id: Int, httpCode: Int?, why: String) {
        if (!link.lost(id)) return
        Log.i(TAG, "signaling down: $why (http=$httpCode)")
        inputWarning.reset()
        sigOpen = false
        registered = false
        sig = null
        clearGrant()
        // The media socket may keep playing (status: input reconnecting); it is
        // bound to this signaling epoch and is replaced after the next hello-ok.
        resetFirstFrame()
        main.post { listener.onInputLost() }
        if (!config.isLab && httpCode != null && httpCode in listOf(401, 403, 404)) { authRequired(); return }
        scheduleSignalingReconnect(why)
    }

    private fun scheduleSignalingReconnect(why: String) {
        if (!running || terminal != null) return
        sigReconnecting = true
        cancelSigRetryTimer()
        val delay = sigRetry.schedule()
        if (delay == null) { Log.i(TAG, "signaling reconnect held until foreground ($why)"); return }
        Log.i(TAG, "signaling reconnect in ${delay}ms ($why)")
        val retry = Runnable {
            sigRetryTimer = null
            if (sigRetry.fire()) openSignaling() else Log.i(TAG, "signaling reconnect held until foreground ($why)")
        }
        sigRetryTimer = retry
        handler.postDelayed(retry, delay)
    }

    private fun cancelSigRetryTimer() {
        sigRetryTimer?.let { handler.removeCallbacks(it) }
        sigRetryTimer = null
    }

    /**
     * The app entered (true) or left (false) the foreground. In the background no
     * reconnect is attempted; on return the backoff resets and a held or pending
     * retry runs immediately.
     */
    fun setForeground(value: Boolean) = handler.post {
        if (!value) {
            sigRetry.background(); mediaRetry.background()
            return@post
        }
        val sigOwed = sigRetry.foreground()
        val mediaOwed = mediaRetry.foreground()
        if (!running) return@post
        if (sigOwed) {
            cancelSigRetryTimer()
            Log.i(TAG, "foreground: signaling reconnect now")
            openSignaling()
        }
        if (mediaOwed) {
            cancelMediaRetryTimer()
            if (registered && media == null) { Log.i(TAG, "foreground: media reconnect now"); startMedia() }
        }
    }

    private fun authRequired() {
        running = false
        handler.removeCallbacksAndMessages(null)
        main.post { listener.onAuthRequired() }
    }

    private fun sendSig(message: JSONObject): Boolean {
        val socket = sig ?: return false
        if (!sigOpen) return false
        liveness.sent(now())
        return socket.send(message.toString())
    }

    private fun handleSignal(text: String, id: Int) {
        val msg = try { JSONObject(text) } catch (_: Exception) { return }
        when (msg.optString("type")) {
            "hello-ok" -> {
                // Every hello-ok on the current socket replaces the credentials, before any early return.
                if (!link.isCurrent(id)) return
                val granted = grant.helloOk(msg)
                main.post { listener.onViewerGrant(granted) }
                val epoch = ViewerGrant.stringField(msg, "mediaEpoch")?.takeIf { Regex("^[a-f0-9]{32}$").matches(it) }
                if (!link.helloOk(id, epoch)) return
                registered = true
                sigRetry.succeeded()
                sigReconnecting = false
                // Surface misses were not counted while signaling was down: give the
                // waiting surface a full window from here.
                surfaceRecovery.waitingGeneration()?.let { generation ->
                    surfaceWatchdog?.let { handler.removeCallbacks(it) }
                    armSurfaceWatchdog(generation)
                }
                // A new signaling epoch: the old media socket belongs to the old
                // epoch, and the host needs a fresh first-frame handshake before it
                // accepts input again (it ignores input until then).
                closeMedia()
                resetFirstFrame()
                mediaEpoch = epoch
                main.post { listener.onFileEpoch(epoch) }
                sendSig(JSONObject().put("type", "media-control").put("action", "mode").put("protocol", V4.VERSION)
                    .put("epoch", mediaEpoch).put("streamMode", streamMode).put("reason", "signaling-start"))
                startMedia()
            }
            "host-capabilities" -> {
                val files = msg.optInt("fileTransfer") >= 1
                val sound = msg.optInt("audio") >= 1
                main.post { listener.onFileCapability(files); listener.onSoundCapability(sound) }
                followCapable = FollowWindow.capable(msg)
                val follow = followCapable
                main.post { listener.onFollowCapability(follow) }
                // Host follow state starts off for each signaling session: resend a wanted "on".
                if (follow && followWanted) sendSig(FollowWindow.control(true, mediaEpoch))
                if (!mediaOpen && media == null && registered) startMedia()
            }
            "pong" -> {
                if (pingSentAt != 0L) { rttMs = now() - pingSentAt; pingSentAt = 0L }
            }
            "file-offer" -> {
                val offer = FileWire.parseOfferMessage(msg) ?: return
                main.post { listener.onFileOffer(offer) }
            }
            "input-capabilities" -> {
                scancode = msg.optInt("protocol") == 1 && msg.optBoolean("scancode")
                clipboardEnabled = msg.optInt("protocol") == 1 && msg.optBoolean("clipboard")
            }
            "desktop-state" -> handleDesktopState(msg)
            "input-status" -> {
                val update = InputStatusUpdate.parse(msg) ?: return
                inputWarning.apply(update)
                publishStatus()
            }
            "secure-action-result" -> if (msg.optString("action") == "deny") {
                val ok = msg.optBoolean("ok")
                val reason = msg.optString("reason").take(120)
                main.post { listener.onSecureActionResult(ok, reason) }
            }
            "first-frame" -> handleFirstFrame(msg)
            "clipboard" -> {
                if (!clipboardEnabled()) return
                val complete = clipboardReceiver.push(
                    msg.optIntOrNull("protocol"), msg.optLongOrNull("id"), msg.optIntOrNull("index"),
                    msg.optIntOrNull("count"), if (msg.has("text")) msg.optString("text") else null,
                ) ?: return
                main.post { listener.onRemoteClipboard(complete) }
            }
            "monitor-list" -> {
                val array = msg.optJSONArray("monitors") ?: return
                if (array.length() > 16) return
                val monitors = (0 until array.length()).mapNotNull { i ->
                    val m = array.optJSONObject(i) ?: return@mapNotNull null
                    val selector = m.optString("selector")
                    if (selector.isEmpty() || selector.length > 128) return@mapNotNull null
                    Monitor(selector, m.optString("name").ifEmpty { selector }, m.optInt("width"), m.optInt("height"), m.optBoolean("selected"))
                }
                main.post { listener.onMonitors(monitors, null) }
                FollowWindow.listState(msg)?.let { follow -> main.post { listener.onFollowState(follow) } }
            }
            "monitor-error" -> {
                val reason = msg.optString("reason").ifEmpty { "unknown error" }.take(180)
                main.post { listener.onMonitors(emptyList(), reason) }
            }
            "peer-left" -> {
                // The host went away: start a fresh signaling epoch, as the browser does.
                Log.i(TAG, "peer-left: recycling signaling")
                inputWarning.reset()
                link.reset()
                try { sig?.close(1000, "peer-left") } catch (_: Exception) {}
                sig = null; sigOpen = false; registered = false
                clearGrant()
                closeMedia()
                resetFirstFrame()
                desktopKind = "default"; secureDesktop = null
                followCapable = false
                main.post { listener.onInputLost(); listener.onSecureDesktop(null); listener.onFileCapability(null); listener.onSoundCapability(false); listener.onFileEpoch(null); listener.onFollowCapability(false) }
                sigRetry.resetBackoff()
                scheduleSignalingReconnect("peer-left")
            }
            "error" -> {
                terminal = msg.optString("message").ifEmpty { "relay error" }
                clearGrant()
                Log.w(TAG, "relay error: $terminal")
                publishStatus()
            }
        }
    }

    private fun handleDesktopState(msg: JSONObject) {
        val state = msg.optString("state")
        if (state != "active" && state != "unavailable") return
        val oldDesktopKind = desktopKind
        val oldAvailable = desktopAvailable
        val generation = msg.optIntOrNull("generation")
        val generationChanged = generation != null && generation != desktopGeneration
        if (generation != null) desktopGeneration = generation
        msg.optString("desktop").takeIf { it in listOf("default", "secure", "none") }?.let { kind ->
            if (kind != desktopKind) {
                desktopKind = kind
                if (kind == "secure") clipboardReceiver.reset()
            }
        }
        val secure = if (desktopKind == "secure") SecureDesktop.parse(msg) else null
        if (secure != secureDesktop) {
            secureDesktop = secure
            main.post { listener.onSecureDesktop(secure) }
        }
        msg.optString("input").takeIf { it in listOf("enabled", "suspended", "read-only") }?.let { inputEnabled = it == "enabled" }
        val reason = msg.optString("reason")
        val monitorSwitch = reason == "monitor-switch" || reason == "monitor-switched"
        val transition = monitorSwitch ||
            (state == "active" && reason in listOf("desktop-switched", "capture-recovered", "session-changed", "agent-restarted"))
        if (generationChanged || oldDesktopKind != desktopKind || oldAvailable != (state == "active") || transition) {
            main.post { listener.onDesktopChanged() }
        }
        if (transition && hasPresented) {
            // Monitor switches keep held input; a desktop-source change releases it, as before.
            monitorHold = monitorSwitch && oldDesktopKind == desktopKind
            desktopAvailable = true
            setAwaiting(true)
            if (state == "active") startFirstFrame("desktop-transition", true)
            return
        }
        monitorHold = false
        desktopAvailable = state == "active"
        setAwaiting(true)
        if (!desktopAvailable) cancelFirstFrame()
        else startFirstFrame("desktop-state", generationChanged)
    }

    // ---- first-frame handshake -------------------------------------------------
    private fun startFirstFrame(reason: String, forceNew: Boolean) {
        if (!sigOpen || mediaEpoch == null || !surfaceRecovery.canRestartMedia()) return
        // Like the browser: an existing request only counts for the same media epoch.
        if (!forceNew && firstFrameRequestReusable(ffRequestId, ffEpoch, mediaEpoch, ffFailed)) return
        cancelFirstFrame()
        val bytes = ByteArray(12).also { random.nextBytes(it) }
        ffRequestId = Base64.getUrlEncoder().withoutPadding().encodeToString(bytes)
        ffEpoch = mediaEpoch
        firstFramePresentation.start(ffRequestId!!, mediaId, forceNew || reason == "desktop-transition")
        ffRetries = 0; ffFailed = false
        setAwaiting(true)
        Log.i(TAG, "first-frame request ($reason)")
        sendFirstFrameRequest()
        armFirstFrameWatchdog()
    }

    private fun sendFirstFrameRequest() {
        val id = ffRequestId ?: return
        sendSig(JSONObject().put("type", "first-frame").put("protocol", 1).put("phase", "request").put("requestId", id)
            .put("transport", "wss").put("epoch", mediaEpoch))
    }

    private fun armFirstFrameWatchdog() {
        ffWatchdog?.let { handler.removeCallbacks(it) }
        val id = ffRequestId
        val watchdog = Runnable {
            if (id != ffRequestId || ffPresented || !surfaceRecovery.canRestartMedia()) return@Runnable
            if (ffRetries < 1) {
                ffRetries++
                sendFirstFrameRequest()
                sendSig(JSONObject().put("type", "media-control").put("action", "keyframe").put("protocol", V4.VERSION)
                    .put("epoch", mediaEpoch).put("reason", "first-frame-timeout").put("codec", codecName()))
                armFirstFrameWatchdog()
            } else {
                ffFailed = true
                recoverStartup("presentation-timeout")
            }
        }
        ffWatchdog = watchdog
        handler.postDelayed(watchdog, 5_000)
    }

    private fun cancelFirstFrame() {
        ffWatchdog?.let { handler.removeCallbacks(it) }
        ffWatchdog = null
    }

    /** Forget the handshake of a finished signaling epoch; input waits for the next one. */
    private fun resetFirstFrame() {
        monitorHold = false
        cancelFirstFrame()
        ffRequestId = null; ffEpoch = null
        firstFramePresentation.reset()
        ffRetries = 0; ffFailed = false
        setAwaiting(true)
    }

    private fun setAwaiting(value: Boolean) {
        awaitingFrame = value
        if (value) link.awaitFrame() else link.presented()
    }

    private fun handleFirstFrame(msg: JSONObject) {
        if (msg.optInt("protocol") != 1 || msg.optString("requestId") != ffRequestId) return
        when (msg.optString("phase")) {
            "admitted" -> {
                val completed = firstFramePresentation.admit(ffRequestId!!, msg.optLongOrNull("generation") ?: ffGeneration)
                if (completed) completePresentation()
                else if (ffPresented) sendPresented()
            }
            "failed" -> {
                if (ffPresented) { cancelFirstFrame(); setAwaiting(false); return }
                ffFailed = true
                cancelFirstFrame()
                recoverStartup(msg.optString("reason").ifEmpty { "host-failure" }.take(96))
            }
            "ready" -> cancelFirstFrame()
        }
    }

    private fun markPresented(generation: Long, mediaLeg: Int) {
        if (mediaLeg != mediaId || !surfaceRecovery.canRestartMedia()) return
        if (!hasPresented) Log.i(TAG, "first frame presented ${(SystemClock.elapsedRealtimeNanos() - startedAt) / 1_000_000} ms after session start")
        hasPresented = true
        val requestId = ffRequestId ?: return
        if (firstFramePresentation.observe(requestId, mediaLeg, generation)) completePresentation()
    }

    private fun completePresentation() {
        cancelFirstFrame()
        monitorHold = false
        val wasAwaiting = awaitingFrame
        setAwaiting(false)
        if (ffAdmitted) sendPresented()
        if (wasAwaiting) main.post { listener.onInputReady() }
    }

    private fun sendPresented() {
        val id = ffRequestId ?: return
        sendSig(JSONObject().put("type", "first-frame").put("protocol", 1).put("phase", "presented").put("requestId", id)
            .put("generation", ffGeneration))
    }

    /** The stream did not start: rebuild the media path in the same signaling epoch. */
    private fun recoverStartup(reason: String) {
        if (!surfaceRecovery.canRestartMedia()) {
            Log.w(TAG, "startup recovery blocked after surface exhaustion: $reason")
            cancelFirstFrame()
            return
        }
        Log.w(TAG, "startup recovery: $reason")
        closeMedia()
        scheduleMediaRetry("startup-recovery")
    }

    /** Reopens /media-ws after the backoff, for the current media leg only; held while backgrounded. */
    private fun scheduleMediaRetry(why: String) {
        mediaReconnecting = true
        cancelMediaRetryTimer()
        val delay = mediaRetry.schedule()
        if (delay == null) { Log.i(TAG, "media reconnect held until foreground ($why)"); return }
        val id = mediaId
        val retry = Runnable {
            mediaRetryTimer = null
            if (!mediaRetry.fire()) { Log.i(TAG, "media reconnect held until foreground ($why)"); return@Runnable }
            if (running && registered && id == mediaId && media == null) startMedia()
        }
        mediaRetryTimer = retry
        handler.postDelayed(retry, delay)
    }

    private fun cancelMediaRetryTimer() {
        mediaRetryTimer?.let { handler.removeCallbacks(it) }
        mediaRetryTimer = null
    }

    // ---- media (v4) -------------------------------------------------------------
    private fun startMedia() {
        if (mediaEpoch == null || media != null || !surfaceRecovery.canRestartMedia()) return
        openMedia()
    }

    private fun openMedia() {
        val epoch = mediaEpoch ?: return
        if (!running || !registered || !surfaceRecovery.canRestartMedia()) return
        val id = mediaLeg.open()
        val cookie = if (config.isLab) null else cookieProvider()
        val current = grant.current
        val request = Request.Builder().url(config.mediaUrl(epoch, current.pc)).apply { if (cookie != null) header("Cookie", cookie) }.build()
        media = http.newWebSocket(request, object : WebSocketListener() {
            override fun onOpen(webSocket: WebSocket, response: Response) = mediaPost(id) {
                // Signaling closed or replaced the grant while this socket connected: its ticket is stale, so it never sends it.
                if (grant.current !== current) { webSocket.cancel(); return@mediaPost }
                mediaLastSend = now(); mediaLastInbound = now()
                webSocket.send(current.hello("media-client"))
            }

            override fun onMessage(webSocket: WebSocket, text: String) = mediaPost(id) {
                mediaLastInbound = now()
                handleMediaText(text)
            }

            override fun onMessage(webSocket: WebSocket, bytes: ByteString) {
                val receivedAt = SystemClock.elapsedRealtimeNanos()
                val packet = bytes.toByteArray()
                mediaPost(id) {
                    mediaLastInbound = now()
                    // BPA1 audio shares the socket: no ACK, and it never reaches the v4 gate.
                    if (AudioWire.isAudio(packet)) onAudioPacket(packet)
                    else onPacket(packet, (receivedAt - startedAt) / 1_000_000.0)
                }
            }

            override fun onClosing(webSocket: WebSocket, code: Int, reason: String) {
                webSocket.close(1000, null)
            }

            override fun onClosed(webSocket: WebSocket, code: Int, reason: String) = mediaPost(id) { mediaDown("closed $code") }

            override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) = mediaPost(id) {
                mediaDown(t.message ?: "failure")
            }
        })
    }

    private inline fun mediaPost(id: Int, crossinline block: () -> Unit) {
        handler.post { if (running && id == mediaId) block() }
    }

    private fun sendMedia(text: String): Boolean {
        val socket = media ?: return false
        if (!mediaOpen) return false
        mediaLastSend = now()
        return socket.send(text)
    }

    private fun handleMediaText(text: String) {
        val msg = try { JSONObject(text) } catch (_: Exception) { return }
        when (msg.optString("type")) {
            "hello-ok" -> {
                val reconnect = mediaEverOpened
                mediaOpen = true
                link.mediaUp = true
                mediaEverOpened = true
                mediaReconnecting = false
                mediaRetry.succeeded()
                // Anything in flight on the old socket is gone: restart from a keyframe.
                gate.reset(if (reconnect) "reconnect" else "connect")
                decoder.dropPending()
                if (mediaLeg.startHost()) {
                    sendSig(AudioWire.optIn(JSONObject().put("type", "media-control").put("action", "start").put("protocol", V4.VERSION)
                        .put("epoch", mediaEpoch).put("codec", preferredCodec).put("streamMode", streamMode)
                        .put("reason", "client-start").put("quality", "high"), AudioPlayer.decoderAvailable))
                    startFirstFrame("wss-host-start", true)
                }
                if (reconnect) requestKeyframe("reconnect", force = true)
            }
            "media-peer" -> {
                val present = msg.optBoolean("present")
                if (present && !hostPresent) gate.reset("host-joined")
                hostPresent = present
            }
        }
    }

    private fun onPacket(packet: ByteArray, receivedMs: Double) {
        val frame = V4.parse(packet) ?: return
        // ACK on receipt: the host measures delivery, not decode time.
        sendMedia(V4.ackMessage(frame.generation, frame.seq, receivedMs, decoder.lastDecodedSeq))
        framesReceived++
        mediaBytes += packet.size
        lastFrameAt = now()
        val verdict = gate.accept(frame)
        verdict.requestKeyframe?.let { requestKeyframe(it) }
        if (!verdict.decode) return
        if (frame.key && (frame.width != videoWidth || frame.height != videoHeight || frame.codec != activeCodec)) {
            videoWidth = frame.width; videoHeight = frame.height; activeCodec = frame.codec
            val (w, h) = videoWidth to videoHeight
            main.post { listener.onVideoSize(w, h) }
        }
        decoder.submit(frame, mediaId)
    }

    private fun onAudioPacket(packet: ByteArray) {
        val parsed = AudioWire.parse(packet) ?: return
        if (!AudioPlayer.decoderAvailable) return
        val player = audio ?: AudioPlayer(soundAudible()).also { audio = it }
        player.offer(parsed)
    }

    /** Local mute, the activity's stopped state: the sound is silenced, its packets are still decoded. */
    fun setSoundMuted(muted: Boolean) {
        soundMuted = muted
        handler.post { audio?.setAudible(soundAudible()) }
    }

    fun setSoundForeground(value: Boolean) = handler.post {
        soundForeground = value
        audio?.setAudible(soundAudible())
    }

    private fun soundAudible() = !soundMuted && soundForeground

    private fun requestKeyframe(reason: String, force: Boolean = false) {
        val now = now()
        if (!force && now - lastKeyframeRequestAt < V4.KEYFRAME_REQUEST_INTERVAL_MS) return
        lastKeyframeRequestAt = now
        Log.i(TAG, "keyframe request: $reason")
        if (!sendMedia(V4.keyframeMessage(reason))) {
            sendSig(JSONObject().put("type", "media-control").put("action", "keyframe").put("protocol", V4.VERSION)
                .put("epoch", mediaEpoch).put("reason", reason).put("codec", codecName()))
        }
    }

    private fun mediaDown(why: String) {
        Log.i(TAG, "media down: $why")
        mediaOpen = false
        link.mediaUp = false
        hostPresent = false
        media = null
        if (!running || !registered || !surfaceRecovery.canRestartMedia()) return
        scheduleMediaRetry("media-down")
    }

    private fun closeMedia() {
        mediaLeg.invalidate()
        cancelMediaRetryTimer()
        mediaRetry.cancel()
        resetFirstFrame() // Neither early presentation nor admission can outlive its media leg.
        try { media?.close(1000, "client-close") } catch (_: Exception) {}
        media = null
        mediaOpen = false
        link.mediaUp = false
        hostPresent = false
        gate.reset("connect")
        decoder.dropPending()
        audio?.release()
        audio = null
    }

    // ---- decoder callbacks (decoder thread) -------------------------------------
    override fun onSurfaceChanged(generation: Long, attached: Boolean) {
        handler.post {
            surfaceWatchdog?.let { handler.removeCallbacks(it) }
            surfaceWatchdog = null
            surfaceRecovery.changed(generation, attached)
            reportLiveFrame = true
            if (attached && running) {
                Log.i(TAG, "surface attached generation=$generation")
                armSurfaceWatchdog(generation)
                if (registered && media == null) startMedia()
            }
        }
    }

    override fun onSurfaceOutput(generation: Long) {
        handler.post {
            if (surfaceRecovery.output(generation)) {
                Log.i(TAG, "surface output generation=$generation")
                surfaceWatchdog?.let { handler.removeCallbacks(it) }
                surfaceWatchdog = null
            }
        }
    }

    private fun armSurfaceWatchdog(generation: Long) {
        val watchdog = Runnable {
            if (!running) return@Runnable
            when (surfaceRecovery.missed(generation, signalingUp = sigOpen && registered)) {
                SurfaceRecovery.Action.WAIT -> armSurfaceWatchdog(generation)
                SurfaceRecovery.Action.KEYFRAME -> {
                    Log.w(TAG, "surface timeout generation=$generation: retry forced keyframe")
                    requestKeyframe("surface-timeout", force = true)
                    armSurfaceWatchdog(generation)
                }
                SurfaceRecovery.Action.RESTART -> {
                    Log.w(TAG, "surface timeout generation=$generation: restart media")
                    if (registered) recoverStartup("surface-timeout")
                    armSurfaceWatchdog(generation)
                }
                SurfaceRecovery.Action.EXHAUSTED -> {
                    Log.w(TAG, "surface timeout generation=$generation: recovery exhausted, automatic media restart stopped")
                    closeMedia() // Invalidates startup/media reconnect timers already queued.
                    ffFailed = true
                    mediaReconnecting = true
                    surfaceWatchdog = null
                    publishStatus()
                }
                SurfaceRecovery.Action.NONE -> Unit
            }
        }
        surfaceWatchdog = watchdog
        handler.postDelayed(watchdog, 2_000)
    }

    override fun onFramePresented(generation: Long, seq: Long, width: Int, height: Int, mediaLeg: Int) {
        handler.post {
            if (mediaLeg != mediaId) return@post
            framesPresented++
            if (reportLiveFrame) {
                reportLiveFrame = false
                main.post { listener.onLiveFrame() }
            }
            markPresented(generation, mediaLeg)
        }
    }

    override fun onNeedsKeyframe(reason: String) {
        handler.post {
            gate.invalidate(reason)
            if (mediaOpen && surfaceRecovery.canRestartMedia()) requestKeyframe(reason, force = reason == "surface")
        }
    }

    override fun onDecoderSelected(name: String, mime: String, hardware: Boolean, lowLatency: Boolean) {
        Log.i(TAG, "decoder $name ($mime) hw=$hardware lowLatency=$lowLatency")
        handler.post {
            decoderLabel = if (hardware) "hw" else "sw"
            decoderName = "$name (${if (hardware) "hardware" else "software"}${if (lowLatency) ", low latency" else ""})"
        }
    }

    // ---- heartbeat and status ----------------------------------------------------
    private fun heartbeat() {
        val now = now()
        if (sigOpen) {
            // The relay answers every ping; an unanswered one means a half-open
            // socket (probed early while input is flowing). Cancelling it runs
            // the normal signalingDown -> reconnect path.
            when (liveness.check(now)) {
                SignalLiveness.Action.PING -> sendPing()
                SignalLiveness.Action.DEAD -> { Log.w(TAG, "signaling unanswered: reconnecting"); sig?.cancel() }
                SignalLiveness.Action.NONE -> {}
            }
        }
        if (mediaOpen) {
            if (now - mediaLastSend >= V4.HEARTBEAT_MS) sendMedia(V4.PING)
            if (now - mediaLastInbound > V4.HEARTBEAT_MS * 2 + 10_000) media?.cancel()
        }
    }

    private fun publishStatus() {
        val status = computeStatus()
        if (status == lastStatus) return
        lastStatus = status
        main.post { listener.onStatus(status) }
    }

    private fun computeStatus(): Status {
        terminal?.let { return Status(State.ERROR, "Session ended: $it") }
        if (surfaceRecovery.exhausted) return Status(State.ERROR, "Stream recovery stopped. Tap to retry.")
        if (link.inputReconnecting && framesPresented > 0) return Status(State.RECONNECTING, "Input reconnecting…")
        if (!sigOpen || !registered) return Status(if (sigReconnecting) State.RECONNECTING else State.CONNECTING, if (sigReconnecting) "Reconnecting…" else "Connecting…")
        if (!desktopAvailable) return Status(State.ERROR, "Desktop unavailable")
        if (mediaOpen && !hostPresent && framesPresented == 0L) return Status(State.CONNECTING, "Waiting for host…")
        val status = when (V4.status(mediaOpen, mediaReconnecting, framesPresented, lastFrameAt, now())) {
            V4.Status.CONNECTED -> Status(State.CONNECTED, "Connected · ${streamLabel()}")
            V4.Status.PAUSED_STATIC -> Status(State.STATIC, "Connected · static")
            V4.Status.RECONNECTING -> Status(State.RECONNECTING, "Reconnecting media…")
            V4.Status.STARTING -> Status(State.CONNECTING, "Starting stream…")
            V4.Status.CONNECTING -> Status(State.CONNECTING, "Connecting…")
        }
        val warning = inputWarning.text
        return if (warning != null && status.state in setOf(State.CONNECTED, State.STATIC))
            status.copy(text = warning, inputWarning = true) else status
    }

    private fun streamLabel(): String {
        val codec = when (activeCodec) { V4.CODEC_HEVC -> "HEVC"; V4.CODEC_H264 -> "H.264"; else -> "…" }
        return "$codec ${videoWidth}×$videoHeight ${decoderLabel}".trim()
    }

    private fun codecName() = when (activeCodec) { V4.CODEC_HEVC -> "hevc"; V4.CODEC_H264 -> "h264"; else -> preferredCodec }

    private fun fetchLabCompat(): String? = try {
        val url = config.base.newBuilder().encodedPath("/lab-compat").build()
        http.newCall(Request.Builder().url(url).build()).execute().use { response ->
            if (!response.isSuccessful) null
            else JSONObject(response.body?.string() ?: "").optJSONObject("compatibility")?.optString("id")?.ifEmpty { null }
        }
    } catch (e: Exception) {
        Log.w(TAG, "lab-compat fetch failed", e)
        null
    }

    private fun now() = SystemClock.elapsedRealtime()

    companion object {
        private const val TAG = "BubblePlaySession"
        const val MODE_WORK = "work"
        const val MODE_GAME = "game"
        private const val PING_STALE_MS = 10_000L

        /** "hevc" when a hardware HEVC decoder exists, else "h264" (override with a setting). */
        fun chooseCodec(preference: String): String = when (preference) {
            "hevc", "h264" -> preference
            else -> if (VideoDecoder.hasHardwareDecoder("video/hevc")) "hevc" else "h264"
        }
    }
}

private fun JSONObject.optIntOrNull(key: String): Int? {
    val v = opt(key)
    return if (v is Number && v.toDouble() == v.toInt().toDouble()) v.toInt() else null
}

private fun JSONObject.optLongOrNull(key: String): Long? {
    val v = opt(key)
    return if (v is Number && v.toDouble() == v.toLong().toDouble()) v.toLong() else null
}
