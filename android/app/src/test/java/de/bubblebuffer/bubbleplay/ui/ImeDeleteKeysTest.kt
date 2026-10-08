package de.bubblebuffer.bubbleplay.ui

import java.text.BreakIterator
import java.util.Locale
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/** Exercises the production router and reconciliation; Android supplies ICU boundaries. */
class ImeDeleteKeysTest {
    private class Editor(
        text: String = "",
        hostPrefix: String = "",
        preceding: (String, Int) -> Int = { value, cursor ->
            BreakIterator.getCharacterInstance(Locale.ROOT).apply { setText(value) }.preceding(cursor)
        },
    ) {
        var local = text
        var host = hostPrefix
        var accepts = true
        val edits = ArrayList<Pair<TextEdit.Edit, Int>>()
        val state = ImeReconciler { edit, forward ->
            edits += edit to forward
            if (accepts) {
                val count = (host.codePointCount(0, host.length) - edit.deleteCount).coerceAtLeast(0)
                host = host.substring(0, host.offsetByCodePoints(0, count)) + edit.insert
            }
            accepts
        }
        var connection = state.openConnection()
        var router = ImeDeleteKeys(preceding)

        init { state.changed(text, connection = connection); edits.clear() }

        fun text(value: String) { local = value; state.changed(value, connection = connection) }

        fun key(kind: ImeDeleteKeys.Kind = ImeDeleteKeys.Kind.BACKWARD,
            action: ImeDeleteKeys.Action = ImeDeleteKeys.Action.DOWN, repeats: Int = 0): Boolean {
            return when (val route = router.route(kind, action, repeats, state.ownsConnection(connection), true, local)) {
                ImeDeleteKeys.Route.Pass -> false
                ImeDeleteKeys.Route.Swallow -> true
                is ImeDeleteKeys.Route.Delete -> {
                    state.beforeDeleteKey(connection)
                    if (!state.ownsConnection(connection)) false
                    else {
                        local = local.substring(0, route.start)
                        state.deletedByKey(local, route.outsideBackspaces, route.outsideForwardDeletes, connection)
                        state.ownsConnection(connection)
                    }
                }
            }
        }
    }

    @Test fun localDelEmitsOneBackspaceAndKeepsTheRemainingWord() {
        val editor = Editor("abc")
        assertTrue(editor.key())
        assertEquals("ab", editor.local)
        assertEquals("ab", editor.state.remoteText)
        assertEquals("ab", editor.host)
        assertEquals(listOf(TextEdit.Edit(1, "") to 0), editor.edits)
        assertTrue(editor.key(action = ImeDeleteKeys.Action.UP))
        assertEquals("ab", editor.local)
        assertEquals(1, editor.edits.size)
    }

    @Test fun repeatedDownsDeleteTextThenSendOneOutsideTapPerDown() {
        val editor = Editor("ab", hostPrefix = "zz")
        assertTrue(editor.key(repeats = 0))
        assertTrue(editor.key(repeats = 1))
        assertTrue(editor.key(repeats = 2))
        assertTrue(editor.key(repeats = 3))
        assertEquals("", editor.local)
        assertEquals("", editor.host)
        assertEquals(List(4) { TextEdit.Edit(1, "") to 0 }, editor.edits)
    }

    @Test fun multipleCarriesItsCountAcrossTheKnownSegmentBoundary() {
        val editor = Editor("ab", hostPrefix = "zz")
        assertTrue(editor.key(action = ImeDeleteKeys.Action.MULTIPLE, repeats = 4))
        assertEquals("", editor.host)
        assertEquals(TextEdit.Edit(4, "") to 0, editor.edits.single())
        assertTrue(editor.key(action = ImeDeleteKeys.Action.MULTIPLE, repeats = 0))
        assertEquals(1, editor.edits.size)
    }

    @Test fun emojiAndCombiningGraphemesMapToRemovedCodePointCounts() {
        val editor = Editor("x😀e\u0301")
        editor.key()
        assertEquals("x😀", editor.local)
        assertEquals(TextEdit.Edit(2, "") to 0, editor.edits.last())
        editor.key()
        assertEquals("x", editor.local)
        assertEquals("x", editor.host)
        assertEquals(TextEdit.Edit(1, "") to 0, editor.edits.last())
    }

    @Test fun injectedExtendedClusterBoundaryDeletesFamilyBySevenCodePoints() {
        // java.text on the JVM does not promise Android ICU's modern ZWJ boundaries.
        // Inject the ICU boundary contract and test routing/count translation separately.
        val family = "👨‍👩‍👧‍👦"
        val editor = Editor("x$family", preceding = { _, _ -> 1 })
        editor.key()
        assertEquals("x", editor.local)
        assertEquals("x", editor.host)
        assertEquals(TextEdit.Edit(7, "") to 0, editor.edits.single())
    }

    @Test fun deletingCommittedSpaceLeavesContextForReplacementOrCompositionCorrection() {
        for (correction in listOf("replaceText", "setComposingRegion+commitText")) {
            val editor = Editor("helo ")
            editor.key()
            assertEquals("helo", editor.local)
            editor.text("hello") // Snapshot after the named Android mutation.
            assertEquals(correction, "hello", editor.host)
            assertEquals(listOf(TextEdit.Edit(1, "") to 0, TextEdit.Edit(1, "lo") to 0), editor.edits)
        }
    }

    @Test fun deleteFlushesPendingNestedWordThenEditsWithoutClearingBatchOrContext() {
        val editor = Editor()
        editor.state.beginBatch(editor.connection)
        editor.state.beginBatch(editor.connection)
        editor.text("hello")
        assertTrue(editor.edits.isEmpty())
        editor.key()
        assertEquals("hell", editor.local)
        assertEquals("hell", editor.host)
        assertEquals(2, editor.state.batchDepth)
        assertEquals(listOf(TextEdit.Edit(0, "hello") to 0, TextEdit.Edit(1, "") to 0), editor.edits)
        editor.text("hello")
        editor.state.endBatch(connection = editor.connection)
        editor.text("hello!")
        assertEquals(2, editor.edits.size)
        editor.state.endBatch(connection = editor.connection)
        assertEquals("hello!", editor.host)
        assertEquals(TextEdit.Edit(0, "o!") to 0, editor.edits.last())
    }

    @Test fun gatedDeleteStillEditsLocalCompositionAndLaterRetriesTheReducedWord() {
        val editor = Editor()
        editor.accepts = false
        editor.text("helo")
        assertTrue(editor.key()) // Locally handled; gated admission is not an invalid connection.
        assertEquals("hel", editor.local)
        assertEquals("hel", editor.state.localText)
        assertEquals("", editor.state.remoteText)
        editor.key(action = ImeDeleteKeys.Action.UP)
        assertEquals("hel", editor.local)
        editor.accepts = true
        assertTrue(editor.state.reconcile())
        assertEquals("hel", editor.host)
        assertEquals(TextEdit.Edit(0, "hel") to 0, editor.edits.last())
    }

    @Test fun deleteUpIsSwallowedForStaleAndReplacementConnectionsWithoutClearingText() {
        val editor = Editor("abc")
        editor.key()
        val old = editor.connection
        editor.connection = editor.state.openConnection()
        assertTrue(editor.router.route(ImeDeleteKeys.Kind.BACKWARD, ImeDeleteKeys.Action.UP, 0,
            editor.state.ownsConnection(old), true, editor.local) == ImeDeleteKeys.Route.Swallow)
        editor.router = ImeDeleteKeys { _, _ -> 0 }
        assertTrue(editor.key(action = ImeDeleteKeys.Action.UP))
        assertEquals("ab", editor.local)
        assertEquals("ab", editor.state.remoteText)
        assertEquals(1, editor.edits.size)
    }

    @Test fun forwardDeleteUsesLocalTrailingClusterThenForwardTapsOutsideContext() {
        val editor = Editor("a")
        editor.key(ImeDeleteKeys.Kind.FORWARD)
        assertEquals(TextEdit.Edit(1, "") to 0, editor.edits.last())
        editor.key(ImeDeleteKeys.Kind.FORWARD, repeats = 2)
        assertEquals(TextEdit.Edit(0, "") to 1, editor.edits.last())
        editor.key(ImeDeleteKeys.Kind.FORWARD, ImeDeleteKeys.Action.MULTIPLE, repeats = 3)
        assertEquals(TextEdit.Edit(0, "") to 3, editor.edits.last())
    }

    @Test fun unrelatedKeysAndStaleDeleteDownUseExistingKeyPath() {
        val router = ImeDeleteKeys { _, _ -> 0 }
        assertEquals(ImeDeleteKeys.Route.Pass, router.route(null, ImeDeleteKeys.Action.DOWN, 0, true, true, "abc"))
        assertEquals(ImeDeleteKeys.Route.Pass, router.route(ImeDeleteKeys.Kind.BACKWARD,
            ImeDeleteKeys.Action.DOWN, 0, false, true, "abc"))
    }

    @Test fun disabledProductionTraceGateDoesNotBuildOrEmitMetadata() {
        var reads = 0
        val emitted = ArrayList<String>()
        val trace = ImeTrace({ false }, { emitted += it })
        trace.record(ImeTrace.Method.COMMIT) { reads++; mapOf("textLength" to 12) }
        assertEquals(0, reads)
        assertTrue(emitted.isEmpty())
    }

    @Test fun enabledTraceUsesApiNamesAndNumericLengthsOnly() {
        val emitted = ArrayList<String>()
        val trace = ImeTrace({ true }, { emitted += it })
        trace.record(ImeTrace.Method.COMMIT) { linkedMapOf("textLength" to 12, "result" to 1) }
        trace.record(ImeTrace.Method.EDIT) { linkedMapOf("deleteCount" to 2, "insertLength" to 4) }
        assertEquals(listOf("commitText textLength=12 result=1", "emitEdit deleteCount=2 insertLength=4"), emitted)
    }
}
