package de.bubblebuffer.bubbleplay.files

import android.content.ContentResolver
import android.content.Context
import android.content.res.AssetFileDescriptor
import android.net.Uri
import android.os.Handler
import android.os.HandlerThread
import android.os.Looper
import android.os.SystemClock
import android.provider.DocumentsContract
import android.provider.OpenableColumns
import android.util.Log
import android.webkit.MimeTypeMap
import de.bubblebuffer.bubbleplay.protocol.FileWire
import okhttp3.HttpUrl
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import okio.ByteString
import okio.ByteString.Companion.toByteString
import java.io.File
import java.io.IOException
import java.io.InputStream
import java.security.MessageDigest
import java.security.SecureRandom
import java.util.ArrayDeque
import java.util.concurrent.TimeUnit

/**
 * File transfer v1 client: one on-demand `/file-ws` socket (same cookie as
 * `/ws`), one upload and one download at a time (others queue), 1 MiB send
 * window, ACK pacing, SHA-256 checks, closes after 60 s idle. A socket drop
 * fails the active transfers (no resume in v1); queued ones go out on a new
 * socket. All state lives on one background thread; [Listener] callbacks
 * arrive on the main thread.
 */
class FileTransfers(
    context: Context,
    private val urlProvider: () -> HttpUrl?,
    private val cookieProvider: () -> String?,
    private val listener: Listener,
) {
    enum class Direction { UP, DOWN }

    enum class Phase { WAITING, ACTIVE, VERIFYING }

    /** One progress row: the active file in a direction plus how many wait behind it. */
    data class Row(val direction: Direction, val name: String, val done: Long, val total: Long, val phase: Phase, val queued: Int) {
        val percent: Int get() = if (total <= 0) (if (phase == Phase.VERIFYING) 100 else 0) else ((done * 100) / total).toInt().coerceIn(0, 100)
    }

    interface Listener {
        fun onProgress(up: Row?, down: Row?)
        /** A whole upload batch finished; [saved] files reached the PC. */
        fun onUploadBatchDone(purpose: String, saved: Int, failed: Int)
        fun onDownloaded(name: String, uri: Uri, mime: String)
        fun onFailed(direction: Direction, name: String, error: String)
    }

    private val app = context.applicationContext
    private val resolver: ContentResolver = app.contentResolver
    private val store = DownloadStore(app)
    private val thread = HandlerThread("bp-file").apply { start() }
    private val handler = Handler(thread.looper)
    private val main = Handler(Looper.getMainLooper())
    private val random = SecureRandom()
    private val http = OkHttpClient.Builder()
        .connectTimeout(10, TimeUnit.SECONDS)
        .readTimeout(0, TimeUnit.MILLISECONDS)
        .writeTimeout(20, TimeUnit.SECONDS)
        .build()

    // ---- jobs ---------------------------------------------------------------------------
    private class Upload(
        val source: Uri, val staged: File?, val name: String, val size: Long, val mime: String, val mtimeMs: Long,
        val purpose: String, val batch: Batch, val batchIndex: Int,
    )

    private class Batch(val id: Long, val purpose: String, var count: Int) {
        var finished = 0
        var saved = 0
        /** Cancelled by the user: no summary message. */
        var silent = false
    }

    private class Download(val offerId: Long, val item: FileWire.Item)

    private class ActiveUp(val job: Upload, val tid: Long) {
        var accepted = false
        var finishing = false
        val window = SendWindow()
        val digest: MessageDigest = MessageDigest.getInstance("SHA-256")
        var stream: InputStream? = null
        var seq = 0L
    }

    private class ActiveDown(val job: Download, val tid: Long) {
        var meta: FileWire.Meta? = null
        var target: DownloadStore.Target? = null
        var check: ReceiveCheck? = null
        val acks = AckSchedule()
    }

    private val ups = ArrayDeque<Upload>()
    private val downs = ArrayDeque<Download>()
    private var up: ActiveUp? = null
    private var down: ActiveDown? = null

    // ---- socket -------------------------------------------------------------------------
    private var ws: WebSocket? = null
    private var wsGen = 0
    private var wsOpen = false
    /** The relay reports the host's file socket present (OFFER / REQUEST only then). */
    private var peerPresent = false
    private var nextTid = 1L
    private var failures = 0
    private var enabled = false
    private var closed = false
    private var lastProgressAt = 0L

    private val idleClose = Runnable {
        if (up == null && down == null && ups.isEmpty() && downs.isEmpty()) closeSocket("idle")
    }
    private val reconnect = Runnable { startNext() }
    private val ping = object : Runnable {
        override fun run() {
            val socket = ws ?: return
            if (wsOpen) socket.send(PING)
            handler.postDelayed(this, PING_MS)
        }
    }
    private val peerTimeout = Runnable {
        if (ws != null && !peerPresent) {
            Log.w(TAG, "the host's file socket never showed up")
            failQueued(ERR_UNAVAILABLE)
            closeSocket("no file peer")
        }
    }
    private val ackTimer = Runnable { ackIfDue() }

    // ---- public API (any thread) ----------------------------------------------------------

    /** The host announced (or lost) `fileTransfer: 1`. Queued work waits while disabled. */
    fun setEnabled(value: Boolean) = handler.post {
        enabled = value
        if (value) startNext()
    }

    /**
     * Uploads [uris] as one batch. Unreadable or too-large items are reported
     * and left out of the batch before it starts.
     */
    fun upload(uris: List<Uri>, purpose: String) = handler.post {
        if (closed || uris.isEmpty()) return@post
        val batch = Batch(random.nextInt().toLong() and FileWire.U32_MAX, purpose, 0)
        val jobs = ArrayList<Upload>()
        for (uri in uris.take(FileWire.MAX_BATCH)) {
            val job = try { prepare(uri, purpose, batch, jobs.size) } catch (e: TooLarge) {
                fail(Direction.UP, e.name, ERR_TOO_LARGE); null
            } catch (e: Exception) {
                Log.w(TAG, "upload: unreadable $uri: ${e.message}")
                fail(Direction.UP, uri.lastPathSegment ?: FileNames.FALLBACK_UPLOAD, ERR_UNREADABLE); null
            }
            if (job != null) jobs += job
        }
        if (jobs.isEmpty()) return@post
        batch.count = jobs.size
        ups.addAll(jobs)
        startNext()
        progress(force = true)
    }

    /** Downloads every item of [offer] (each one REQUESTed in turn). */
    fun download(offer: FileWire.Offer) = handler.post {
        if (closed) return@post
        offer.items.forEach { downs.add(Download(offer.offerId, it)) }
        startNext()
        progress(force = true)
    }

    /**
     * A new signaling epoch (or none): the relay pairs file sockets per epoch,
     * so the old socket is dropped (failing what was in flight); queued work
     * goes out on a socket for the new epoch.
     */
    fun epochChanged() = handler.post {
        if (ws != null) socketLost("epoch changed", auth = false, counts = false)
    }

    /** Stops the active transfer in [direction] and everything queued behind it. */
    fun cancel(direction: Direction) = handler.post { cancelDirection(direction, ERR_CANCELLED, report = false) }

    /** The session ended: cancel everything and close the socket (no background transfers in v1). */
    fun cancelAll() = handler.post {
        cancelDirection(Direction.UP, ERR_CANCELLED, report = false)
        cancelDirection(Direction.DOWN, ERR_CANCELLED, report = false)
        closeSocket("session ended")
    }

    fun shutdown() {
        handler.post {
            closed = true
            cancelDirection(Direction.UP, ERR_CANCELLED, report = false)
            cancelDirection(Direction.DOWN, ERR_CANCELLED, report = false)
            closeSocket("shutdown")
            thread.quitSafely()
        }
    }

    // ---- preparing uploads -----------------------------------------------------------------

    private class TooLarge(val name: String) : Exception()

    private fun prepare(uri: Uri, purpose: String, batch: Batch, index: Int): Upload {
        var name: String? = null
        var size = -1L
        resolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE), null, null, null)?.use { c ->
            if (c.moveToFirst()) {
                c.getColumnIndex(OpenableColumns.DISPLAY_NAME).takeIf { it >= 0 && !c.isNull(it) }?.let { name = c.getString(it) }
                c.getColumnIndex(OpenableColumns.SIZE).takeIf { it >= 0 && !c.isNull(it) }?.let { size = c.getLong(it) }
            }
        }
        val mtime = try {
            resolver.query(uri, arrayOf(DocumentsContract.Document.COLUMN_LAST_MODIFIED), null, null, null)?.use { c ->
                if (c.moveToFirst() && !c.isNull(0)) c.getLong(0) else null
            }
        } catch (_: Exception) { null } ?: System.currentTimeMillis()
        val display = FileNames.forUpload(name ?: uri.lastPathSegment)
        if (size < 0) size = try {
            resolver.openAssetFileDescriptor(uri, "r")?.use { it.length } ?: AssetFileDescriptor.UNKNOWN_LENGTH
        } catch (_: Exception) { AssetFileDescriptor.UNKNOWN_LENGTH }
        var staged: File? = null
        if (size < 0) {
            // The provider can't tell the size up front: copy it into the cache
            // (bounded by the 2 GiB limit) so OFFER can carry the exact size.
            val file = File(app.cacheDir, "upload-${System.nanoTime()}")
            val input = resolver.openInputStream(uri) ?: throw IOException("no stream")
            var total = 0L
            try {
                input.use { inp ->
                    file.outputStream().use { out ->
                        val buf = ByteArray(FileWire.MAX_CHUNK)
                        while (true) {
                            val n = inp.read(buf)
                            if (n < 0) break
                            total += n
                            if (total > FileWire.MAX_FILE) throw TooLarge(display)
                            out.write(buf, 0, n)
                        }
                    }
                }
            } catch (e: Exception) { file.delete(); throw e }
            staged = file
            size = total
        }
        if (size > FileWire.MAX_FILE) { staged?.delete(); throw TooLarge(display) }
        val mime = FileNames.mime(display, resolver.getType(uri)) { MimeTypeMap.getSingleton().getMimeTypeFromExtension(it) }
        return Upload(uri, staged, display, size, mime, mtime, purpose, batch, index)
    }

    // ---- scheduling --------------------------------------------------------------------------

    private fun startNext() {
        if (closed || !enabled) return
        if (ups.isEmpty() && downs.isEmpty() && up == null && down == null) { armIdle(); return }
        val socket = ws
        if (socket == null) { openSocket(); return }
        if (!wsOpen || !peerPresent) return
        if (up == null) ups.pollFirst()?.let { startUpload(socket, it) }
        if (down == null) downs.pollFirst()?.let { startDownload(socket, it) }
        armIdle()
    }

    private fun armIdle() {
        handler.removeCallbacks(idleClose)
        if (ws != null && up == null && down == null && ups.isEmpty() && downs.isEmpty()) handler.postDelayed(idleClose, FileWire.IDLE_CLOSE_MS)
    }

    private fun openSocket() {
        val url = urlProvider()
        if (url == null) { failQueued(ERR_UNAVAILABLE); return }
        val gen = ++wsGen
        nextTid = 1
        val request = Request.Builder().url(url).apply { cookieProvider()?.let { header("Cookie", it) } }.build()
        Log.i(TAG, "opening file socket")
        ws = http.newWebSocket(request, object : WebSocketListener() {
            override fun onOpen(webSocket: WebSocket, response: Response) = on(gen) {
                wsOpen = true
                failures = 0
                webSocket.send(HELLO)
                handler.removeCallbacks(ping)
                handler.postDelayed(ping, PING_MS)
                handler.removeCallbacks(peerTimeout)
                handler.postDelayed(peerTimeout, PEER_WAIT_MS)
            }
            override fun onMessage(webSocket: WebSocket, bytes: ByteString) {
                val data = bytes.toByteArray()
                on(gen) { onPacket(data) }
            }
            override fun onMessage(webSocket: WebSocket, text: String) = on(gen) { onText(text) }
            override fun onClosing(webSocket: WebSocket, code: Int, reason: String) {
                webSocket.close(1000, null)
            }
            override fun onClosed(webSocket: WebSocket, code: Int, reason: String) = on(gen) { socketLost("closed $code", auth = false) }
            override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) = on(gen) {
                val code = response?.code ?: 0
                socketLost("failure ${t.javaClass.simpleName} $code", auth = code == 401 || code == 403)
            }
        })
    }

    private fun on(gen: Int, block: () -> Unit) {
        handler.post { if (gen == wsGen && !closed) block() }
    }

    private fun onText(text: String) {
        val msg = try { org.json.JSONObject(text) } catch (_: Exception) { return }
        when (msg.optString("type")) {
            "hello-ok" -> Log.i(TAG, "file socket ready")
            "file-peer" -> {
                peerPresent = msg.optBoolean("present")
                if (peerPresent) { handler.removeCallbacks(peerTimeout); startNext() }
            }
            "error" -> Log.w(TAG, "file relay error: ${msg.optString("message").take(120)}")
        }
    }

    private fun closeSocket(why: String) {
        handler.removeCallbacks(idleClose)
        handler.removeCallbacks(reconnect)
        handler.removeCallbacks(ping)
        handler.removeCallbacks(peerTimeout)
        peerPresent = false
        val socket = ws ?: return
        Log.i(TAG, "closing file socket: $why")
        wsGen++
        ws = null
        wsOpen = false
        try { socket.close(1000, why.take(64)) } catch (_: Exception) {}
    }

    private fun socketLost(why: String, auth: Boolean, counts: Boolean = true) {
        Log.w(TAG, "file socket lost: $why")
        val wasOpen = wsOpen
        val socket = ws
        wsGen++
        ws = null
        wsOpen = false
        peerPresent = false
        try { socket?.close(1000, null) } catch (_: Exception) {}
        handler.removeCallbacks(idleClose)
        handler.removeCallbacks(ping)
        handler.removeCallbacks(peerTimeout)
        up?.let { finishUp(it, ok = false, error = ERR_CONNECTION) }
        down?.let { finishDown(it, ok = false, error = ERR_CONNECTION) }
        if (!wasOpen && counts) failures++
        when {
            auth -> failQueued(ERR_AUTH)
            failures >= MAX_OPEN_FAILURES -> failQueued(ERR_CONNECTION)
            ups.isNotEmpty() || downs.isNotEmpty() -> handler.postDelayed(reconnect, RECONNECT_MS * failures.coerceAtLeast(1))
        }
        progress(force = true)
    }

    private fun failQueued(error: String) {
        while (true) { val job = ups.pollFirst() ?: break; finishBatchItem(job, false); job.staged?.delete(); fail(Direction.UP, job.name, error) }
        while (true) { val job = downs.pollFirst() ?: break; fail(Direction.DOWN, job.item.name, error) }
        failures = 0
        progress(force = true)
    }

    private fun cancelDirection(direction: Direction, error: String, report: Boolean) {
        val socket = ws
        if (direction == Direction.UP) {
            up?.job?.batch?.silent = !report
            ups.forEach { it.batch.silent = !report }
            up?.let { active ->
                socket?.send(FileWire.encodeJson(FileWire.CANCEL, active.tid, FileWire.cancelJson(error)).toByteString())
                finishUp(active, ok = false, error = error, report = report)
            }
            while (true) { val job = ups.pollFirst() ?: break; finishBatchItem(job, false); job.staged?.delete() }
        } else {
            down?.let { active ->
                socket?.send(FileWire.encodeJson(FileWire.CANCEL, active.tid, FileWire.cancelJson(error)).toByteString())
                finishDown(active, ok = false, error = error, report = report)
            }
            downs.clear()
        }
        progress(force = true)
        startNext()
    }

    private fun allocTid(): Long {
        val tid = nextTid
        nextTid = if (nextTid >= FileWire.U32_MAX) 1 else nextTid + 1
        return tid
    }

    // ---- upload ---------------------------------------------------------------------------------

    private fun startUpload(socket: WebSocket, job: Upload) {
        val active = ActiveUp(job, allocTid())
        up = active
        val meta = FileWire.Meta(job.name, job.size, job.mime, job.mtimeMs, job.purpose, job.batch.id, job.batchIndex, job.batch.count)
        send(socket, FileWire.encodeJson(FileWire.OFFER, active.tid, meta.toJson()))
        progress(force = true)
    }

    private fun pumpUpload() {
        val active = up ?: return
        val socket = ws ?: return
        if (!active.accepted || active.finishing) return
        val stream = active.stream ?: return
        val buffer = ByteArray(FileWire.MAX_CHUNK)
        while (active.window.canSend(FileWire.MAX_CHUNK) || active.window.room() >= active.job.size - active.window.sent) {
            val n = try { readUpTo(stream, buffer) } catch (e: IOException) {
                Log.w(TAG, "upload read failed: ${e.message}")
                abortUpload(active, ERR_UNREADABLE); return
            }
            if (n < 0) {
                if (active.window.sent != active.job.size) { abortUpload(active, ERR_SIZE_CHANGED); return }
                active.finishing = true
                send(socket, FileWire.encode(FileWire.DONE, active.tid, 0, active.digest.digest()))
                progress(force = true)
                return
            }
            if (active.window.sent + n > active.job.size) { abortUpload(active, ERR_SIZE_CHANGED); return }
            active.digest.update(buffer, 0, n)
            if (!send(socket, FileWire.encode(FileWire.DATA, active.tid, active.seq, buffer.copyOf(n)))) return
            active.seq++
            active.window.onSent(n)
            progress()
        }
    }

    /** Reads until [buffer] is full or the stream ends; -1 only at the end with nothing read. */
    private fun readUpTo(stream: InputStream, buffer: ByteArray): Int {
        var filled = 0
        while (filled < buffer.size) {
            val n = stream.read(buffer, filled, buffer.size - filled)
            if (n < 0) break
            filled += n
        }
        return if (filled == 0) -1 else filled
    }

    private fun abortUpload(active: ActiveUp, error: String) {
        ws?.send(FileWire.encodeJson(FileWire.CANCEL, active.tid, FileWire.cancelJson(error)).toByteString())
        finishUp(active, ok = false, error = error)
        startNext()
    }

    private fun finishUp(active: ActiveUp, ok: Boolean, error: String? = null, report: Boolean = true) {
        if (up !== active) return
        up = null
        try { active.stream?.close() } catch (_: IOException) {}
        active.job.staged?.delete()
        if (!ok && report) fail(Direction.UP, active.job.name, error ?: ERR_PROTOCOL)
        finishBatchItem(active.job, ok)
        progress(force = true)
    }

    private fun finishBatchItem(job: Upload, ok: Boolean) {
        val batch = job.batch
        batch.finished++
        if (ok) batch.saved++
        if (batch.finished == batch.count && !batch.silent) {
            val saved = batch.saved
            val failed = batch.count - saved
            main.post { listener.onUploadBatchDone(batch.purpose, saved, failed) }
        }
    }

    // ---- download ---------------------------------------------------------------------------------

    private fun startDownload(socket: WebSocket, job: Download) {
        val active = ActiveDown(job, allocTid())
        down = active
        send(socket, FileWire.encodeJson(FileWire.REQUEST, active.tid, FileWire.requestJson(job.offerId, job.item.id)))
        progress(force = true)
    }

    private fun abortDownload(active: ActiveDown, error: String) {
        ws?.send(FileWire.encodeJson(FileWire.CANCEL, active.tid, FileWire.cancelJson(error)).toByteString())
        finishDown(active, ok = false, error = error)
        startNext()
    }

    private fun finishDown(active: ActiveDown, ok: Boolean, error: String? = null, report: Boolean = true) {
        if (down !== active) return
        down = null
        handler.removeCallbacks(ackTimer)
        // Partial files never stay behind.
        if (!ok) active.target?.let { store.discard(it) }
        if (!ok && report) fail(Direction.DOWN, active.meta?.name ?: active.job.item.name, error ?: ERR_PROTOCOL)
        progress(force = true)
    }

    private fun sendAck(active: ActiveDown) {
        val total = active.acks.acked()
        ws?.let { send(it, FileWire.ack(active.tid, total)) }
    }

    private fun ackIfDue() {
        val active = down ?: return
        val now = SystemClock.uptimeMillis()
        if (active.acks.due(now)) sendAck(active) else scheduleAck(active)
    }

    private fun scheduleAck(active: ActiveDown) {
        handler.removeCallbacks(ackTimer)
        val at = active.acks.dueAt()
        if (at >= 0) handler.postAtTime(ackTimer, at)
    }

    // ---- inbound packets ------------------------------------------------------------------------------

    private fun onPacket(bytes: ByteArray) {
        val parsed = FileWire.parse(bytes)
        val packet = parsed.packet ?: return protocolError("bad packet: ${parsed.error}")
        handler.removeCallbacks(idleClose)
        val activeUp = up
        val activeDown = down
        when {
            activeUp != null && packet.transferId == activeUp.tid && packet.type != FileWire.REQUEST -> onUploadPacket(activeUp, packet)
            activeDown != null && packet.transferId == activeDown.tid -> onDownloadPacket(activeDown, packet)
            // Late packets for a transfer we already ended (a CANCEL crossing ours) are harmless.
            else -> Log.d(TAG, "ignoring packet for transfer ${packet.transferId}: type ${packet.type}")
        }
        armIdle()
    }

    private fun onUploadPacket(active: ActiveUp, packet: FileWire.Packet) {
        when (packet.type) {
            FileWire.ACCEPT -> {
                if (active.accepted) return abortUpload(active, ERR_PROTOCOL)
                active.accepted = true
                active.stream = try {
                    (active.job.staged?.inputStream() ?: resolver.openInputStream(active.job.source)) ?: throw IOException("no stream")
                } catch (e: Exception) {
                    Log.w(TAG, "upload open failed: ${e.message}")
                    return abortUpload(active, ERR_UNREADABLE)
                }
                pumpUpload()
            }
            FileWire.ACK -> {
                if (!active.accepted || !active.window.onAck(FileWire.ackTotal(packet))) return abortUpload(active, ERR_PROTOCOL)
                progress()
                pumpUpload()
            }
            FileWire.RESULT -> {
                val (ok, error) = FileWire.parseResult(packet.payload) ?: return abortUpload(active, ERR_PROTOCOL)
                if (ok && !active.finishing) return abortUpload(active, ERR_PROTOCOL)
                finishUp(active, ok, error ?: ERR_REFUSED)
                startNext()
            }
            FileWire.CANCEL -> {
                finishUp(active, ok = false, error = FileWire.parseCancel(packet.payload) ?: ERR_REFUSED)
                startNext()
            }
            else -> abortUpload(active, ERR_PROTOCOL)
        }
    }

    private fun onDownloadPacket(active: ActiveDown, packet: FileWire.Packet) {
        when (packet.type) {
            FileWire.OFFER -> {
                if (active.meta != null) return abortDownload(active, ERR_PROTOCOL)
                val meta = FileWire.parseMeta(packet.payload) ?: return abortDownload(active, ERR_PROTOCOL)
                active.meta = meta
                val name = FileNames.forDownload(meta.name)
                val mime = FileNames.mime(name, meta.mime) { MimeTypeMap.getSingleton().getMimeTypeFromExtension(it) }
                active.target = try { store.create(name, mime) } catch (e: Exception) {
                    Log.w(TAG, "download create failed: ${e.message}")
                    return abortDownload(active, ERR_STORAGE)
                }
                active.check = ReceiveCheck(meta.size)
                ws?.let { send(it, FileWire.encode(FileWire.ACCEPT, active.tid)) }
                progress(force = true)
            }
            FileWire.DATA -> {
                val check = active.check ?: return abortDownload(active, ERR_PROTOCOL)
                val target = active.target ?: return abortDownload(active, ERR_PROTOCOL)
                check.onData(packet.seq, packet.payload)?.let { return abortDownload(active, it) }
                try { target.stream.write(packet.payload) } catch (e: IOException) {
                    Log.w(TAG, "download write failed: ${e.message}")
                    return abortDownload(active, ERR_STORAGE)
                }
                if (active.acks.onData(packet.payload.size, SystemClock.uptimeMillis())) sendAck(active) else scheduleAck(active)
                progress()
            }
            FileWire.DONE -> {
                val check = active.check ?: return abortDownload(active, ERR_PROTOCOL)
                val target = active.target ?: return abortDownload(active, ERR_PROTOCOL)
                if (active.acks.received > active.acks.lastAcked) sendAck(active)
                val error = check.onDone(packet.payload)
                val socket = ws
                if (error != null) {
                    socket?.let { send(it, FileWire.encodeJson(FileWire.RESULT, active.tid, FileWire.resultJson(false, error))) }
                    finishDown(active, ok = false, error = error)
                    return startNext()
                }
                val published = try { store.publish(target) } catch (e: Exception) {
                    Log.w(TAG, "download publish failed: ${e.message}")
                    null
                }
                if (published == null) {
                    socket?.let { send(it, FileWire.encodeJson(FileWire.RESULT, active.tid, FileWire.resultJson(false, ERR_STORAGE))) }
                    finishDown(active, ok = false, error = ERR_STORAGE)
                    return startNext()
                }
                socket?.let { send(it, FileWire.encodeJson(FileWire.RESULT, active.tid, FileWire.resultJson(true))) }
                finishDown(active, ok = true)
                main.post { listener.onDownloaded(published.name, published.uri, published.mime) }
                startNext()
            }
            FileWire.CANCEL -> {
                finishDown(active, ok = false, error = FileWire.parseCancel(packet.payload) ?: ERR_REFUSED)
                startNext()
            }
            else -> abortDownload(active, ERR_PROTOCOL)
        }
    }

    private fun protocolError(why: String) {
        Log.w(TAG, "file socket protocol error: $why")
        up?.let { abortUpload(it, ERR_PROTOCOL) }
        down?.let { abortDownload(it, ERR_PROTOCOL) }
    }

    private fun send(socket: WebSocket, bytes: ByteArray): Boolean {
        if (socket.send(bytes.toByteString())) return true
        // OkHttp refuses once the socket is closing or its queue is full.
        socketLost("send refused", auth = false)
        return false
    }

    // ---- reporting -----------------------------------------------------------------------------------

    private fun fail(direction: Direction, name: String, error: String) {
        Log.w(TAG, "transfer failed ($direction) $name: $error")
        main.post { listener.onFailed(direction, name, error) }
    }

    private fun progress(force: Boolean = false) {
        val now = SystemClock.uptimeMillis()
        if (!force && now - lastProgressAt < PROGRESS_MS) return
        lastProgressAt = now
        val upRow = up?.let {
            val phase = when { !it.accepted -> Phase.WAITING; it.finishing -> Phase.VERIFYING; else -> Phase.ACTIVE }
            Row(Direction.UP, it.job.name, it.window.acked, it.job.size, phase, ups.size)
        } ?: ups.peekFirst()?.let { Row(Direction.UP, it.name, 0, it.size, Phase.WAITING, ups.size - 1) }
        val downRow = down?.let {
            val total = it.meta?.size ?: it.job.item.size
            val phase = if (it.meta == null) Phase.WAITING else Phase.ACTIVE
            Row(Direction.DOWN, it.meta?.name ?: it.job.item.name, it.check?.received ?: 0, total, phase, downs.size)
        } ?: downs.peekFirst()?.let { Row(Direction.DOWN, it.item.name, 0, it.item.size, Phase.WAITING, downs.size - 1) }
        main.post { listener.onProgress(upRow, downRow) }
    }

    companion object {
        private const val TAG = "BubblePlayFiles"
        private const val PROGRESS_MS = 100L
        private const val RECONNECT_MS = 1_000L
        private const val MAX_OPEN_FAILURES = 3
        private const val PING_MS = 15_000L
        private const val PEER_WAIT_MS = 10_000L
        private const val HELLO = "{\"type\":\"hello\",\"role\":\"file-client\"}"
        private const val PING = "{\"type\":\"ping\"}"

        const val ERR_CANCELLED = "cancelled"
        const val ERR_CONNECTION = "connection"
        const val ERR_AUTH = "auth"
        const val ERR_UNAVAILABLE = "unavailable"
        const val ERR_TOO_LARGE = "too-large"
        const val ERR_UNREADABLE = "unreadable"
        const val ERR_SIZE_CHANGED = "size-changed"
        const val ERR_STORAGE = "storage"
        const val ERR_PROTOCOL = "unexpected"
        const val ERR_REFUSED = "refused"
    }
}
