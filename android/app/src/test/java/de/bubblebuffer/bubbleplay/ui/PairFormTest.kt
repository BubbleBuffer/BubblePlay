package de.bubblebuffer.bubbleplay.ui

import de.bubblebuffer.bubbleplay.pcs.PcEntry
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class PairFormTest {
    private val id = "B7QK-2M9X-H4PD-R8TN"

    @Test
    fun relayOriginKeepsOnlySchemeHostAndPort() {
        assertEquals("https://relay.example.dev", PairForm.relayOrigin("https://Relay.Example.dev/path?x=1"))
        assertEquals("http://10.0.0.5:8080", PairForm.relayOrigin("  http://10.0.0.5:8080/  "))
    }

    @Test
    fun relayOriginRefusesWhatIsNotAnHttpOrigin() {
        assertNull(PairForm.relayOrigin("ftp://relay.example.dev"))
        assertNull(PairForm.relayOrigin("relay.example.dev"))
        assertNull(PairForm.relayOrigin("https://user:secret@relay.example.dev"))
        assertNull(PairForm.relayOrigin("https://"))
        assertNull(PairForm.relayOrigin("not a url"))
        assertNull(PairForm.relayOrigin(""))
    }

    @Test
    fun pcIdAcceptsAnyCaseDashesAndSpaces() {
        assertEquals(id, PairForm.pcId(id))
        assertEquals(id, PairForm.pcId("b7qk-2m9x-h4pd-r8tn"))
        assertEquals(id, PairForm.pcId("B7QK 2M9X H4PD R8TN"))
        assertEquals(id, PairForm.pcId("B7QK2M9XH4PDR8TN"))
    }

    @Test
    fun pcIdRefusesWrongLengthAndLettersOutsideTheAlphabet() {
        assertNull(PairForm.pcId("U7QK-2M9X-H4PD-R8TN"))
        assertNull(PairForm.pcId("B7QK-2M9X-H4PD"))
        assertNull(PairForm.pcId(""))
    }

    @Test
    fun defaultNameUsesTheFirstFourCharacters() {
        assertEquals("PC B7QK", PairForm.defaultName(id))
        assertEquals("PC B7QK", PairForm.name("   ", id))
        assertEquals("Desk", PairForm.name(" Desk ", id))
    }

    @Test
    fun suggestedRelayIsTheMostRecentlyPairedPcsRelay() {
        assertNull(PairForm.suggestedRelay(emptyList()))
        val older = PcEntry(id, "AAAA", "https://old.example.dev", "Old", 1L)
        val newer = PcEntry("Q3WT-6MNC-5XYZ-2KHB", "BBBB", "https://new.example.dev", "New", 5L)
        assertEquals("https://new.example.dev", PairForm.suggestedRelay(listOf(older, newer)))
    }

    @Test
    fun displayNameFallsBackToTheDefault() {
        assertEquals("Desk", PairForm.displayName(PcEntry(id, "AAAA", "https://r", "Desk", 1L)))
        assertEquals("PC B7QK", PairForm.displayName(PcEntry(id, "AAAA", "https://r", "", 1L)))
    }
}
