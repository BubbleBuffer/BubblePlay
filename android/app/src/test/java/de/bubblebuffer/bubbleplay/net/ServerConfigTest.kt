package de.bubblebuffer.bubbleplay.net

import okhttp3.HttpUrl.Companion.toHttpUrl
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertSame
import org.junit.Test

class ServerConfigTest {
    private val epoch = "a".repeat(32)
    private val pc = "B7QK-2M9X-H4PD-R8TN"
    private val run = "5c1f3e2a-8b4d-4e6f-9a0b-1c2d3e4f5a6b"
    private val prod = ServerConfig("https://relay.example/".toHttpUrl())
    private val lab = ServerConfig("https://relay.example/".toHttpUrl(), labRun = run, labCompat = "c1")

    @Test fun signalingUrlIsUnchanged() {
        assertEquals("https://relay.example/ws", prod.signalingUrl().toString())
        assertEquals("https://relay.example/lab-ws?run=$run&compat=c1", lab.signalingUrl().toString())
    }

    @Test fun signalingHelloIsUnchanged() {
        val hello = ServerConfig.signalingHello()
        assertEquals(2, hello.length())
        assertEquals("hello", hello.getString("type"))
        assertEquals("client", hello.getString("role"))
        assertEquals("hello", JSONObject(hello.toString()).getString("type"))
    }

    @Test fun mediaAndFileUrlsWithoutPcAreUnchanged() {
        assertEquals("https://relay.example/media-ws?epoch=$epoch", prod.mediaUrl(epoch).toString())
        assertEquals("https://relay.example/file-ws?epoch=$epoch", prod.fileUrl(epoch).toString())
        assertEquals("https://relay.example/media-ws?epoch=$epoch", prod.mediaUrl(epoch, null).toString())
        assertEquals("https://relay.example/file-ws?epoch=$epoch", prod.fileUrl(epoch, null).toString())
    }

    @Test fun pcFollowsTheEpoch() {
        assertEquals("https://relay.example/media-ws?epoch=$epoch&pc=$pc", prod.mediaUrl(epoch, pc).toString())
        assertEquals("https://relay.example/file-ws?epoch=$epoch&pc=$pc", prod.fileUrl(epoch, pc).toString())
    }

    @Test fun labUrlsStayTheSameWithoutPc() {
        assertEquals(
            "https://relay.example/lab-media-ws?epoch=$epoch&run=$run&compat=c1",
            lab.mediaUrl(epoch).toString(),
        )
        assertEquals(
            "https://relay.example/lab-file-ws?epoch=$epoch&run=$run&compat=c1",
            lab.fileUrl(epoch).toString(),
        )
    }

    @Test fun labUrlsPlacePcAfterEpochBeforeRunAndCompat() {
        assertEquals(
            "https://relay.example/lab-media-ws?epoch=$epoch&pc=$pc&run=$run&compat=c1",
            lab.mediaUrl(epoch, pc).toString(),
        )
        assertEquals(
            "https://relay.example/lab-file-ws?epoch=$epoch&pc=$pc&run=$run&compat=c1",
            lab.fileUrl(epoch, pc).toString(),
        )
    }

    @Test fun withPcLeavesTheUrlAloneWhenThereIsNoPc() {
        val url = prod.fileUrl(epoch)
        assertSame(url, ServerConfig.withPc(url, null))
    }

    @Test fun withPcAddsThePcAfterTheEpochForProduction() {
        assertEquals(
            prod.fileUrl(epoch, pc).toString(),
            ServerConfig.withPc(prod.fileUrl(epoch), pc).toString(),
        )
    }

    @Test fun withPcAddsThePcAfterTheEpochInLabUrls() {
        // The file URL from fileUrl(epoch) has run and compat; the PC must still follow the epoch.
        assertEquals(
            "https://relay.example/lab-file-ws?epoch=$epoch&pc=$pc&run=$run&compat=c1",
            ServerConfig.withPc(lab.fileUrl(epoch), pc).toString(),
        )
        assertEquals(
            lab.fileUrl(epoch, pc).toString(),
            ServerConfig.withPc(lab.fileUrl(epoch), pc).toString(),
        )
    }

    @Test fun withPcNeverDuplicatesAnExistingPc() {
        val other = "C7QK-2M9X-H4PD-R8TN"
        val once = ServerConfig.withPc(lab.fileUrl(epoch, pc), other)
        assertEquals(listOf(other), once.queryParameterValues("pc"))
        assertEquals("https://relay.example/lab-file-ws?epoch=$epoch&pc=$other&run=$run&compat=c1", once.toString())
        // Setting the same PC again does not add a second one.
        val again = ServerConfig.withPc(once, other)
        assertEquals(listOf(other), again.queryParameterValues("pc"))
        assertEquals(once.toString(), again.toString())
    }

    @Test fun withPcAppendsWhenThereIsNoEpoch() {
        assertEquals(
            "https://relay.example/file-ws?pc=$pc",
            ServerConfig.withPc("https://relay.example/file-ws".toHttpUrl(), pc).toString(),
        )
    }

    @Test fun pcIsEncodedInTheUrlAsOneValue() {
        // Opaque relay values are encoded, so separators in them cannot add parameters.
        val odd = "a&epoch=evil b/c"
        val url = ServerConfig.withPc(prod.fileUrl(epoch), odd)
        assertEquals(odd, url.queryParameter("pc"))
        assertEquals(listOf(epoch), url.queryParameterValues("epoch"))
        assertEquals(listOf(odd), url.queryParameterValues("pc"))
    }

    @Test fun urlsNeverCarryTheTicket() {
        // A URL gets only the PC: the ticket is written in the hello, never here.
        val ticket = "T".repeat(42) + "x"
        val grant = ViewerGrant.from(pc, ticket)
        val urls = listOf(
            prod.mediaUrl(epoch, grant.pc).toString(),
            prod.fileUrl(epoch, grant.pc).toString(),
            ServerConfig.withPc(lab.fileUrl(epoch), grant.pc).toString(),
            lab.mediaUrl(epoch, grant.pc).toString(),
        )
        urls.forEach { assertFalse(it, it.contains(ticket)) }
    }
}
