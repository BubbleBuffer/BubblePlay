package de.bubblebuffer.bubbleplay.auth

import de.bubblebuffer.bubbleplay.Fixtures
import de.bubblebuffer.bubbleplay.keys.DeviceKey
import org.json.JSONObject
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import java.util.Base64

/** The shared device-key contract (test-only vectors). */
internal val contract: JSONObject by lazy { Fixtures.json("device-keys-contract.json") }
internal val handshakeVectors: JSONObject get() = contract.getJSONObject("handshake")
internal val phase2: JSONObject get() = contract.getJSONObject("phase2")
internal val pairingVectors: JSONObject get() = phase2.getJSONObject("pairing")

internal fun hex(value: String): ByteArray = Fixtures.hex(value)

internal fun ByteArray.toHex(): String = joinToString("") { "%02x".format(it) }

internal fun b64(text: String): ByteArray = Base64.getUrlDecoder().decode(text)

internal fun b64(bytes: ByteArray): String = Base64.getUrlEncoder().withoutPadding().encodeToString(bytes)

internal fun keyFromSeed(seedHex: String): DeviceKey = DeviceKey.fromSeed(hex(seedHex))

internal fun viewerKey(): DeviceKey = keyFromSeed(handshakeVectors.getJSONObject("viewer").getString("seedHex"))

internal fun pcKey(): DeviceKey = keyFromSeed(handshakeVectors.getJSONObject("pc").getString("seedHex"))

/** Hands out the given byte strings in order, one per call; each call must ask for its exact size. */
internal fun fixedRandom(vararg chunks: ByteArray): AuthRandom {
    val queue = ArrayDeque(chunks.toList())
    return AuthRandom { out ->
        val next = queue.removeFirstOrNull() ?: error("random exhausted")
        check(next.size == out.size) { "random chunk size ${next.size}, asked ${out.size}" }
        next.copyInto(out)
    }
}

/** The fixture viewer's nonce then ephemeral private key. */
internal fun fixtureRandom(): AuthRandom {
    val viewer = handshakeVectors.getJSONObject("viewer")
    return fixedRandom(b64(viewer.getString("nonce")), hex(viewer.getString("ephemeralPrivateHex")))
}

internal fun pcPublic(): ByteArray = b64(handshakeVectors.getJSONObject("pc").getString("public"))

internal fun pcId(): String = handshakeVectors.getJSONObject("pc").getString("id")

internal fun normalMode(): Handshake.Mode = Handshake.Mode.Normal(pcId(), pcPublic())

internal fun pairingMode(): Handshake.Mode = Handshake.Mode.Pairing(pcId(), pairingVectors.getString("code"))

/** A handshake started with the fixture's origin, epoch, randomness and dtls-bind-v1 (so caps match the fixture). */
internal fun fixtureHandshake(mode: Handshake.Mode = normalMode()): Handshake = Handshake.start(
    viewerKey(),
    handshakeVectors.getString("relayOrigin"),
    handshakeVectors.getString("sessionEpoch"),
    mode,
    fixtureRandom(),
    extraCaps = listOf("dtls-bind-v1"),
)

internal fun authHostText(): String = handshakeVectors.getJSONObject("messages").getJSONObject("authHost").toString()

/** The fixture auth-host with [change] applied. */
internal fun authHostWith(change: JSONObject.() -> Unit): String =
    JSONObject(authHostText()).apply(change).toString()

internal fun assertJsonEquals(expected: JSONObject, actual: String) {
    val parsed = JSONObject(actual)
    assertTrue("expected $expected\nactual   $parsed", expected.similar(parsed))
}

internal inline fun <reified T : Throwable> assertThrows(block: () -> Unit): T {
    try {
        block()
    } catch (e: Throwable) {
        if (e is T) return e
        throw e
    }
    fail("expected ${T::class.java.simpleName}")
    throw AssertionError()
}
