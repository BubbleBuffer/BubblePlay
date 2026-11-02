package de.bubblebuffer.bubbleplay.net

import de.bubblebuffer.bubbleplay.auth.AuthCrypto
import de.bubblebuffer.bubbleplay.auth.Handshake
import de.bubblebuffer.bubbleplay.auth.PairAdmit
import de.bubblebuffer.bubbleplay.auth.PairLink
import de.bubblebuffer.bubbleplay.auth.Pairing
import de.bubblebuffer.bubbleplay.auth.RelayProof
import de.bubblebuffer.bubbleplay.auth.b64
import de.bubblebuffer.bubbleplay.auth.fixtureRandom
import de.bubblebuffer.bubbleplay.auth.handshakeVectors
import de.bubblebuffer.bubbleplay.auth.pairingVectors
import de.bubblebuffer.bubbleplay.auth.pcId
import de.bubblebuffer.bubbleplay.auth.pcKey
import de.bubblebuffer.bubbleplay.auth.viewerKey
import de.bubblebuffer.bubbleplay.keys.Ed25519Verify
import de.bubblebuffer.bubbleplay.log.allText
import de.bubblebuffer.bubbleplay.log.logLines
import de.bubblebuffer.bubbleplay.log.withTestLog
import de.bubblebuffer.bubbleplay.pcs.PcEntry
import okhttp3.Protocol
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import okio.ByteString
import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Test
import java.io.IOException
import java.util.concurrent.AbstractExecutorService
import java.util.concurrent.Callable
import java.util.concurrent.Delayed
import java.util.concurrent.Executor
import java.util.concurrent.ScheduledExecutorService
import java.util.concurrent.ScheduledFuture
import java.util.concurrent.TimeUnit

class PairingClientTest {
    @Test
    fun aRefusalIsLoggedWithItsCauseAndNeverTheCode() = withTestLog { log, dir ->
        Script().use { s ->
            s.admit()
            s.wire.listener.onClosing(s.wire, 1008, "pairing window closed")
            log.flush()
            val lines = dir.logLines()
            val failed = lines.single { it.getString("msg") == "pairing failed" }
            assertEquals("warn", failed.getString("level"))
            assertEquals(s.link.pcId, failed.getString("pc"))
            val fields = failed.getJSONObject("fields")
            assertEquals("Refused", fields.getString("failure"))
            assertEquals(1008, fields.getInt("wsClose"))
            assertEquals("pairing window closed", fields.getString("closeReason"))
            assertEquals("AuthHost", fields.getString("state"))
            assertTrue(fields.getBoolean("admitted"))
            assertTrue(lines.any { it.getString("msg") == "pairing started" })
            assertTrue(lines.any { it.getString("msg") == "admitted by the relay in pairing mode" })
            assertFalse(dir.allText().contains(s.link.code))
        }
    }

    @Test
    fun theWholePairingIsLoggedStepByStepWithoutTheCodeOrTheNumber() = withTestLog { log, dir ->
        Script().use { s ->
            s.admit()
            s.prove()
            s.wire.receive("""{"type":"pair-done"}""")
            log.flush()
            val messages = dir.logLines().map { it.getString("msg") }
            for (step in listOf(
                "pairing started", "relay socket open", "relay challenge answered",
                "admitted by the relay in pairing mode", "auth-host verified", "PC pair proof verified",
                "comparison number shown, waiting for the PC owner", "pairing allowed by the PC",
                "pairing complete, handing the PC to the caller",
            )) assertTrue("missing step: $step in $messages", step in messages)
            val text = dir.allText()
            assertFalse(text.contains(s.link.code))
            assertFalse(text.contains(Pairing.sasNumber(s.hash)))
            assertFalse(text.contains(Pairing.sasDisplay(Pairing.sasNumber(s.hash))))
        }
    }

    @Test
    fun aTimeoutAndASocketFailureAreLoggedWithTheirCause() = withTestLog { log, dir ->
        Script().use { s ->
            s.open()
            s.timer.latestTimeout().fire()
            log.flush()
            val failed = dir.logLines().single { it.getString("msg") == "pairing failed" }.getJSONObject("fields")
            assertEquals("Timeout", failed.getString("failure"))
            assertTrue(failed.getString("why").startsWith("no answer in state Challenge"))
        }
        Script().use { s ->
            s.open()
            s.wire.fail()
            log.flush()
            val failed = dir.logLines().last { it.getString("msg") == "pairing failed" }.getJSONObject("fields")
            assertEquals("ConnectionLost", failed.getString("failure"))
            assertEquals("socket failure", failed.getString("why"))
            assertEquals("java.io.IOException: offline", failed.getString("error"))
        }
    }

    @Test
    fun signedRelayAdmissionThenPairingPinsVerifiedPc() = Script().use { s ->
        s.admit()
        assertEquals("/ws", s.wire.request.url.encodedPath)
        assertEquals(s.link.pcId, s.wire.request.url.queryParameter("pc"))
        assertNull(s.wire.request.header("Cookie"))
        assertNull(s.wire.request.header("Origin"))
        val hello = s.wire.sent.json(0)
        assertEquals(setOf("type", "role", "id", "public", "pair"), hello.keys().asSequence().toSet())
        assertEquals("viewer", hello.getString("role"))
        assertEquals(s.key.id, hello.getString("id"))
        assertEquals(PairAdmit.hash(s.link.pcId, s.link.code), hello.getString("pair"))
        assertTrue(RelayProof.verify(
            "viewer", s.link.relay, s.challenge, s.key.id, s.key.publicKey,
            b64(s.wire.sent.json(1).getString("signature")),
        ))
        assertEquals(listOf("control-aead-v1", "pair-v1"),
            s.wire.sent.json(2).getJSONArray("caps").let { caps -> (0 until caps.length()).map(caps::getString) })

        s.prove()
        assertEquals(listOf("hello", "register-proof", "auth-hello", "auth-finish", "pair-proof"), s.wire.types())
        val finish = s.wire.sent.json(3)
        assertTrue(Ed25519Verify.verify(s.key.publicKey,
            Handshake.viewerSignedBytes(s.hash, s.hostSignature), b64(finish.getString("signature"))))
        assertArrayEquals(Pairing.viewerMac(Pairing.pairKey(s.link.code), s.hash),
            b64(s.wire.sent.json(4).getString("mac")))
        assertEquals(listOf("stage:Connecting", "stage:Proving", "stage:WaitingForPc",
            "number:${Pairing.sasDisplay(Pairing.sasNumber(s.hash))}"), s.events)
        s.wire.receive("""{"type":"pair-done"}""")
        val paired = s.paired!!
        assertEquals(s.link.pcId, paired.id)
        assertEquals(b64(s.pc.publicKey), paired.publicKey)
        assertEquals(s.link.relay, paired.relay)
        assertEquals("Desk", paired.name)
        assertEquals(123456L, paired.paired)
        assertEquals(0L, paired.lastSeen)
        assertEquals("paired", s.events.last())
        assertEquals(listOf(1000), s.wire.closeCodes)
        assertTrue(s.timer.tasks.all { it.isCancelled() })
        s.wire.receive("""{"type":"pair-denied"}""")
        s.wire.fail()
        assertEquals(1, s.terminalEvents().size)
    }

    @Test
    fun deniedBeforeOrAfterPcProofIsTerminal() {
        for (showNumber in listOf(false, true)) Script().use { s ->
            s.admit()
            s.host()
            if (showNumber) s.pcProof()
            s.wire.receive("""{"type":"pair-denied"}""")
            assertEquals(listOf("denied"), s.terminalEvents())
            assertEquals(showNumber, s.events.any { it.startsWith("number:") })
            s.wire.closed(1008)
            assertEquals(1, s.terminalEvents().size)
        }
    }

    @Test
    fun badMacAndBadPcSignatureFailWithoutComparisonNumber() {
        Script().use { s ->
            s.admit()
            s.host()
            s.pcProof(ByteArray(32))
            assertEquals(listOf("failed:ProofMismatch"), s.terminalEvents())
            assertFalse(s.events.any { it.startsWith("number:") })
        }
        Script().use { s ->
            s.admit()
            s.host(badSignature = true)
            assertEquals(listOf("failed:PcKeyInvalid"), s.terminalEvents())
            assertFalse(s.events.any { it.startsWith("number:") })
            assertFalse("auth-finish" in s.wire.types())
        }
    }

    @Test
    fun normalHelloOkCannotFinishPairingAndBadChallengeIsRefused() {
        Script().use { s ->
            s.open()
            s.answerChallenge()
            s.wire.receive(s.helloOk().apply { remove("pairing"); put("ticket", b64(ByteArray(32))) }.toString())
            assertEquals(listOf("failed:Refused"), s.terminalEvents())
            assertFalse("auth-hello" in s.wire.types())
        }
        Script().use { s ->
            s.open()
            s.wire.receive("""{"type":"register-challenge","nonce":"bad"}""")
            assertEquals(listOf("failed:Refused"), s.terminalEvents())
        }
        Script().use { s ->
            s.open()
            s.answerChallenge()
            s.answerChallenge()
            assertEquals(listOf("failed:Refused"), s.terminalEvents())
        }
    }

    @Test
    fun policyAndPreAdmissionClosesAreRefusedWhileOtherDropsAreConnectionLost() {
        Script().use { s ->
            s.open()
            s.wire.closed(1008)
            assertEquals(listOf("failed:Refused"), s.terminalEvents())
        }
        Script().use { s ->
            s.admit()
            s.wire.closed(1008)
            assertEquals(listOf("failed:Refused"), s.terminalEvents())
        }
        Script().use { s ->
            s.open()
            s.wire.closed(1000)
            assertEquals(listOf("failed:Refused"), s.terminalEvents())
        }
        Script().use { s ->
            s.admit()
            s.wire.closed(1001)
            assertEquals(listOf("failed:ConnectionLost"), s.terminalEvents())
        }
        Script().use { s ->
            s.open()
            s.wire.fail()
            assertEquals(listOf("failed:ConnectionLost"), s.terminalEvents())
        }
        Script().use { s ->
            s.admit()
            s.wire.acceptSend = false
            s.host()
            assertEquals(listOf("failed:ConnectionLost"), s.terminalEvents())
        }
    }

    @Test
    fun missingHostAndPcDecisionHaveSeparateDeadlinesAndStaleTimersDoNothing() {
        Script().use { s ->
            s.admit()
            assertEquals(10_000L, s.timer.latestTimeout().delayMs)
            s.timer.latestTimeout().fire()
            assertEquals(listOf("failed:Timeout"), s.terminalEvents())
            s.host()
            assertFalse(s.events.any { it.startsWith("number:") })
        }
        Script().use { s ->
            s.admit()
            val authDeadline = s.timer.latestTimeout()
            s.host()
            val proofDeadline = s.timer.latestTimeout()
            s.pcProof()
            assertEquals(300_000L, s.timer.latestTimeout().delayMs)
            authDeadline.fireEvenIfCancelled()
            proofDeadline.fireEvenIfCancelled()
            assertTrue(s.terminalEvents().isEmpty())
            val heartbeat = s.timer.tasks.single { it.periodic }
            assertEquals(30_000L, heartbeat.delayMs)
            heartbeat.fire()
            assertEquals("ping", s.wire.types().last())
            s.wire.receive("""{"type":"pong"}""")
            assertEquals(300_000L, s.timer.latestTimeout().delayMs)
            s.timer.latestTimeout().fire()
            assertEquals(listOf("failed:Timeout"), s.terminalEvents())
        }
        Script().use { s ->
            s.open()
            s.timer.latestTimeout().fire()
            assertEquals(listOf("failed:Timeout"), s.terminalEvents())
        }
    }

    @Test
    fun outOfOrderPairDoneNeverPinsPcOrShowsANumber() = Script().use { s ->
        s.admit()
        s.host()
        s.wire.receive("""{"type":"pair-done"}""")
        assertEquals(listOf("failed:ConnectionLost"), s.terminalEvents())
        assertNull(s.paired)
        assertFalse(s.events.any { it.startsWith("number:") })
    }

    @Test
    fun cancelIsQuietAndSuppressesQueuedCallbacksAndLateSocketEvents() {
        val queue = QueuedExecutor()
        Script(queue).use { s ->
            s.admit()
            s.prove()
            assertTrue(s.events.isEmpty())
            s.client.cancel()
            queue.drain()
            s.wire.receive("""{"type":"pair-done"}""")
            s.wire.fail()
            s.timer.tasks.forEach { it.fireEvenIfCancelled() }
            queue.drain()
            assertTrue(s.events.isEmpty())
            assertTrue(s.wire.cancelled)
        }
        Script().use { s ->
            s.client.cancel()
            s.client.start()
            assertEquals(0, s.wire.opens)
            assertTrue(s.events.isEmpty())
        }
    }

    @Test
    fun callbacksUseTheInjectedExecutorAndStartIsIdempotent() {
        val queue = QueuedExecutor()
        Script(queue).use { s ->
            s.admit()
            s.prove()
            s.wire.receive("""{"type":"pair-done"}""")
            s.client.start()
            assertTrue(s.events.isEmpty())
            queue.drain()
            assertEquals(listOf("stage:Connecting", "stage:Proving", "stage:WaitingForPc",
                "number:${Pairing.sasDisplay(Pairing.sasNumber(s.hash))}", "paired"), s.events)
            assertEquals(1, s.wire.opens)
        }
    }

    private class Script(callbacks: Executor = Executor { it.run() }) : AutoCloseable {
        val key = viewerKey()
        val pc = pcKey()
        val link = PairLink(handshakeVectors.getString("relayOrigin"), pcId(), pairingVectors.getString("code"))
        val epoch = "0123456789abcdef0123456789abcdef"
        val challenge = ByteArray(32) { it.toByte() }
        val wire = ScriptedSocket()
        val timer = ManualScheduler()
        val events = mutableListOf<String>()
        var paired: PcEntry? = null
        lateinit var hash: ByteArray
        lateinit var hostSignature: ByteArray
        val client = PairingClient(key, link, "Desk", object : PairingListener {
            override fun onStage(stage: PairingStage) { events += "stage:$stage" }
            override fun onNumber(display: String) { events += "number:$display" }
            override fun onPaired(pc: PcEntry) { paired = pc; events += "paired" }
            override fun onDenied() { events += "denied" }
            override fun onFailed(reason: PairingFailure) { events += "failed:$reason" }
        }, wire, callbacks, timer, fixtureRandom(), { 123456L })

        fun open() { client.start(); wire.open() }
        fun answerChallenge() {
            wire.receive(JSONObject().put("type", "register-challenge").put("nonce", b64(challenge)).toString())
        }
        fun helloOk() = JSONObject().put("type", "hello-ok").put("pc", link.pcId)
            .put("mediaEpoch", epoch).put("iceServers", org.json.JSONArray()).put("pairing", true)
        fun admit() { open(); answerChallenge(); wire.receive(helloOk().toString()) }
        fun host(badSignature: Boolean = false) {
            val viewer = wire.sent.json(2)
            val host = handshakeVectors.getJSONObject("pc")
            val nonce = b64(host.getString("nonce"))
            val ephemeral = b64(host.getString("ephemeral"))
            hash = AuthCrypto.sha256(AuthCrypto.lengthPrefixed(
                AuthCrypto.utf8("bubbleplay-auth-v1"), AuthCrypto.utf8(link.relay), AuthCrypto.utf8(epoch),
                AuthCrypto.utf8(key.id), key.publicKey, AuthCrypto.utf8(pc.id), pc.publicKey,
                b64(viewer.getString("nonce")), nonce, b64(viewer.getString("ephemeral")), ephemeral,
            ))
            hostSignature = pc.sign(Handshake.pcSignedBytes(hash))
            wire.receive(JSONObject().put("type", "auth-host").put("id", pc.id).put("public", b64(pc.publicKey))
                .put("nonce", b64(nonce)).put("ephemeral", b64(ephemeral))
                .put("signature", b64(if (badSignature) ByteArray(64) else hostSignature)).toString())
        }
        fun pcProof(mac: ByteArray = Pairing.pcMac(Pairing.pairKey(link.code), hash)) {
            wire.receive(JSONObject().put("type", "pair-proof").put("mac", b64(mac)).toString())
        }
        fun prove() { host(); pcProof() }
        fun terminalEvents() = events.filter { it == "paired" || it == "denied" || it.startsWith("failed:") }
        override fun close() { client.cancel(); key.close(); pc.close() }
    }

    private class ScriptedSocket : WebSocket, WebSocket.Factory {
        lateinit var request: Request
        lateinit var listener: WebSocketListener
        val sent = mutableListOf<String>()
        val closeCodes = mutableListOf<Int>()
        var cancelled = false
        var acceptSend = true
        var opens = 0
        override fun newWebSocket(request: Request, listener: WebSocketListener): WebSocket {
            opens++
            this.request = request
            this.listener = listener
            return this
        }
        fun open() = listener.onOpen(this, Response.Builder().request(request).protocol(Protocol.HTTP_1_1)
            .code(101).message("Switching Protocols").build())
        fun receive(text: String) = listener.onMessage(this, text)
        fun closed(code: Int) = listener.onClosed(this, code, "")
        fun fail() = listener.onFailure(this, IOException("offline"), null)
        fun types() = sent.map { JSONObject(it).getString("type") }
        override fun request() = request
        override fun queueSize() = 0L
        override fun send(text: String): Boolean { if (acceptSend) sent += text; return acceptSend }
        override fun send(bytes: ByteString) = false
        override fun close(code: Int, reason: String?): Boolean { closeCodes += code; return true }
        override fun cancel() { cancelled = true }
    }

    private class QueuedExecutor : Executor {
        private val queue = ArrayDeque<Runnable>()
        override fun execute(command: Runnable) { queue.addLast(command) }
        fun drain() { while (queue.isNotEmpty()) queue.removeFirst().run() }
    }

    /** No threads or wall-clock sleeps: cancelled tasks can be fired to model timer races. */
    private class ManualScheduler : AbstractExecutorService(), ScheduledExecutorService {
        val tasks = mutableListOf<Task>()
        private var stopped = false
        fun latestTimeout() = tasks.last { !it.periodic }
        override fun schedule(command: Runnable, delay: Long, unit: TimeUnit): ScheduledFuture<*> =
            Task(command, unit.toMillis(delay), false).also(tasks::add)
        override fun scheduleWithFixedDelay(command: Runnable, initialDelay: Long, delay: Long, unit: TimeUnit): ScheduledFuture<*> =
            Task(command, unit.toMillis(initialDelay), true).also(tasks::add)
        override fun <V> schedule(callable: Callable<V>, delay: Long, unit: TimeUnit): ScheduledFuture<V> =
            throw UnsupportedOperationException()
        override fun scheduleAtFixedRate(command: Runnable, initialDelay: Long, period: Long, unit: TimeUnit): ScheduledFuture<*> =
            throw UnsupportedOperationException()
        override fun execute(command: Runnable) = command.run()
        override fun shutdown() { stopped = true }
        override fun shutdownNow(): MutableList<Runnable> { stopped = true; return mutableListOf() }
        override fun isShutdown() = stopped
        override fun isTerminated() = stopped
        override fun awaitTermination(timeout: Long, unit: TimeUnit) = stopped

        class Task(private val command: Runnable, val delayMs: Long, val periodic: Boolean) : ScheduledFuture<Unit> {
            private var cancelled = false
            fun fire() { if (!cancelled) command.run() }
            fun fireEvenIfCancelled() = command.run()
            override fun cancel(mayInterruptIfRunning: Boolean): Boolean { cancelled = true; return true }
            override fun isCancelled() = cancelled
            override fun isDone() = cancelled
            override fun get() = Unit
            override fun get(timeout: Long, unit: TimeUnit) = Unit
            override fun getDelay(unit: TimeUnit) = unit.convert(delayMs, TimeUnit.MILLISECONDS)
            override fun compareTo(other: Delayed) = getDelay(TimeUnit.MILLISECONDS).compareTo(other.getDelay(TimeUnit.MILLISECONDS))
        }
    }
}

private fun List<String>.json(index: Int) = JSONObject(this[index])
