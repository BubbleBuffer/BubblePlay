package de.bubblebuffer.bubbleplay.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class RemoteClipboardTest {
    @Test
    fun latestBackgroundSnapshotWaitsForForeground() {
        val clipboard = RemoteClipboard()
        clipboard.receive("old")
        assertNull(clipboard.takeAutomatic())
        clipboard.receive("latest")
        assertNull(clipboard.takeAutomatic())
        clipboard.foreground = true
        assertEquals("latest", clipboard.takeAutomatic())
        assertNull(clipboard.takeAutomatic())
    }

    @Test
    fun automaticClipboardRemainsTheDefault() {
        val clipboard = RemoteClipboard()
        clipboard.foreground = true
        clipboard.receive("from PC")
        assertEquals("from PC", clipboard.takeAutomatic())
    }

    @Test
    fun localPasteOrCopyDropsTheUnwrittenHostSnapshot() {
        for (foreground in listOf(false, true)) {
            val clipboard = RemoteClipboard()
            clipboard.receive("pending PC clipboard")
            clipboard.localAction()
            clipboard.foreground = foreground
            assertNull(clipboard.takeAutomatic())
            clipboard.foreground = true
            assertNull(clipboard.takeAutomatic())
            assertNull(clipboard.pending)
        }
    }

    @Test
    fun copyInAnotherAppWinsWhenForegroundAccessReturns() {
        val clipboard = RemoteClipboard()
        clipboard.foreground = true
        clipboard.observeLocal(10L)
        clipboard.foreground = false
        clipboard.receive("pending PC clipboard")
        assertNull(clipboard.takeAutomatic())
        clipboard.foreground = true
        clipboard.observeLocal(11L)
        assertNull(clipboard.takeAutomatic())
    }

    @Test
    fun unchangedClipboardAllowsTheLatestSnapshotOnResume() {
        val clipboard = RemoteClipboard()
        clipboard.observeLocal(10L)
        clipboard.receive("latest PC clipboard")
        clipboard.foreground = true
        clipboard.observeLocal(10L)
        assertEquals("latest PC clipboard", clipboard.takeAutomatic())
    }

    @Test
    fun ourAsynchronousWriteNotificationDoesNotDiscardANewerSnapshot() {
        val clipboard = RemoteClipboard()
        clipboard.foreground = true
        clipboard.observeLocal(10L)
        clipboard.receive("first PC clipboard")
        assertEquals("first PC clipboard", clipboard.takeAutomatic())
        clipboard.rememberWrite(11L)
        clipboard.foreground = false
        clipboard.receive("newer PC clipboard")
        clipboard.observeLocal(11L)
        clipboard.foreground = true
        assertEquals("newer PC clipboard", clipboard.takeAutomatic())
    }

    @Test
    fun clearingThePhoneClipboardAlsoDiscardsThePendingSnapshot() {
        val clipboard = RemoteClipboard()
        clipboard.observeLocal(10L)
        clipboard.receive("pending PC clipboard")
        clipboard.observeLocal(null)
        clipboard.foreground = true
        assertNull(clipboard.takeAutomatic())
    }

    @Test
    fun automaticOffKeepsLatestSnapshotForManualForegroundCopy() {
        val clipboard = RemoteClipboard()
        clipboard.automatic = false
        clipboard.foreground = true
        clipboard.receive("old")
        assertNull(clipboard.takeAutomatic())
        clipboard.receive("latest")
        assertNull(clipboard.takeAutomatic())
        assertTrue(clipboard.row.enabled)
        assertEquals("latest", clipboard.take())
        assertFalse(clipboard.row.enabled)
        assertNull(clipboard.take())
    }

    @Test
    fun manualCopyCannotWriteWhileBackgroundedEvenWithAPendingSnapshot() {
        val clipboard = RemoteClipboard()
        clipboard.automatic = false
        clipboard.receive("old")
        clipboard.receive("latest")
        assertNull(clipboard.take())
        assertTrue(clipboard.row.enabled)
        clipboard.foreground = true
        assertEquals("latest", clipboard.take())
        assertFalse(clipboard.row.enabled)
    }

    @Test
    fun moreRowStateTracksPendingUpdatesIncludingEmptyTextAndUnicode() {
        val clipboard = RemoteClipboard()
        assertFalse(clipboard.row.enabled)
        clipboard.receive("")
        assertTrue(clipboard.row.enabled)
        assertEquals(0, clipboard.row.characters)
        clipboard.receive("A\uD83D\uDC08B")
        assertTrue(clipboard.row.enabled)
        assertEquals(3, clipboard.row.characters)
        clipboard.localAction()
        assertFalse(clipboard.row.enabled)
        assertEquals(0, clipboard.row.characters)
    }

    @Test
    fun turningAutomaticOnConsumesTheWaitingSnapshotOnlyInForeground() {
        val clipboard = RemoteClipboard()
        clipboard.automatic = false
        clipboard.receive("waiting")
        clipboard.automatic = true
        assertNull(clipboard.takeAutomatic())
        assertTrue(clipboard.row.enabled)
        clipboard.foreground = true
        assertEquals("waiting", clipboard.takeAutomatic())
        assertFalse(clipboard.row.enabled)
    }

    @Test
    fun manualEmptyTextCopyConsumesThePendingRow() {
        val clipboard = RemoteClipboard()
        clipboard.automatic = false
        clipboard.foreground = true
        clipboard.receive("")
        assertEquals("", clipboard.take())
        assertFalse(clipboard.row.enabled)
    }
}
