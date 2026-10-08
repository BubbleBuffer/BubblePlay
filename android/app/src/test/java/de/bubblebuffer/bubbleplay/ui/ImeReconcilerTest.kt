package de.bubblebuffer.bubbleplay.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/** Tests the platform-independent snapshot/batch contract, not a simulated Android IME. */
class ImeReconcilerTest {
    private val edits = ArrayList<Pair<TextEdit.Edit, Int>>()
    private var accepts = true
    private val state = ImeReconciler { edit, forward ->
        edits += edit to forward
        accepts
    }

    @Test fun composingDeleteThenSameCompositionDoesNotDeleteTwice() {
        state.changed("helo") // setComposingText
        val start = ImeReconciler.beforeStart("helo", 4, 1, false)!!
        state.changed("helo".substring(0, start)) // deleteSurroundingText
        state.changed("hel") // setComposingText
        assertEquals(listOf(TextEdit.Edit(0, "helo"), TextEdit.Edit(1, "")), edits.map { it.first })
        assertEquals("hel", state.remoteText)
    }

    @Test fun committedWordReconversionReplacesKnownTail() {
        state.changed("helo") // commitText
        state.changed("helo") // setComposingRegion changes spans only
        state.changed("hello") // commitText replaces that composing range
        assertEquals(listOf(TextEdit.Edit(0, "helo"), TextEdit.Edit(1, "lo")), edits.map { it.first })
        assertEquals("hello", state.remoteText)
    }

    @Test fun sameTextCommitAndFinishKeepCommittedContext() {
        state.changed("hello")
        state.changed("hello") // commitText clears composition, not text
        state.changed("hello") // finishComposingText clears spans, not text
        state.changed("hello world")
        assertEquals(listOf(TextEdit.Edit(0, "hello"), TextEdit.Edit(0, " world")), edits.map { it.first })
    }

    @Test fun nestedBatchesEmitOnlyTheFinalOuterSnapshot() {
        state.beginBatch()
        state.changed("h")
        state.beginBatch()
        state.changed("helo")
        state.changed("hello")
        assertTrue(state.endBatch())
        assertTrue(edits.isEmpty())
        state.changed("hello ")
        assertTrue(state.endBatch())
        assertEquals(listOf(TextEdit.Edit(0, "hello ")), edits.map { it.first })
        assertFalse(state.endBatch())
    }

    @Test fun internalBaseConnectionBatchesDeferUntilMethodReturns() {
        state.beginBatch()
        state.endBatch(reconcile = false)
        assertTrue(edits.isEmpty())
        state.changed("hello")
        assertEquals(1, edits.size)
    }

    @Test fun utf16AndCodePointDeletionHaveDifferentUnits() {
        val text = "a😀"
        assertEquals(2, ImeReconciler.beforeStart(text, 3, 1, false))
        assertEquals(1, ImeReconciler.beforeStart(text, 3, 2, false))
        assertEquals(1, ImeReconciler.beforeStart(text, 3, 1, true))
        assertEquals(0, ImeReconciler.beforeStart(text, 3, 2, true))
        assertNull(ImeReconciler.beforeStart("a\uD83D", 2, 1, true))
        state.changed(text)
        state.changed(text.substring(0, ImeReconciler.beforeStart(text, 3, 1, true)!!))
        assertEquals(TextEdit.Edit(1, ""), edits.last().first)
    }

    @Test fun failedAdmissionRetainsRemoteSnapshotAndPendingLocalText() {
        state.changed("helo")
        accepts = false
        assertFalse(state.changed("hello"))
        assertEquals("helo", state.remoteText)
        assertEquals("hello", state.localText)
        accepts = true
        assertTrue(state.reconcile())
        assertEquals(TextEdit.Edit(1, "lo"), edits.last().first)
        assertEquals("hello", state.remoteText)
    }

    @Test fun twoUtf16UnitDeletesDoNotSendAnUnpairedSurrogate() {
        state.changed("a😀")
        assertFalse(state.changed("a\uD83D"))
        assertEquals("a😀", state.remoteText)
        assertEquals(1, edits.size)
        assertTrue(state.changed("a"))
        assertEquals(TextEdit.Edit(1, ""), edits.last().first)
    }

    @Test fun wholeWordSwipeThenSpaceDoesNotDuplicateWord() {
        state.changed("bubble") // gesture composition
        state.changed("bubble") // same-word commit
        state.changed("bubble ")
        assertEquals(listOf(TextEdit.Edit(0, "bubble"), TextEdit.Edit(0, " ")), edits.map { it.first })
    }

    @Test fun reentrantResetCannotRestoreAStaleSnapshot() {
        lateinit var resetting: ImeReconciler
        resetting = ImeReconciler { _, _ -> resetting.reset(); true }
        assertFalse(resetting.changed("hello"))
        assertEquals("", resetting.remoteText)
        assertEquals("", resetting.localText)
    }

    @Test fun prefixTrimKeepsRemoteAndLocalAlignedAndDoesNotSplitEmoji() {
        val text = "a😀" + "z".repeat(ImeReconciler.MAX_CONTEXT - 1)
        state.changed(text)
        val trim = ImeReconciler.trimCount(text)
        assertEquals(3, trim)
        state.trimPrefix(trim)
        assertEquals(text.substring(trim), state.remoteText)
        assertEquals(state.remoteText, state.localText)
        state.changed(state.localText + "!")
        assertEquals(TextEdit.Edit(0, "!"), edits.last().first)
    }

    @Test fun deletesOutsideKnownContextAreDeferredWithTheBatch() {
        state.beginBatch()
        state.changed("", backspacesOutsideContext = 2, deletesOutsideContext = 1)
        assertTrue(edits.isEmpty())
        state.endBatch()
        assertEquals(TextEdit.Edit(2, "") to 1, edits.single())
    }

    @Test fun resetAbortsNestedOldConnectionAndNewInputDoesNotStall() {
        val old = state.openConnection()
        state.beginBatch(old)
        state.beginBatch(old)
        state.changed("older", connection = old)
        state.reset()
        assertEquals(0, state.batchDepth)
        val fresh = state.openConnection()
        state.beginBatch(fresh)
        assertFalse(state.beginBatch(old))
        assertFalse(state.endBatch(connection = old))
        assertFalse(state.changed("stale", connection = old))
        state.closeConnection(old)
        assertEquals(1, state.batchDepth)
        state.changed("new", connection = fresh)
        state.endBatch(connection = fresh)
        assertEquals(TextEdit.Edit(0, "new"), edits.last().first)
        state.changed("new!", connection = fresh)
        assertEquals(TextEdit.Edit(0, "!"), edits.last().first)
    }

    @Test fun replacementConnectionDropsOrphanedBatchOwnershipButKeepsText() {
        val old = state.openConnection()
        state.changed("old", connection = old)
        state.beginBatch(old)
        state.beginBatch(old)
        state.changed("older", connection = old)
        val fresh = state.openConnection()
        assertEquals(0, state.batchDepth)
        assertFalse(state.endBatch(connection = old))
        state.changed("older!", connection = fresh)
        assertEquals(TextEdit.Edit(0, "er!"), edits.last().first)
    }

    @Test fun keyBoundaryCommitsNestedPendingCompositionOnceThenResets() {
        val connection = state.openConnection()
        state.beginBatch(connection)
        state.beginBatch(connection)
        state.changed("hello", connection = connection)
        assertTrue(edits.isEmpty())
        assertTrue(state.beforeKeyEvent(connection))
        assertTrue(state.beforeKeyEvent(connection))
        assertEquals(listOf(TextEdit.Edit(0, "hello")), edits.map { it.first })
        assertEquals("hello", state.localText) // DOWN/UP preparation never clears prematurely.
        state.reset(invalidateConnection = false, preserveBatch = true) // Completed UP retires the segment.
        assertEquals(2, state.batchDepth)
        assertTrue(state.ownsConnection(connection))
        assertTrue(state.endBatch(connection = connection))
        assertEquals(1, state.batchDepth)
        assertTrue(state.endBatch(connection = connection))
        assertEquals(0, state.batchDepth)
        state.changed("next", connection = connection)
        assertEquals(TextEdit.Edit(0, "next"), edits.last().first)
    }

    @Test fun rejectedKeyBoundaryKeepsPendingCompositionAndRemoteSnapshot() {
        val connection = state.openConnection()
        state.beginBatch(connection)
        state.beginBatch(connection)
        state.changed("hello", connection = connection)
        accepts = false
        assertFalse(state.beforeKeyEvent(connection))
        assertEquals("", state.remoteText)
        assertEquals("hello", state.localText)
        assertEquals(2, state.batchDepth)
    }

    @Test fun deletingBeyondRetainedPrefixThenRecommittingSameTextRewritesWholeSegment() {
        var host = "zz"
        val sent = ArrayList<TextEdit.Edit>()
        val mirrored = ImeReconciler { edit, _ ->
            sent += edit
            host = applyEdit(host, edit)
            true
        }
        mirrored.changed("abc")
        assertEquals("zzabc", host)
        mirrored.beginBatch()
        mirrored.changed("", backspacesOutsideContext = 2) // deleteSurroundingText(5, 0)
        mirrored.changed("abc") // commitText("abc", 1)
        mirrored.endBatch()
        assertEquals(TextEdit.Edit(5, "abc"), sent.last())
        assertEquals("abc", host)
    }

    @Test fun multipleOutsideDeletesAndFailedAdmissionKeepFullReplacementPending() {
        var host = "yyyzz"
        var admitted = true
        val sent = ArrayList<TextEdit.Edit>()
        val mirrored = ImeReconciler { edit, _ ->
            sent += edit
            if (admitted) host = applyEdit(host, edit)
            admitted
        }
        mirrored.changed("abc")
        mirrored.beginBatch()
        mirrored.changed("", backspacesOutsideContext = 2)
        mirrored.changed("abc")
        admitted = false
        assertFalse(mirrored.endBatch())
        assertEquals("yyyzzabc", host)
        assertEquals("abc", mirrored.remoteText)
        assertTrue(mirrored.hasPendingEdits) // Same text can still represent a required replacement.
        mirrored.beginBatch()
        mirrored.changed("", backspacesOutsideContext = 1)
        mirrored.changed("abc")
        admitted = true
        mirrored.endBatch()
        assertEquals(TextEdit.Edit(6, "abc"), sent.last())
        assertEquals("yyabc", host)
        assertFalse(mirrored.hasPendingEdits)
        mirrored.changed("abc!")
        assertEquals(TextEdit.Edit(0, "!"), sent.last())
        assertEquals("yyabc!", host)
    }

    @Test fun replacementConnectionRetainsOutsideDeleteButResetDiscardsIt() {
        val old = state.openConnection()
        state.changed("abc", connection = old)
        state.beginBatch(old)
        state.changed("", backspacesOutsideContext = 2, connection = old)
        val fresh = state.openConnection()
        assertFalse(state.changed("stale", connection = old))
        state.changed("abc", connection = fresh)
        assertEquals(TextEdit.Edit(5, "abc"), edits.last().first)
        state.beginBatch(fresh)
        state.changed("", backspacesOutsideContext = 2, connection = fresh)
        state.reset()
        val reset = state.openConnection()
        state.changed("abc", connection = reset)
        assertEquals(TextEdit.Edit(0, "abc"), edits.last().first)
    }

    @Test fun spacePrintableAndTabKeysFollowPendingTextBeforeOuterBatchEnds() {
        for (keyText in listOf(" ", "x", "\t")) {
            var host = ""
            val sent = ArrayList<TextEdit.Edit>()
            val mirrored = ImeReconciler { edit, _ ->
                sent += edit
                host = applyEdit(host, edit)
                true
            }
            val connection = mirrored.openConnection()
            mirrored.beginBatch(connection)
            mirrored.beginBatch(connection)
            mirrored.changed("hello", connection = connection)
            assertTrue(mirrored.beforeKeyEvent(connection)) // DOWN
            host += keyText
            assertTrue(mirrored.beforeKeyEvent(connection)) // UP
            mirrored.reset(invalidateConnection = false, preserveBatch = true)
            assertTrue(mirrored.endBatch(connection = connection))
            assertEquals(1, mirrored.batchDepth)
            assertTrue(mirrored.endBatch(connection = connection))
            assertEquals("hello$keyText", host)
            assertEquals(listOf(TextEdit.Edit(0, "hello")), sent)
        }
    }

    @Test fun staleKeyBoundaryAfterRestartCannotFlushOrCloseNewOwnersBatch() {
        val old = state.openConnection()
        state.beginBatch(old)
        state.changed("hello", connection = old)
        assertTrue(state.beforeKeyEvent(old))
        state.reset() // Navigation key restarts input inside its sink.
        val fresh = state.openConnection()
        state.beginBatch(fresh)
        state.changed("next", connection = fresh)
        assertFalse(state.beforeKeyEvent(old))
        assertEquals(1, state.batchDepth)
        assertEquals("next", state.localText)
        assertEquals(1, edits.size)
        state.endBatch(connection = fresh)
        assertEquals(TextEdit.Edit(0, "next"), edits.last().first)
    }

    @Test fun printableKeyFlushesPriorWordButLaterCommitsWaitForRemainingOuterBatch() {
        var host = ""
        val sent = ArrayList<TextEdit.Edit>()
        val mirrored = ImeReconciler { edit, _ ->
            sent += edit
            host = applyEdit(host, edit)
            true
        }
        val connection = mirrored.openConnection()
        mirrored.beginBatch(connection)
        mirrored.beginBatch(connection)
        mirrored.changed("hello", connection = connection)
        assertTrue(mirrored.beforeKeyEvent(connection)) // Space DOWN
        host += " "
        assertTrue(mirrored.beforeKeyEvent(connection)) // Space UP
        mirrored.reset(invalidateConnection = false, preserveBatch = true)
        assertEquals(2, mirrored.batchDepth)
        mirrored.changed("n", connection = connection)
        mirrored.changed("next", connection = connection)
        assertEquals("hello ", host)
        assertEquals(1, sent.size)
        mirrored.endBatch(connection = connection)
        mirrored.changed("next word", connection = connection)
        assertEquals("hello ", host)
        assertEquals(1, sent.size)
        mirrored.endBatch(connection = connection)
        assertEquals(listOf(TextEdit.Edit(0, "hello"), TextEdit.Edit(0, "next word")), sent)
        assertEquals("hello next word", host)
        assertEquals(0, mirrored.batchDepth)
    }

    @Test fun replacementEditableSnapshotIsObservedAfterBaseInternalBatchCompletes() {
        state.changed("helo")
        state.beginBatch() // BaseInputConnection.replaceText's internal batch.
        state.endBatch(reconcile = false) // The wrapped superclass method is still running.
        assertEquals(1, edits.size)
        state.changed("hello") // Wrapper observes the now-mutated Editable.
        assertEquals(TextEdit.Edit(1, "lo"), edits.last().first)
        assertEquals("hello", state.remoteText)
    }

    private fun applyEdit(host: String, edit: TextEdit.Edit): String {
        val remaining = (host.codePointCount(0, host.length) - edit.deleteCount).coerceAtLeast(0)
        return host.substring(0, host.offsetByCodePoints(0, remaining)) + edit.insert
    }
}
