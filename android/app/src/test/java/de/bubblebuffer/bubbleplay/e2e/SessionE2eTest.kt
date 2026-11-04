package de.bubblebuffer.bubbleplay.e2e

import de.bubblebuffer.bubbleplay.auth.AuthJson
import de.bubblebuffer.bubbleplay.auth.CtlChannel
import de.bubblebuffer.bubbleplay.auth.pcKey
import de.bubblebuffer.bubbleplay.auth.viewerKey
import de.bubblebuffer.bubbleplay.log.CtlPassThrough
import de.bubblebuffer.bubbleplay.net.MonitorPreviews
import de.bubblebuffer.bubbleplay.net.SessionWire
import okhttp3.OkHttpClient
import org.junit.After
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import java.util.concurrent.TimeUnit

/**
 * An admitted viewer against the real relay and a scripted PC: signed hello, peer-joined with the
 * epoch before hello-ok, handshake with the pinned key, then ctl in both directions only.
 */
class SessionE2eTest {
    @get:Rule val relay = RelayServer()

    private val http = OkHttpClient.Builder().readTimeout(0, TimeUnit.MILLISECONDS).build()
    private val closers = mutableListOf<AutoCloseable>()

    @After fun tearDown() {
        closers.asReversed().forEach { runCatching { it.close() } }
        http.dispatcher.executorService.shutdown()
        http.connectionPool.evictAll()
    }

    private fun <T : AutoCloseable> T.closing(): T = also { closers.add(it) }

    @Test fun admittedViewerRunsHandshakeAndCtlBothWays() {
        val viewerKey = viewerKey()
        val pc = FakePc(relay, http, pcKey()).closing()
        pc.register()
        pc.admit(viewerKey.id)

        val viewer = ViewerWire(relay, http, viewerKey, pc.id, pc.key.publicKey, DEVICE_NAME).closing()
        viewer.start()

        // Decisions item 22: the PC gets peer-joined (epoch, pairing=false) before anything else.
        val peer = pc.expectPeerJoined()
        assertFalse(peer.pairing)
        val admitted = viewer.expect<SessionWire.Event.Admitted>()
        assertEquals(peer.mediaEpoch, admitted.hello.mediaEpoch)
        assertFalse(admitted.hello.pairing)
        assertNotNull(admitted.hello.ticket)

        // The PC builds its transcript with the epoch from peer-joined; the pinned key verifies.
        val handshake = pc.answerAuthHello(peer)
        assertEquals(viewerKey.id, handshake.viewerId)
        assertEquals(listOf(CtlChannel.CAP, CtlPassThrough.CAP, MonitorPreviews.CAP), handshake.viewerCaps)
        handshake.awaitAuthFinish()
        viewer.expect<SessionWire.Event.Authenticated>()

        // Decisions item 19: the viewer's first ctl is caps with its name.
        val caps = handshake.receiveCtl()
        assertEquals(CtlChannel.KIND_CAPS, caps.kind)
        val capsJson = AuthJson.parseObject(caps.text())
        assertEquals("caps", capsJson["type"])
        assertEquals(listOf(CtlChannel.CAP, CtlPassThrough.CAP, MonitorPreviews.CAP), capsJson["caps"])
        assertEquals(DEVICE_NAME, capsJson["name"])

        // A binary input frame goes as ctl kind "input" and decrypts to the exact bytes.
        val input = byteArrayOf(0x42, 0x50, 0x49, 0x31, 0x00, 0x7f, 0x80.toByte(), 0xff.toByte(), 0x01)
        assertTrue(viewer.wire.sendInput(input))
        val inputCtl = handshake.receiveCtl()
        assertEquals(CtlChannel.KIND_INPUT, inputCtl.kind)
        assertArrayEquals(input, inputCtl.plaintext)

        // Endpoint JSON from the viewer also goes as ctl.
        assertTrue(viewer.wire.sendJson("{\"type\":\"media-control\",\"action\":\"monitors\",\"reason\":\"e2e\"}"))
        val control = handshake.receiveCtl()
        assertEquals("media-control", control.kind)
        assertEquals("monitors", AuthJson.parseObject(control.text())["action"])

        // PC -> viewer: caps first (decisions item 16), then a cursor binary ctl (item 20).
        handshake.sendCtlJson(linkedMapOf("type" to "caps", "caps" to listOf(CtlChannel.CAP)))
        val cursor = byteArrayOf(0x43, 0x55, 0x52, 0x31, 0x02, 0x00, 0x10, 0x00, 0x20)
        handshake.sendCtl(CtlChannel.KIND_CURSOR, cursor)
        val pcCaps = viewer.expect<SessionWire.Event.Endpoint>()
        assertEquals("caps", AuthJson.parseObject(pcCaps.text)["type"])
        val cursorEvent = viewer.expect<SessionWire.Event.Cursor>()
        assertArrayEquals(cursor, cursorEvent.bytes)

        // Round trip once more so every frame above has crossed before the plaintext check.
        handshake.sendCtlJson(linkedMapOf("type" to "host-capabilities", "fileTransfer" to 0, "audio" to 0))
        assertEquals("host-capabilities", AuthJson.parseObject(viewer.expect<SessionWire.Event.Endpoint>().text)["type"])
        pc.sync()

        // Decisions item 15: no plaintext endpoint message crosses after the handshake.
        val pcAfter = pc.received.map { AuthJson.parseObject(it)["type"] }
            .let { it.drop(it.indexOf("auth-finish") + 1) }
            .filterNot { it == "pong" } // the PC's own relay ping above
        assertEquals(listOf("ctl", "ctl", "ctl"), pcAfter)
        val viewerAfter = viewer.typesAfter("auth-host")
        assertTrue("viewer got plaintext after the handshake: $viewerAfter", viewerAfter.all { it == "ctl" || it in RELAY_TYPES })
        assertEquals(3, viewerAfter.count { it == "ctl" })
    }

    companion object {
        const val DEVICE_NAME = "E2E Pixel"
        private val RELAY_TYPES = setOf("ping", "pong")
    }
}
