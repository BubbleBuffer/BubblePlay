package de.bubblebuffer.bubbleplay.net

import android.os.Handler
import android.os.HandlerThread
import android.os.Looper
import android.os.SystemClock
import android.util.Log
import de.bubblebuffer.bubbleplay.input.RemoteInput
import de.bubblebuffer.bubbleplay.media.VideoDecoder
import de.bubblebuffer.bubbleplay.protocol.ClipboardReceiver
import de.bubblebuffer.bubbleplay.protocol.CursorState
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
    private val preferredCodec: String,
    private val listener: Listener,
) : RemoteInput.Transport, VideoDecoder.Listener {

    interface Listener {
        fun onStatus(status: Status)
        fun onAuthRequired()
        fun onCursor(state: CursorState)
        fun onRemoteClipboard(text: String)
        fun onMonitors(monitors: List<Monitor>, error: String?)
        fun onVideoSize(width: Int, height: Int)
        /** Input became usable after a presented frame (sync the cursor). */
        fun onInputReady()
        fun onSecureDesktop(secure: Boolean)
        /** The signaling socket carrying input was lost: drop held keys/buttons locally. */
        fun onInputLost()
    }

    enum class State { CONNECTING, CONNECTED, STATIC, RECONNECTING, ERROR }

    data class Status(val state: State, val text: String)

    data class Monitor(val selector: String, val name: String, val width: Int, val height: Int, val selected: Boolean)

    private var config = config
    private val thread = HandlerThread("bp-session").apply { start() }
    private val handler = Handler(thread.looper)
    private val main = Handler(Looper.getMainLooper())
    val decoder = VideoDecoder(this)
    private val http = OkHttpClient.Builder()
        .connectTimeout(10, TimeUnit.SECONDS)
        .readTimeout(0, TimeUnit.MILLISECONDS)
        .writeTimeout(10, TimeUnit.SECONDS)
        .build()
    private val random = SecureRandom()
    private val startedAt = SystemClock.elapsedRealtimeNanos()

    private var running = false
    private var terminal: String? = null

    // Signaling (/ws).
    @Volatile private var sig: WebSocket? = null
    /** Owns the current signaling generation and decides which socket may carry input. */
    private val link = InputLink<WebSocket>()
    private val liveness = SignalLiveness(V4.HEARTBEAT_MS)
    @Volatile private var sigOpen = false
    @Volatile private var registered = false
    private var sigBackoff = 0L
    private var sigReconnecting = false
    private var mediaEpoch: String? = null

    // Host state.
    @Volatile private var scancode = false
    @Volatile private var clipboardEnabled = false
    @Volatile private var desktopAvailable = true
    @Volatile private var inputEnabled = true
    @Volatile private var awaitingFrame = true
    private var desktopKind = "default"
    private var desktopGeneration: Int? = null
    private var hasPresented = false
    private val clipboardReceiver = ClipboardReceiver()

    // Media (/media-ws, v4).
    private var media: WebSocket? = null
    private var mediaId = 0
    private var mediaOpen = false
    private var mediaEverOpened = false
    private var mediaReconnecting = false
    private var mediaBackoff = 0L
    private var hostPresent = false
    private var hostStarted = false
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
    private var ffGeneration = 0L
    private var ffAdmitted = false
    private var ffPresented = false
    private var ffRequireAdmission = false
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
            handler.postDelayed(this, 250)
        }
    }

    // Diagnostics: one logcat line every 5 s while frames flow.
    private var statsAt = 0L
    private var statsReceived = 0L
    private var statsDecoded = 0L
    private var statsPresented = 0L
    private var statsSkipped = 0L

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
        handler.post(tick)
    }

    /** Closes both sockets and the decoder; the session cannot be restarted. */
    fun stop() {
        handler.post {
            running = false
            handler.removeCallbacksAndMessages(null)
            closeMedia()
            link.reset()
            try { sig?.close(1000, "client-stop") } catch (_: Exception) {}
            sig = null; sigOpen = false; registered = false
            thread.quitSafely()
        }
        decoder.release()
    }

    fun requestMonitor(selector: String) = handler.post {
        sendSig(JSONObject().put("type", "media-control").put("action", "monitor").put("protocol", V4.VERSION)
            .put("epoch", mediaEpoch).put("monitor", selector).put("reason", "user-monitor"))
    }

    fun requestMonitorList() = handler.post {
        sendSig(JSONObject().put("type", "media-control").put("action", "monitors").put("protocol", V4.VERSION)
            .put("epoch", mediaEpoch).put("reason", "user-refresh"))
    }

    // ---- RemoteInput.Transport (any thread) ----------------------------------
    // Input only ever goes out on the current, hello-completed, first-frame-ready socket.
    override fun inputOpen() = link.inputSocket() != null && inputEnabled && desktopAvailable && !awaitingFrame
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
                sendSig(JSONObject().put("type", "hello").put("role", "client"))
            }

            override fun onMessage(webSocket: WebSocket, text: String) = post({ id }) {
                liveness.inbound(now())
                handleSignal(text, id)
            }

            override fun onMessage(webSocket: WebSocket, bytes: ByteString) {
                if (webSocket !== sig) return // a stale socket
                liveness.inbound(now())
                val state = CursorState.decode(bytes.toByteArray()) ?: return
                main.post { listener.onCursor(state) }
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

    /** Runs [block] on the session thread only if that socket generation is still current. */
    private inline fun post(crossinline gen: () -> Int, crossinline block: () -> Unit) {
        handler.post { if (running && link.isCurrent(gen())) block() }
    }

    private fun signalingDown(id: Int, httpCode: Int?, why: String) {
        if (!link.lost(id)) return
        Log.i(TAG, "signaling down: $why (http=$httpCode)")
        sigOpen = false
        registered = false
        sig = null
        // The media socket may keep playing (status: input reconnecting); it is
        // bound to this signaling epoch and is replaced after the next hello-ok.
        resetFirstFrame()
        main.post { listener.onInputLost() }
        if (!config.isLab && httpCode != null && httpCode in listOf(401, 403, 404)) { authRequired(); return }
        scheduleSignalingReconnect(why)
    }

    private fun scheduleSignalingReconnect(why: String) {
        if (!running || terminal != null) return
        sigBackoff = V4.nextBackoffMs(sigBackoff)
        sigReconnecting = true
        Log.i(TAG, "signaling reconnect in ${sigBackoff}ms ($why)")
        handler.postDelayed({ openSignaling() }, sigBackoff)
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
                val epoch = msg.optString("mediaEpoch").takeIf { Regex("^[a-f0-9]{32}$").matches(it) }
                if (!link.helloOk(id, epoch)) return
                registered = true
                sigBackoff = 0
                sigReconnecting = false
                // A new signaling epoch: the old media socket belongs to the old
                // epoch, and the host needs a fresh first-frame handshake before it
                // accepts input again (it ignores input until then).
                if (media != null) closeMedia()
                resetFirstFrame()
                mediaEpoch = epoch
                sendSig(JSONObject().put("type", "media-control").put("action", "mode").put("protocol", V4.VERSION)
                    .put("epoch", mediaEpoch).put("streamMode", "work").put("reason", "signaling-start"))
                startMedia()
            }
            "host-capabilities" -> if (!mediaOpen && media == null && registered) startMedia()
            "input-capabilities" -> {
                scancode = msg.optInt("protocol") == 1 && msg.optBoolean("scancode")
                clipboardEnabled = msg.optInt("protocol") == 1 && msg.optBoolean("clipboard")
            }
            "desktop-state" -> handleDesktopState(msg)
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
            }
            "monitor-error" -> {
                val reason = msg.optString("reason").ifEmpty { "unknown error" }.take(180)
                main.post { listener.onMonitors(emptyList(), reason) }
            }
            "peer-left" -> {
                // The host went away: start a fresh signaling epoch, as the browser does.
                Log.i(TAG, "peer-left: recycling signaling")
                link.reset()
                try { sig?.close(1000, "peer-left") } catch (_: Exception) {}
                sig = null; sigOpen = false; registered = false
                closeMedia()
                resetFirstFrame()
                main.post { listener.onInputLost() }
                sigBackoff = 0
                scheduleSignalingReconnect("peer-left")
            }
            "error" -> {
                terminal = msg.optString("message").ifEmpty { "relay error" }
                Log.w(TAG, "relay error: $terminal")
                publishStatus()
            }
        }
    }

    private fun handleDesktopState(msg: JSONObject) {
        val state = msg.optString("state")
        if (state != "active" && state != "unavailable") return
        val generation = msg.optIntOrNull("generation")
        val generationChanged = generation != null && generation != desktopGeneration
        if (generation != null) desktopGeneration = generation
        msg.optString("desktop").takeIf { it in listOf("default", "secure", "none") }?.let { kind ->
            if (kind != desktopKind) {
                desktopKind = kind
                if (kind == "secure") clipboardReceiver.reset()
                main.post { listener.onSecureDesktop(kind == "secure") }
            }
        }
        msg.optString("input").takeIf { it in listOf("enabled", "suspended", "read-only") }?.let { inputEnabled = it == "enabled" }
        val reason = msg.optString("reason")
        val transition = reason == "monitor-switch" || reason == "monitor-switched" ||
            (state == "active" && reason in listOf("desktop-switched", "capture-recovered", "session-changed", "agent-restarted"))
        if (transition && hasPresented) {
            desktopAvailable = true
            setAwaiting(true)
            if (state == "active") startFirstFrame("desktop-transition", true)
            return
        }
        desktopAvailable = state == "active"
        setAwaiting(true)
        if (!desktopAvailable) cancelFirstFrame()
        else startFirstFrame("desktop-state", generationChanged)
    }

    // ---- first-frame handshake -------------------------------------------------
    private fun startFirstFrame(reason: String, forceNew: Boolean) {
        if (!sigOpen || mediaEpoch == null) return
        // Like the browser: an existing request only counts for the same media epoch.
        if (!forceNew && firstFrameRequestReusable(ffRequestId, ffEpoch, mediaEpoch, ffFailed)) return
        cancelFirstFrame()
        val bytes = ByteArray(12).also { random.nextBytes(it) }
        ffRequestId = Base64.getUrlEncoder().withoutPadding().encodeToString(bytes)
        ffEpoch = mediaEpoch
        ffGeneration = 0; ffAdmitted = false; ffPresented = false; ffRetries = 0; ffFailed = false
        ffRequireAdmission = forceNew || reason == "desktop-transition"
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
            if (id != ffRequestId || ffPresented) return@Runnable
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
        cancelFirstFrame()
        ffRequestId = null; ffEpoch = null
        ffGeneration = 0; ffAdmitted = false; ffPresented = false; ffRetries = 0; ffFailed = false
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
                ffAdmitted = true
                msg.optLongOrNull("generation")?.let { ffGeneration = it }
                if (ffPresented) sendPresented()
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

    private fun markPresented(generation: Long) {
        if (!hasPresented) Log.i(TAG, "first frame presented ${(SystemClock.elapsedRealtimeNanos() - startedAt) / 1_000_000} ms after session start")
        hasPresented = true
        if (ffPresented || ffRequestId == null) return
        if (ffRequireAdmission && !ffAdmitted) return
        if (ffRequireAdmission && ffGeneration != 0L && generation != ffGeneration) return
        ffPresented = true
        cancelFirstFrame()
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
        Log.w(TAG, "startup recovery: $reason")
        closeMedia()
        mediaBackoff = V4.nextBackoffMs(mediaBackoff)
        mediaReconnecting = true
        handler.postDelayed({ if (registered) startMedia() }, mediaBackoff)
    }

    // ---- media (v4) -------------------------------------------------------------
    private fun startMedia() {
        if (mediaEpoch == null || media != null) return
        openMedia()
    }

    private fun openMedia() {
        val epoch = mediaEpoch ?: return
        if (!running || !registered) return
        val id = ++mediaId
        val cookie = if (config.isLab) null else cookieProvider()
        val request = Request.Builder().url(config.mediaUrl(epoch)).apply { if (cookie != null) header("Cookie", cookie) }.build()
        media = http.newWebSocket(request, object : WebSocketListener() {
            override fun onOpen(webSocket: WebSocket, response: Response) = mediaPost(id) {
                mediaLastSend = now(); mediaLastInbound = now()
                webSocket.send("{\"type\":\"hello\",\"role\":\"media-client\"}")
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
                    onPacket(packet, (receivedAt - startedAt) / 1_000_000.0)
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
                mediaBackoff = 0
                // Anything in flight on the old socket is gone: restart from a keyframe.
                gate.reset(if (reconnect) "reconnect" else "connect")
                decoder.dropPending()
                if (!hostStarted) {
                    hostStarted = true
                    sendSig(JSONObject().put("type", "media-control").put("action", "start").put("protocol", V4.VERSION)
                        .put("epoch", mediaEpoch).put("codec", preferredCodec).put("streamMode", "work")
                        .put("reason", "client-start").put("quality", "high"))
                    startFirstFrame("wss-host-start", false)
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
        lastFrameAt = now()
        val verdict = gate.accept(frame)
        verdict.requestKeyframe?.let { requestKeyframe(it) }
        if (!verdict.decode) return
        if (frame.key && (frame.width != videoWidth || frame.height != videoHeight || frame.codec != activeCodec)) {
            videoWidth = frame.width; videoHeight = frame.height; activeCodec = frame.codec
            val (w, h) = videoWidth to videoHeight
            main.post { listener.onVideoSize(w, h) }
        }
        decoder.submit(frame)
    }

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
        if (!running || !registered) return
        mediaBackoff = V4.nextBackoffMs(mediaBackoff)
        mediaReconnecting = true
        val id = mediaId
        handler.postDelayed({ if (id == mediaId && media == null) openMedia() }, mediaBackoff)
    }

    private fun closeMedia() {
        mediaId++
        try { media?.close(1000, "client-close") } catch (_: Exception) {}
        media = null
        mediaOpen = false
        link.mediaUp = false
        hostPresent = false
        hostStarted = false
        gate.reset("connect")
        decoder.dropPending()
    }

    // ---- decoder callbacks (decoder thread) -------------------------------------
    override fun onFramePresented(generation: Long, seq: Long, width: Int, height: Int) {
        framesPresented++
        handler.post { markPresented(generation) }
    }

    override fun onNeedsKeyframe(reason: String) {
        handler.post {
            gate.invalidate(reason)
            if (mediaOpen) requestKeyframe(reason, force = reason == "surface")
        }
    }

    override fun onDecoderSelected(name: String, mime: String, hardware: Boolean, lowLatency: Boolean) {
        Log.i(TAG, "decoder $name ($mime) hw=$hardware lowLatency=$lowLatency")
        handler.post { decoderLabel = if (hardware) "hw" else "sw" }
    }

    // ---- heartbeat and status ----------------------------------------------------
    private fun heartbeat() {
        val now = now()
        if (sigOpen) {
            // The relay answers every ping; an unanswered one means a half-open
            // socket (probed early while input is flowing). Cancelling it runs
            // the normal signalingDown -> reconnect path.
            when (liveness.check(now)) {
                SignalLiveness.Action.PING -> sendSig(JSONObject().put("type", "ping"))
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
        if (link.inputReconnecting && framesPresented > 0) return Status(State.RECONNECTING, "Input reconnecting…")
        if (!sigOpen || !registered) return Status(if (sigReconnecting) State.RECONNECTING else State.CONNECTING, if (sigReconnecting) "Reconnecting…" else "Connecting…")
        if (!desktopAvailable) return Status(State.ERROR, "Desktop unavailable")
        if (mediaOpen && !hostPresent && framesPresented == 0L) return Status(State.CONNECTING, "Waiting for host…")
        return when (V4.status(mediaOpen, mediaReconnecting, framesPresented, lastFrameAt, now())) {
            V4.Status.CONNECTED -> Status(State.CONNECTED, "Connected · ${streamLabel()}")
            V4.Status.PAUSED_STATIC -> Status(State.STATIC, "Connected · static")
            V4.Status.RECONNECTING -> Status(State.RECONNECTING, "Reconnecting media…")
            V4.Status.STARTING -> Status(State.CONNECTING, "Starting stream…")
            V4.Status.CONNECTING -> Status(State.CONNECTING, "Connecting…")
        }
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
