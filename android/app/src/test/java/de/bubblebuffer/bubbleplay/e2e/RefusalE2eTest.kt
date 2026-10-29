package de.bubblebuffer.bubbleplay.e2e

import de.bubblebuffer.bubbleplay.auth.AuthJson
import de.bubblebuffer.bubbleplay.auth.ViewerAuth
import de.bubblebuffer.bubbleplay.auth.keyFromSeed
import de.bubblebuffer.bubbleplay.auth.pcKey
import de.bubblebuffer.bubbleplay.auth.viewerKey
import de.bubblebuffer.bubbleplay.net.SessionProblem
import de.bubblebuffer.bubbleplay.net.SessionWire
import okhttp3.OkHttpClient
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import java.util.concurrent.TimeUnit

/** Refusals end the session with the outcome Session shows, against the real relay. */
class RefusalE2eTest {
    @get:Rule val relay = RelayServer()

    private val http = OkHttpClient.Builder().readTimeout(0, TimeUnit.MILLISECONDS).build()
    private val closers = mutableListOf<AutoCloseable>()

    @After fun tearDown() {
        closers.asReversed().forEach { runCatching { it.close() } }
        http.dispatcher.executorService.shutdown()
        http.connectionPool.evictAll()
    }

    private fun <T : AutoCloseable> T.closing(): T = also { closers.add(it) }

    private fun viewerFor(pc: FakePc) =
        ViewerWire(relay, http, viewerKey(), pc.id, pc.key.publicKey, SessionE2eTest.DEVICE_NAME).closing()

    @Test fun viewerNotOnTheAdmitListIsRefusedWithoutReconnecting() {
        val pc = FakePc(relay, http, pcKey()).closing()
        pc.register()
        pc.admit(keyFromSeed("11".repeat(32)).id) // someone else

        val viewer = viewerFor(pc)
        viewer.start()
        val closed = viewer.expect<ViewerWire.Closed>()
        assertEquals(1008, closed.code)
        assertFalse(viewer.wire.admitted)
        // Non-null: Session.signalingDown calls endSession (terminal, no reconnect) with this text.
        assertEquals(SessionProblem.RELAY_REFUSED, closed.problem)
        assertTrue(viewer.received.none { it.contains("\"hello-ok\"") })
        // The PC never hears about a refused viewer.
        pc.expectQuiet(300)
    }

    @Test fun pcAnsweringWithAnotherKeyIsPcKeyChanged() {
        val pc = FakePc(relay, http, pcKey()).closing()
        pc.register()
        pc.admit(viewerKey().id)

        val viewer = viewerFor(pc)
        viewer.start()
        val peer = pc.expectPeerJoined()
        viewer.expect<SessionWire.Event.Admitted>()
        val other = keyFromSeed("22".repeat(32))
        assertNotEquals(pc.id, other.id)
        pc.answerAuthHello(peer, signer = other)
        val failed = viewer.expect<SessionWire.Event.Failed>()
        assertEquals(SessionProblem.PC_KEY_CHANGED, failed.problem)
        // No auth-finish and no ctl reach the PC (the viewer's departure may show up as peer-left).
        Thread.sleep(300)
        pc.sync()
        val pcTypes = pc.received.map { AuthJson.parseObject(it)["type"] }
        assertFalse(pcTypes.toString(), pcTypes.contains("auth-finish") || pcTypes.contains("ctl"))
    }

    @Test(timeout = 15_000)
    fun pcThatNeverAnswersTimesOutAfterTheAuthHostLimit() {
        val pc = FakePc(relay, http, pcKey()).closing()
        pc.register()
        pc.admit(viewerKey().id)

        val viewer = viewerFor(pc)
        viewer.start()
        pc.expectPeerJoined()
        viewer.expect<SessionWire.Event.Admitted>()
        val admittedAt = System.nanoTime()
        pc.expect("auth-hello") // and never answer
        val failed = viewer.expect<SessionWire.Event.Failed>(ViewerAuth.AUTH_HOST_TIMEOUT_MS + 3_000)
        val waitedMs = TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - admittedAt)
        assertEquals(SessionProblem.AUTH_TIMEOUT, failed.problem)
        assertTrue("timed out after $waitedMs ms", waitedMs >= ViewerAuth.AUTH_HOST_TIMEOUT_MS - 500)
        assertTrue("timed out after $waitedMs ms", waitedMs <= ViewerAuth.AUTH_HOST_TIMEOUT_MS + 3_000)
    }
}
