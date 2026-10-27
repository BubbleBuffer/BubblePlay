package de.bubblebuffer.bubbleplay.net

import okhttp3.HttpUrl.Companion.toHttpUrl
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class ServerConfigTest {
    @Test fun allSocketsAddressThePcAndDataSocketsCarryOpaqueEpoch() {
        val config = ServerConfig("https://relay.example/old?x=y".toHttpUrl(), "B7QK-2M9X-H4PD-R8TN")
        val epoch = "a".repeat(32)
        assertEquals("https://relay.example/ws?pc=B7QK-2M9X-H4PD-R8TN", config.signalingUrl().toString())
        assertEquals("https://relay.example/media-ws?epoch=$epoch&pc=B7QK-2M9X-H4PD-R8TN", config.mediaUrl(epoch).toString())
        assertEquals("https://relay.example/file-ws?epoch=$epoch&pc=B7QK-2M9X-H4PD-R8TN", config.fileUrl(epoch).toString())
        assertNull(config.mediaUrl(epoch).queryParameter("ticket"))
    }
    @Test fun valuesAreEncodedAsSingleParameters() {
        val config = ServerConfig("https://relay.example/".toHttpUrl(), "pc&bad=x")
        val url = config.fileUrl("opaque&epoch=x")
        assertEquals(listOf("pc&bad=x"), url.queryParameterValues("pc"))
        assertEquals(listOf("opaque&epoch=x"), url.queryParameterValues("epoch"))
    }
}
