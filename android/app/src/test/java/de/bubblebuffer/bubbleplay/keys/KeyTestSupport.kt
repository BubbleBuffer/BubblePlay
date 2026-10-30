package de.bubblebuffer.bubbleplay.keys

import de.bubblebuffer.bubbleplay.Fixtures
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.fail

/** The shared device-key contract (fixtures/shared), which holds the new format name only. */
internal fun sharedContract(): JSONObject = Fixtures.json("shared/device-keys-contract.json")

/** The `keyFiles` section of the shared contract: files in the shared format. */
internal fun sharedKeyFiles(): JSONObject = sharedContract().getJSONObject("keyFiles")

internal fun contractIds(): List<JSONObject> =
    Fixtures.objects(Fixtures.json("device-keys-contract.json").getJSONArray("ids"))

internal fun contractHandshake(): JSONObject =
    Fixtures.json("device-keys-contract.json").getJSONObject("handshake")

/** Asserts that [block] throws IllegalArgumentException (a caller error, not a refused file). */
internal fun illegalArgument(block: () -> Unit) {
    try {
        block()
    } catch (e: IllegalArgumentException) {
        return
    }
    fail("expected IllegalArgumentException")
}

internal fun hex(value: String): ByteArray = Fixtures.hex(value)

internal fun ByteArray.toHex(): String = joinToString("") { "%02x".format(it) }

/** A deep copy of [base] with [change] applied, as key-file text. */
internal fun mutatedText(base: JSONObject, change: JSONObject.() -> Unit): String =
    JSONObject(base.toString()).apply(change).toString()

/** Asserts that [block] refuses with [reason]. */
internal fun refusedWith(reason: DeviceKeyException.Reason, block: () -> Unit) {
    try {
        block()
    } catch (e: DeviceKeyException) {
        assertEquals(reason, e.reason)
        return
    }
    fail("expected refusal $reason")
}

/** Runs [block] and closes the key afterwards. */
internal inline fun <T> DeviceKey.useKey(block: (DeviceKey) -> T): T = try {
    block(this)
} finally {
    close()
}
