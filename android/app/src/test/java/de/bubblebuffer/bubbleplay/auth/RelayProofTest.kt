package de.bubblebuffer.bubbleplay.auth

import de.bubblebuffer.bubbleplay.Fixtures
import org.json.JSONObject
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class RelayProofTest {
    private val relay = contract.getJSONObject("relayProof")
    private val vectors = Fixtures.objects(relay.getJSONArray("vectors"))

    private fun seedFor(id: String): String =
        Fixtures.objects(contract.getJSONArray("ids")).first { it.getString("id") == id }.getString("seedHex")

    @Test
    fun reproducesEveryVectorThroughTheSigningPath() {
        for (v in vectors) {
            val role = v.getString("role")
            val origin = v.getString("origin")
            val nonce = b64(v.getString("nonce"))
            val id = v.getString("id")
            val public = b64(v.getString("public"))
            assertArrayEquals(hex(v.getString("nonceHex")), nonce)
            val encoding = RelayProof.encoding(role, origin, nonce, id, public)
            assertEquals(v.getString("name"), v.getString("encodingHex"), encoding.toHex())
            assertEquals(v.getString("sha256Hex"), RelayProof.hash(role, origin, nonce, id, public).toHex())
            val valid = v.getString("expected") == "valid"
            assertEquals(v.getString("name"), valid, RelayProof.verify(role, origin, nonce, id, public, b64(v.getString("signature"))))
            if (valid) {
                keyFromSeed(seedFor(id)).use { key ->
                    assertEquals(v.getString("signature"), b64(RelayProof.sign(key, origin, nonce, role)))
                }
            }
        }
    }

    @Test
    fun viewerDefaultRoleMatchesTheViewerVector() {
        val v = vectors.first { it.getString("name") == "viewer" }
        viewerKey().use { key ->
            assertEquals(v.getString("signature"), b64(RelayProof.sign(key, v.getString("origin"), b64(v.getString("nonce")))))
        }
    }

    @Test
    fun verifyRefusesOtherRolesFlippedBitsAndForeignIds() {
        val v = vectors.first { it.getString("name") == "viewer" }
        val nonce = b64(v.getString("nonce"))
        val public = b64(v.getString("public"))
        val sig = b64(v.getString("signature"))
        val origin = v.getString("origin")
        assertTrue(RelayProof.verify("viewer", origin, nonce, v.getString("id"), public, sig))
        assertFalse(RelayProof.verify("host", origin, nonce, v.getString("id"), public, sig))
        assertFalse(RelayProof.verify("viewer", origin, nonce.copyOf().also { it[0] = (it[0] + 1).toByte() }, v.getString("id"), public, sig))
        assertFalse(RelayProof.verify("viewer", origin, nonce, v.getString("id"), public, sig.copyOf().also { it[10] = (it[10] + 1).toByte() }))
        assertFalse(RelayProof.verify("viewer", origin, nonce, pcId(), public, sig))
        assertFalse(RelayProof.verify("viewer", origin, nonce, v.getString("id"), public, sig.copyOf(63)))
    }

    @Test
    fun viewerHelloMatchesTheFixture() {
        val expected = pairingVectors.getJSONObject("messages").getJSONObject("viewerHello")
        viewerKey().use { key ->
            assertJsonEquals(expected, ViewerHello.hello(key, expected.getString("pair")))
            val plain = JSONObject(ViewerHello.hello(key))
            assertEquals(setOf("type", "role", "id", "public"), jsonKeys(plain))
            assertEquals("viewer", plain.getString("role"))
        }
    }

    @Test
    fun registerChallengeAndProof() {
        val v = vectors.first { it.getString("name") == "viewer" }
        val challenge = JSONObject().put("type", "register-challenge").put("nonce", v.getString("nonce")).toString()
        val nonce = ViewerHello.parseRegisterChallenge(challenge)
        assertArrayEquals(b64(v.getString("nonce")), nonce)
        viewerKey().use { key ->
            val proof = ViewerHello.registerProof(key, v.getString("origin"), nonce)
            assertJsonEquals(JSONObject().put("type", "register-proof").put("signature", v.getString("signature")), proof)
        }
        // The host-role fixture message has the same shape.
        assertArrayEquals(
            b64(relay.getJSONObject("messages").getJSONObject("registerChallenge").getString("nonce")),
            ViewerHello.parseRegisterChallenge(relay.getJSONObject("messages").getJSONObject("registerChallenge").toString()),
        )
        for (bad in listOf(
            """{"type":"register-challenge"}""",
            """{"type":"register-challenge","nonce":"AAAA"}""",
            """{"type":"register-challenge","nonce":"${v.getString("nonce")}","x":1}""",
            """{"type":"hello-ok","nonce":"${v.getString("nonce")}"}""",
            """{"type":"register-challenge","nonce":"${v.getString("nonce")}","nonce":"${v.getString("nonce")}"}""",
        )) {
            assertThrows<AuthJson.Malformed> { ViewerHello.parseRegisterChallenge(bad) }
        }
    }

    @Test
    fun helloOkForNormalAndPairingViewers() {
        val ticket = relay.getJSONObject("messages").getJSONObject("helloOk").getString("ticket")
        val epoch = "0123456789abcdef0123456789abcdef"
        val ice = """[{"urls":"stun:stun.l.google.com:19302"}]"""
        val normal = ViewerHello.parseHelloOk(
            """{"type":"hello-ok","pc":"${pcId()}","mediaEpoch":"$epoch","iceServers":$ice,"ticket":"$ticket"}""", pcId(),
        )
        assertEquals(HelloOk(pcId(), epoch, ticket, false, ice), normal)
        val pairing = ViewerHello.parseHelloOk(
            """{"type":"hello-ok","pc":"${pcId()}","mediaEpoch":"$epoch","iceServers":[],"pairing":true}""", pcId(),
        )
        assertTrue(pairing.pairing)
        assertNull(pairing.ticket)
        for (bad in listOf(
            """{"type":"hello-ok","pc":"${pcId()}","mediaEpoch":"$epoch","iceServers":[]}""",
            """{"type":"hello-ok","pc":"${pcId()}","mediaEpoch":"$epoch","iceServers":[],"ticket":"$ticket","pairing":true}""",
            """{"type":"hello-ok","pc":"${pcId()}","mediaEpoch":"$epoch","iceServers":[],"pairing":false}""",
            """{"type":"hello-ok","pc":"47Z3-3QX1-AJH6-2RKB","mediaEpoch":"$epoch","iceServers":[],"ticket":"$ticket"}""",
            """{"type":"hello-ok","pc":"${pcId()}","mediaEpoch":"XYZ","iceServers":[],"ticket":"$ticket"}""",
            """{"type":"hello-ok","pc":"${pcId()}","mediaEpoch":"$epoch","ticket":"$ticket"}""",
            """{"type":"hello-ok","pc":"${pcId()}","mediaEpoch":"$epoch","iceServers":[],"ticket":"short"}""",
        )) {
            assertThrows<AuthJson.Malformed> { ViewerHello.parseHelloOk(bad, pcId()) }
        }
    }

    @Test
    fun dataSocketHellos() {
        val ticket = relay.getJSONObject("messages").getJSONObject("helloOk").getString("ticket")
        assertJsonEquals(
            JSONObject().put("type", "hello").put("role", "media-viewer").put("ticket", ticket),
            ViewerHello.dataHello(ViewerHello.ROLE_MEDIA_VIEWER, ticket),
        )
        assertJsonEquals(
            JSONObject().put("type", "hello").put("role", "file-viewer").put("ticket", ticket),
            ViewerHello.dataHello(ViewerHello.ROLE_FILE_VIEWER, ticket),
        )
        assertThrows<IllegalArgumentException> { ViewerHello.dataHello("media-client", ticket) }
    }

    @Test
    fun admitHashMatchesTheFixture() {
        val pc = pairingVectors.getString("pcId")
        val code = pairingVectors.getString("code")
        assertEquals(pairingVectors.getString("admitBytesHex"), PairAdmit.bytes(pc, code).toHex())
        assertEquals(pairingVectors.getString("admit"), PairAdmit.hash(pc, code))
        assertEquals(pairingVectors.getString("admit"), PairAdmit.hash("77vh7m568gjky12j", "7oq-ik9 mxrl"))
        assertEquals(pairingVectors.getJSONObject("messages").getJSONObject("pairOpen").getString("admit"), PairAdmit.hash(pc, code))
        assertThrows<IllegalArgumentException> { PairAdmit.hash(pc, "short") }
        assertThrows<IllegalArgumentException> { PairAdmit.hash("nope", code) }
    }
}
