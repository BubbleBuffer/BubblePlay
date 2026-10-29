package de.bubblebuffer.bubbleplay.pcs

import org.junit.Assert.assertEquals
import org.junit.Test

class PcListCodecTest {
    private val desk = PcEntry("B7QK-2M9X-H4PD-R8TN", "AAAA", "https://relay.example.dev", "Desk", 1_700_000_000_000L)
    private val unnamed = PcEntry("Q3WT-6MNC-5XYZ-2KHB", "BBBB", "https://relay.example.dev:8443", "", 1_700_000_100_000L)

    @Test
    fun roundTripKeepsEveryField() {
        val entries = listOf(desk.copy(lastSeen = 1_700_000_200_000L, problem = "PC said \"revoked\"\nRetry later"), unnamed)
        assertEquals(entries, PcListCodec.decode(PcListCodec.encode(entries)))
    }

    @Test
    fun oldVersionOneListDefaultsConnectionMetadata() {
        val text = """{"version":1,"pcs":[{"id":"X","publicKey":"B","relay":"https://r","name":"n","paired":2}]}"""
        assertEquals(listOf(PcEntry("X", "B", "https://r", "n", 2L)), PcListCodec.decode(text))
    }

    @Test
    fun nullOrMalformedOptionalMetadataDoesNotLosePairedPc() {
        val text = """
            {"version":1,"pcs":[
              {"id":"X","publicKey":"B","relay":"https://r","name":"n","paired":2,"lastSeen":null,"problem":null},
              {"id":"Y","publicKey":"C","relay":"https://r","name":"m","paired":3,"lastSeen":"bad","problem":{"message":"bad"}}
            ]}
        """.trimIndent()
        assertEquals(
            listOf(PcEntry("X", "B", "https://r", "n", 2L), PcEntry("Y", "C", "https://r", "m", 3L)),
            PcListCodec.decode(text),
        )
    }

    @Test
    fun nullOrEmptyTextGivesNoEntries() {
        assertEquals(emptyList<PcEntry>(), PcListCodec.decode(null))
        assertEquals(emptyList<PcEntry>(), PcListCodec.decode(""))
    }

    @Test
    fun textOfAnotherShapeGivesNoEntries() {
        assertEquals(emptyList<PcEntry>(), PcListCodec.decode("{not json"))
        assertEquals(emptyList<PcEntry>(), PcListCodec.decode("[]"))
        assertEquals(emptyList<PcEntry>(), PcListCodec.decode("{\"version\":1}"))
    }

    @Test
    fun malformedEntriesAreSkippedAndTheRestKept() {
        val text = """
            {"version":1,"pcs":[
              {"id":"","publicKey":"A","relay":"https://r","name":"","paired":1},
              {"id":"X","publicKey":"B","relay":"https://r","name":"n","paired":2},
              {"id":"Y","publicKey":"C","relay":"https://r","name":"n"}
            ]}
        """.trimIndent()
        assertEquals(listOf(PcEntry("X", "B", "https://r", "n", 2L)), PcListCodec.decode(text))
    }
}
