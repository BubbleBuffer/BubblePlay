package de.bubblebuffer.bubbleplay.auth

import de.bubblebuffer.bubbleplay.Fixtures
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class PairLinkTest {
    private val link = pairingVectors.getJSONObject("qr").getString("android")

    @Test
    fun normalisesTheFixtureInputs() {
        for (case in Fixtures.objects(pairingVectors.getJSONArray("normalise"))) {
            assertEquals(case.getString("canonical"), PairingCode.normalise(case.getString("input")))
        }
        val code = pairingVectors.getString("code")
        assertEquals(code, PairingCode.normalise(code))
        assertEquals(pairingVectors.getString("codeDisplay"), PairingCode.display(code))
    }

    @Test
    fun refusesCodesThatAreNotTenCrockfordChars() {
        for (bad in listOf("", "70Q1K9MXR", "70Q1K9MXR12", "70Q1K9MXRU", "70Q1K9MX_1", "70Q1K9MXRé1", "70Q1K9MXRı")) {
            assertNull(bad, PairingCode.normalise(bad))
        }
        assertThrows<IllegalArgumentException> { PairingCode.display("70q1k9mxr1") }
    }

    @Test
    fun parsesTheFixtureLinkAndRoundTrips() {
        val parsed = PairLink.parse(link)!!
        assertEquals(PairLink("https://example.test", pairingVectors.getString("pcId"), pairingVectors.getString("code")), parsed)
        assertEquals(link, parsed.toUri())
        assertEquals(parsed, PairLink.parse(parsed.toUri()))
    }

    @Test
    fun canonicalisesIdCodeAndOrigin() {
        val parsed = PairLink.parse(
            "bubbleplay://pair?relay=HTTPS%3A%2F%2FExample.Test%3A443%2F&pc=77vh7m568gjky12j&code=70q1k-9mxrl",
        )!!
        assertEquals(PairLink("https://example.test", "77VH-7M56-8GJK-Y12J", "70Q1K9MXR1"), parsed)
        assertEquals(
            "http://relay.local:8080",
            PairLink.parse("bubbleplay://pair?relay=http%3A%2F%2Frelay.local%3A8080&pc=77VH-7M56-8GJK-Y12J&code=70Q1K9MXR1")!!.relay,
        )
    }

    @Test
    fun refusesEverythingElse() {
        val pc = "pc=77VH-7M56-8GJK-Y12J"
        val code = "code=70Q1K9MXR1"
        val relay = "relay=https%3A%2F%2Fexample.test"
        val bad = listOf(
            "https://pair?$relay&$pc&$code",
            "bubbleplay://other?$relay&$pc&$code",
            "bubbleplay://pair/x?$relay&$pc&$code",
            "bubbleplay://pair?$relay&$pc&$code#frag",
            "bubbleplay://pair?$relay&$pc",
            "bubbleplay://pair?$relay&$pc&$code&extra=1",
            "bubbleplay://pair?$relay&$pc&$code&$code",
            "bubbleplay://pair?relay=ftp%3A%2F%2Fexample.test&$pc&$code",
            "bubbleplay://pair?relay=https%3A%2F%2Fexample.test%2Fpath&$pc&$code",
            "bubbleplay://pair?relay=https%3A%2F%2Fexample.test%3Fq%3D1&$pc&$code",
            "bubbleplay://pair?relay=https%3A%2F%2Fexample.test%23f&$pc&$code",
            "bubbleplay://pair?relay=https%3A%2F%2Fuser%40example.test&$pc&$code",
            "bubbleplay://pair?relay=example.test&$pc&$code",
            "bubbleplay://pair?$relay&pc=77VH-7M56-8GJK-Y12&$code",
            "bubbleplay://pair?$relay&pc=77VH-7M56-8GJK-Y12U&$code",
            "bubbleplay://pair?$relay&$pc&code=70Q1K9MXR",
            "bubbleplay://pair?$relay&$pc&code=70Q1K9MXR%",
            "bubbleplay://pair?$relay&$pc&code=70Q1K9MXR%FF",
            "bubbleplay:pair?$relay&$pc&$code",
            "not a uri",
        )
        for (uri in bad) assertNull(uri, PairLink.parse(uri))
    }
}
