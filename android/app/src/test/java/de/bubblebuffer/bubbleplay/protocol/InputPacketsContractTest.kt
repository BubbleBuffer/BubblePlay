package de.bubblebuffer.bubbleplay.protocol

import de.bubblebuffer.bubbleplay.Fixtures
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Test

/** fixtures/input-contract.json "pointerKeyTextPackets": the Kotlin encoders against the shared vectors. */
class InputPacketsContractTest {
    private val contract = Fixtures.json("input-contract.json").getJSONObject("pointerKeyTextPackets")

    private fun hex(bytes: ByteArray) = bytes.joinToString("") { "%02x".format(it.toInt() and 0xff) }

    private fun encode(d: JSONObject): ByteArray = when (val kind = d.getString("kind")) {
        "key" -> InputPackets.key(d.getInt("vk"), d.getBoolean("up"))
        "buttons" -> InputPackets.buttons(d.getInt("mask"))
        "releaseAll" -> InputPackets.releaseAll()
        "motion" -> InputPackets.motion(d.getInt("seq"), d.getInt("dx"), d.getInt("dy"), d.getInt("wheelX"), d.getInt("wheelY"))
        "absolute" -> InputPackets.absolute(d.getInt("seq"), d.getInt("x"), d.getInt("y"))
        "buttonsAt" -> InputPackets.buttonsAt(d.getInt("seq"), d.getInt("x"), d.getInt("y"), d.getInt("mask"))
        "cursorSync" -> InputPackets.cursorSync(d.getInt("seq"))
        "text" -> InputPackets.text(d.getString("text"))!!
        else -> error("unknown kind $kind")
    }

    @Test
    fun encodersProduceTheContractBytes() {
        for (vector in Fixtures.objects(contract.getJSONArray("vectors"))) {
            assertEquals(vector.getString("name"), vector.getString("hex"), hex(encode(vector.getJSONObject("decoded"))))
        }
    }

    @Test
    fun textStopsAtTheContractLimit() {
        val max = contract.getInt("textMaxBytes")
        assertNotNull(InputPackets.text("x".repeat(max)))
        assertNull(InputPackets.text("x".repeat(max + 1)))
        assertNull(InputPackets.text(""))
    }
}
