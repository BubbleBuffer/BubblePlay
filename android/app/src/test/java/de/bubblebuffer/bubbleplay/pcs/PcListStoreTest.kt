package de.bubblebuffer.bubbleplay.pcs

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class PcListStoreTest {
    /** SharedPreferences stand-in: one text value. */
    private class MemoryBacking(var text: String? = null) : PcListStore.Backing {
        override fun read(): String? = text
        override fun write(text: String) {
            this.text = text
        }
    }

    private val desk = PcEntry("B7QK-2M9X-H4PD-R8TN", "AAAA", "https://relay.example.dev", "Desk", 1L)
    private val den = PcEntry("Q3WT-6MNC-5XYZ-2KHB", "BBBB", "https://relay.example.dev", "Den", 2L)

    @Test
    fun putThenListKeepsOrderAndGetFindsEachPc() {
        val store = PcListStore(MemoryBacking())
        store.put(desk)
        store.put(den)
        assertEquals(listOf(desk, den), store.list())
        assertEquals(den, store.get(den.id))
        assertNull(store.get("0000-0000-0000-0000"))
    }

    @Test
    fun putWithAnExistingIdReplacesThatEntryInPlace() {
        val store = PcListStore(MemoryBacking())
        store.put(desk)
        store.put(den)
        val moved = desk.copy(relay = "https://other.example.dev", paired = 9L)
        store.put(moved)
        assertEquals(listOf(moved, den), store.list())
    }

    @Test
    fun renameChangesOnlyTheNamedPc() {
        val store = PcListStore(MemoryBacking())
        store.put(desk)
        store.put(den)
        store.rename(desk.id, "Office")
        assertEquals(listOf(desk.copy(name = "Office"), den), store.list())
    }

    @Test
    fun removeDropsOnlyThatPc() {
        val store = PcListStore(MemoryBacking())
        store.put(desk)
        store.put(den)
        store.remove(desk.id)
        assertEquals(listOf(den), store.list())
    }

    @Test
    fun touchPersistsOnlyTheNamedPcsMetadataAndSuccessClearsTheProblem() {
        val backing = MemoryBacking()
        val store = PcListStore(backing)
        store.put(desk)
        store.put(den)
        store.touch(desk.id, 123L, "Key revoked")
        assertEquals(listOf(desk.copy(lastSeen = 123L, problem = "Key revoked"), den), PcListStore(backing).list())
        store.touch(desk.id, 456L, null)
        assertEquals(listOf(desk.copy(lastSeen = 456L), den), PcListStore(backing).list())
    }

    @Test
    fun touchDoesNotCreateAnUnknownPc() {
        val store = PcListStore(MemoryBacking())
        store.put(desk)
        store.touch(den.id, 123L, "Offline")
        assertEquals(listOf(desk), store.list())
    }

    @Test
    fun badStoredTextIsAnEmptyListAndCanBeWrittenOver() {
        val backing = MemoryBacking("{broken")
        val store = PcListStore(backing)
        assertEquals(emptyList<PcEntry>(), store.list())
        store.put(desk)
        assertEquals(listOf(desk), store.list())
    }
}
