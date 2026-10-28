package de.bubblebuffer.bubbleplay.e2e

import de.bubblebuffer.bubbleplay.auth.AuthCrypto
import de.bubblebuffer.bubbleplay.auth.PairLink
import de.bubblebuffer.bubbleplay.auth.Pairing
import de.bubblebuffer.bubbleplay.auth.pcKey
import de.bubblebuffer.bubbleplay.auth.viewerKey
import de.bubblebuffer.bubbleplay.keys.StrictBase64Url
import de.bubblebuffer.bubbleplay.net.PairingClient
import de.bubblebuffer.bubbleplay.net.PairingFailure
import de.bubblebuffer.bubbleplay.net.PairingListener
import de.bubblebuffer.bubbleplay.net.PairingStage
import de.bubblebuffer.bubbleplay.net.SessionWire
import de.bubblebuffer.bubbleplay.pcs.PcEntry
import okhttp3.OkHttpClient
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import java.util.concurrent.Executor
import java.util.concurrent.Executors
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit

/** The real PairingClient against the real relay and a scripted PC (pairing at the PC, decisions 1-5, 17). */
class PairingE2eTest {
    @get:Rule val relay = RelayServer()

    private val http = OkHttpClient.Builder().readTimeout(0, TimeUnit.MILLISECONDS).build()
    private val scheduler = Executors.newSingleThreadScheduledExecutor { Thread(it, "e2e-pairing-timer").apply { isDaemon = true } }
    private val closers = mutableListOf<AutoCloseable>()
    private val events = LinkedBlockingQueue<Any>()
    private var client: PairingClient? = null

    data class Stage(val stage: PairingStage)
    data class SasNumber(val display: String)
    data class Paired(val pc: PcEntry)
    object Denied
    data class Failed(val reason: PairingFailure)

    private val listener = object : PairingListener {
        override fun onStage(stage: PairingStage) { events.add(Stage(stage)) }
        override fun onNumber(display: String) { events.add(SasNumber(display)) }
        override fun onPaired(pc: PcEntry) { events.add(Paired(pc)) }
        override fun onDenied() { events.add(Denied) }
        override fun onFailed(reason: PairingFailure) { events.add(Failed(reason)) }
    }

    /** Test callback executor: listener calls run on the socket thread, in order. */
    private val direct = Executor { it.run() }

    @After fun tearDown() {
        client?.cancel()
        closers.asReversed().forEach { runCatching { it.close() } }
        scheduler.shutdownNow()
        http.dispatcher.executorService.shutdown()
        http.connectionPool.evictAll()
    }

    private fun <T : AutoCloseable> T.closing(): T = also { closers.add(it) }

    private fun startPairing(pc: FakePc, code: String) {
        val link = PairLink(relay.origin, pc.id, code)
        assertEquals("the link round-trips", link, PairLink.parse(link.toUri()))
        client = PairingClient(viewerKey(), link, PC_NAME, listener, http, direct, scheduler).also { it.start() }
        assertEquals(Stage(PairingStage.Connecting), next())
    }

    private fun next(timeoutMs: Long = FakePc.WAIT_MS): Any =
        events.poll(timeoutMs, TimeUnit.MILLISECONDS) ?: die("viewer: no pairing callback within $timeoutMs ms")

    /** PC with an open window for [CODE]; the viewer pairs up to the shown number. Returns the PC's handshake. */
    private fun pairUpToNumber(pc: FakePc): PcHandshake {
        pc.register()
        pc.admit()
        pc.pairOpen(CODE)
        startPairing(pc, CODE)
        val peer = pc.expectPeerJoined()
        assertTrue(peer.pairing)
        assertEquals(Stage(PairingStage.Proving), next())
        val handshake = pc.answerAuthHello(peer)
        assertTrue(handshake.viewerCaps.contains(Pairing.CAP))
        handshake.awaitAuthFinish()
        handshake.exchangePairProofs(CODE)
        assertEquals(Stage(PairingStage.WaitingForPc), next())
        val number = next() as? SasNumber ?: die("expected the comparison number")
        // Both sides compute the same comparison number.
        assertEquals(handshake.sasDisplay, number.display)
        return handshake
    }

    @Test fun pairsThenThePcAdmitsTheViewer() {
        val pc = FakePc(relay, http, pcKey()).closing()
        pairUpToNumber(pc)

        pc.send("{\"type\":\"pair-done\"}")
        val paired = next() as? Paired ?: die("expected onPaired")
        assertEquals(pc.id, paired.pc.id)
        assertEquals(AuthCrypto.b64(pc.key.publicKey), paired.pc.publicKey)
        assertEquals(relay.origin, paired.pc.relay)
        assertEquals(PC_NAME, paired.pc.name)

        // The PC adds the key to its trusted list and re-sends admit; the stored entry now connects normally.
        val viewerId = viewerKey().id
        pc.admit(viewerId)
        val viewer = ViewerWire(relay, http, viewerKey(), paired.pc.id,
            StrictBase64Url.decode(paired.pc.publicKey, 32), SessionE2eTest.DEVICE_NAME).closing()
        viewer.start()
        val peer = pc.expectPeerJoined()
        assertFalse(peer.pairing)
        val admitted = viewer.expect<SessionWire.Event.Admitted>()
        assertFalse(admitted.hello.pairing)
        assertNotNull(admitted.hello.ticket)
        val handshake = pc.answerAuthHello(peer)
        assertFalse(handshake.viewerCaps.contains(Pairing.CAP))
        handshake.awaitAuthFinish()
        viewer.expect<SessionWire.Event.Authenticated>()
        assertEquals("caps", handshake.receiveCtl().kind)
    }

    @Test fun wrongCodeIsRefused() {
        val pc = FakePc(relay, http, pcKey()).closing()
        pc.register()
        pc.admit()
        pc.pairOpen(CODE)
        startPairing(pc, OTHER_CODE)
        assertEquals(Failed(PairingFailure.Refused), next())
        pc.expectQuiet(300)
    }

    @Test fun pairDeniedReportsDenied() {
        val pc = FakePc(relay, http, pcKey()).closing()
        pairUpToNumber(pc)
        pc.send("{\"type\":\"pair-denied\"}")
        assertEquals(Denied, next())
    }

    @Test fun wrongPcProofFailsWithoutANumber() {
        val pc = FakePc(relay, http, pcKey()).closing()
        pc.register()
        pc.admit()
        pc.pairOpen(CODE)
        startPairing(pc, CODE)
        val peer = pc.expectPeerJoined()
        assertEquals(Stage(PairingStage.Proving), next())
        val handshake = pc.answerAuthHello(peer)
        handshake.awaitAuthFinish()
        handshake.exchangePairProofs(CODE, wrongMac = true)
        // ProofMismatch comes straight after Proving: no WaitingForPc, no number.
        assertEquals(Failed(PairingFailure.ProofMismatch), next())
        assertEquals(null, events.poll(300, TimeUnit.MILLISECONDS))
    }

    companion object {
        const val PC_NAME = "Office PC"
        const val CODE = "7K3QW9X2MD"
        const val OTHER_CODE = "7K3QW9X2ME"
    }
}
