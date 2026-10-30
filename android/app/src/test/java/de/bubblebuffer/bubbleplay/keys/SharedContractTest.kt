package de.bubblebuffer.bubbleplay.keys

import de.bubblebuffer.bubbleplay.Fixtures
import de.bubblebuffer.bubbleplay.auth.RelayProof
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/** The shared device-key contract as the app reads it: identities, and relay proofs under each vector's own label. */
class SharedContractTest {
    @Test
    fun sharedIdsDeriveFromTheirSeedsAndPublicKeys() {
        val ids = Fixtures.objects(sharedContract().getJSONArray("ids"))
        assertTrue(ids.isNotEmpty())
        for (id in ids) {
            val name = id.getString("name")
            val publicKey = StrictBase64Url.decode(id.getString("public"), 32)
            assertEquals(name, id.getString("id"), DeviceId.fromPublicKey(publicKey))
            DeviceKey.fromSeed(hex(id.getString("seedHex"))).useKey { key ->
                assertEquals(name, id.getString("id"), key.id)
                assertArrayEquals(publicKey, key.publicKey)
            }
        }
    }

    @Test
    fun relayProofVectorsReproduceUnderTheirOwnLabel() {
        val vectors = Fixtures.objects(sharedContract().getJSONObject("relayProof").getJSONArray("vectors"))
        assertTrue(vectors.isNotEmpty())
        for (vector in vectors) {
            val name = vector.getString("name")
            val label = vector.getString("label")
            val role = vector.getString("role")
            val origin = vector.getString("origin")
            val id = vector.getString("id")
            val publicKey = StrictBase64Url.decode(vector.getString("public"), 32)
            val nonce = hex(vector.getString("nonceHex"))
            val signature = StrictBase64Url.decode(vector.getString("signature"), 64)

            assertEquals(name, vector.getString("encodingHex"),
                RelayProof.encoding(label, role, origin, nonce, id, publicKey).toHex())
            assertEquals(name, vector.getString("sha256Hex"),
                RelayProof.hash(label, role, origin, nonce, id, publicKey).toHex())
            assertEquals(name, vector.getString("expected") == "valid",
                RelayProof.verify(label, role, origin, nonce, id, publicKey, signature))
        }
    }
}
