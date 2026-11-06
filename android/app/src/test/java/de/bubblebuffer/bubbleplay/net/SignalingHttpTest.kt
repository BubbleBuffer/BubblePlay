package de.bubblebuffer.bubbleplay.net

import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import okio.ByteString.Companion.encodeUtf8
import org.junit.Assert.*
import org.junit.Test
import java.net.ServerSocket
import java.net.Socket
import java.util.Base64
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicReference

/** Real sockets: the signaling client bounds the open plus upgrade, and never cuts an upgraded socket. */
class SignalingHttpTest {
    /** The client as Session builds it (10 s connect, no read timeout), with a short handshake limit. */
    private fun client(handshakeMs: Long) = OkHttpClient.Builder()
        .connectTimeout(10, TimeUnit.SECONDS)
        .readTimeout(0, TimeUnit.MILLISECONDS)
        .writeTimeout(10, TimeUnit.SECONDS)
        .build()
        .forSignaling(handshakeMs)

    private class Probe : WebSocketListener() {
        val opened = CountDownLatch(1)
        val failed = CountDownLatch(1)
        val message = CountDownLatch(1)
        val closed = CountDownLatch(1)
        val failure = AtomicReference<Throwable>()
        val response = AtomicReference<Response>()
        override fun onOpen(webSocket: WebSocket, response: Response) = opened.countDown()
        override fun onMessage(webSocket: WebSocket, text: String) = message.countDown()
        override fun onClosed(webSocket: WebSocket, code: Int, reason: String) = closed.countDown()
        override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
            failure.set(t); this.response.set(response); failed.countDown()
        }
    }

    private fun request(port: Int) = Request.Builder().url("http://127.0.0.1:$port/ws").build()

    private fun acceptKey(requestHead: String): String {
        val key = requestHead.lineSequence().first { it.startsWith("Sec-WebSocket-Key:", ignoreCase = true) }
            .substringAfter(':').trim()
        return Base64.getEncoder().encodeToString(
            java.security.MessageDigest.getInstance("SHA-1").digest((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").toByteArray()))
    }

    private fun readHead(s: Socket): String {
        val sb = StringBuilder()
        val input = s.getInputStream()
        while (!sb.endsWith("\r\n\r\n")) { val b = input.read(); if (b < 0) break; sb.append(b.toChar()) }
        return sb.toString()
    }

    @Test
    fun serverThatAcceptsButNeverAnswersTheUpgradeFailsWithinTheLimit() {
        ServerSocket(0).use { server ->
            val held = AtomicReference<Socket>()
            val t = Thread { held.set(server.accept()) }.apply { isDaemon = true; start() }
            val probe = Probe()
            val started = System.nanoTime()
            val socket = client(1_000).newWebSocket(request(server.localPort), probe)
            try {
                assertTrue("the open must fail on its own", probe.failed.await(5, TimeUnit.SECONDS))
                val elapsedMs = (System.nanoTime() - started) / 1_000_000
                assertTrue("failed after $elapsedMs ms, before the limit", elapsedMs >= 900)
                assertTrue("failed after $elapsedMs ms, long after the limit", elapsedMs < 3_000)
                // An ordinary connection failure: no HTTP response, so it is not a terminal refusal.
                assertNull(probe.response.get())
                assertNotNull(probe.failure.get())
                assertNull(SessionProblem.closed(probe.response.get()?.code, admitted = false))
                assertFalse(probe.opened.await(0, TimeUnit.MILLISECONDS))
            } finally {
                socket.cancel(); t.join(1_000); held.get()?.close()
            }
        }
    }

    @Test
    fun upgradedQuietSocketStaysOpenPastTheLimit() {
        ServerSocket(0).use { server ->
            val release = CountDownLatch(1)
            val t = Thread {
                server.accept().use { s ->
                    val head = readHead(s)
                    s.getOutputStream().apply {
                        write(("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n" +
                            "Sec-WebSocket-Accept: ${acceptKey(head)}\r\n\r\n").toByteArray())
                        flush()
                    }
                    release.await(10, TimeUnit.SECONDS)
                    // One unmasked text frame "hi" after the quiet period.
                    s.getOutputStream().apply { write(byteArrayOf(0x81.toByte(), 2) + "hi".encodeUtf8().toByteArray()); flush() }
                    Thread.sleep(500)
                }
            }.apply { isDaemon = true; start() }
            val probe = Probe()
            val socket = client(500).newWebSocket(request(server.localPort), probe)
            try {
                assertTrue(probe.opened.await(5, TimeUnit.SECONDS))
                // Well past the 500 ms limit, and past a connect/read timeout that was applied by mistake.
                assertFalse("a quiet upgraded socket must not be cut", probe.failed.await(2_000, TimeUnit.MILLISECONDS))
                assertFalse(probe.closed.await(0, TimeUnit.MILLISECONDS))
                release.countDown()
                assertTrue("the socket still carries data", probe.message.await(5, TimeUnit.SECONDS))
            } finally {
                release.countDown(); socket.cancel(); t.join(2_000)
            }
        }
    }

    @Test
    fun limitIsTenSecondsLikeTheWindowsClient() {
        assertEquals(10_000L, SIGNALING_HANDSHAKE_MS)
        assertEquals(10_000, OkHttpClient().forSignaling().callTimeoutMillis)
    }
}
