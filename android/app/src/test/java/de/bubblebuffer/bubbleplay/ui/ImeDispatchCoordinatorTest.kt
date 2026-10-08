package de.bubblebuffer.bubbleplay.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/** Uses the real coordinator/reconciler and real text/latch senders, without Android objects. */
class ImeDispatchCoordinatorTest {
    private class Editor {
        var local = ""
        var composing = false
        var acceptsText = true
        var inputOpen = true
        var acceptsKeys = true
        var restarts = 0
        val packets = ArrayList<String>()
        val keyAttempts = ArrayList<String>()
        val clears = ArrayList<String>()
        val posted = ArrayList<() -> Unit>()
        val latches = linkedSetOf<String>()
        private val keys = LatchedKeySender(latches, LinkedHashMap(), { code, up ->
            keyAttempts += "$code:$up"
            if (acceptsKeys) packets += "$code:$up"
            acceptsKeys
        }, {})
        private val sender: ImeTextSender by lazy {
            ImeTextSender({ text ->
                if (acceptsText) packets += "text:$text"
                acceptsText
            }, { code -> keys.down(code) && keys.up(code) },
                { code -> keys.down(code) && keys.up(code) }, { latches.isNotEmpty() }, { dispatch.requestReset() })
        }
        val state: ImeReconciler = ImeReconciler { edit, _ -> inputOpen && sender.send(edit.deleteCount, edit.insert) }
        var connection = state.openConnection()
        val dispatch: ImeDispatchCoordinator = ImeDispatchCoordinator(state::ownsConnection, state::beforeKeyEvent, { invalidate ->
            val needed = local.isNotEmpty() || composing || state.hasPendingEdits
            clears += state.remoteText
            local = ""
            composing = false
            state.reset(invalidateConnection = invalidate, preserveBatch = !invalidate)
            needed
        }, { restarts++ }, { posted += it })

        fun edit(text: String, composition: Boolean = false) {
            dispatch.beginOperation(editorEdit = true)
            try { local = text; composing = composition; state.changed(text, connection = connection) }
            finally { dispatch.endOperation() }
        }

        fun finish() = edit(local, composition = false)

        fun key(action: ImeDispatchCoordinator.Action, code: String = "Enter", token: Int = connection): Boolean =
            dispatch.dispatch(token, code.hashCode(), action, resetAfter = true,
                restartAfter = ImeDispatchCoordinator.needsRestart(code), deduplicateDown = code == "Enter") {
                when (action) {
                    ImeDispatchCoordinator.Action.DOWN -> keys.down(code)
                    ImeDispatchCoordinator.Action.UP -> keys.up(code)
                    ImeDispatchCoordinator.Action.MULTIPLE -> keys.down(code) && keys.up(code)
                    ImeDispatchCoordinator.Action.OTHER -> false
                }
            }

        fun action(): Boolean = dispatch.editorAction(connection, "Enter".hashCode(),
            { keys.down("Enter") }, { keys.up("Enter") })

        fun replaceConnection() { dispatch.connectionChanged(); connection = state.openConnection() }
        fun runPosted() { while (posted.isNotEmpty()) posted.removeAt(0)() }
    }

    @Test fun composingHelloThenEnterDownUpFlushesOnceAndDoesNotRestartBeforeUp() {
        val editor = Editor()
        editor.state.beginBatch(editor.connection)
        editor.edit("hello", composition = true)
        assertTrue(editor.key(ImeDispatchCoordinator.Action.DOWN))
        assertEquals(listOf("text:hello", "Enter:false"), editor.packets)
        assertEquals("hello", editor.local)
        assertEquals("hello", editor.state.remoteText)
        assertTrue(editor.posted.isEmpty())
        assertEquals(0, editor.restarts)
        assertTrue(editor.key(ImeDispatchCoordinator.Action.UP))
        assertEquals(listOf("text:hello", "Enter:false", "Enter:true"), editor.packets)
        assertEquals("", editor.local)
        assertEquals("", editor.state.remoteText)
        assertEquals(1, editor.state.batchDepth)
        assertEquals(0, editor.restarts)
        editor.runPosted()
        assertEquals(1, editor.restarts)
    }

    @Test fun composingHelloThenEditorActionSendsOneExplicitPair() {
        val editor = Editor()
        editor.state.beginBatch(editor.connection)
        editor.edit("hello", composition = true)
        assertTrue(editor.action())
        assertEquals(listOf("text:hello", "Enter:false", "Enter:true"), editor.packets)
        assertEquals("", editor.local)
        assertEquals(0, editor.restarts)
        editor.runPosted()
        assertEquals(1, editor.restarts)
    }

    @Test fun committedHelloThenEnterDownUpDoesNotResendCommittedText() {
        val editor = Editor()
        editor.edit("hello")
        editor.key(ImeDispatchCoordinator.Action.DOWN)
        editor.key(ImeDispatchCoordinator.Action.UP)
        assertEquals(listOf("text:hello", "Enter:false", "Enter:true"), editor.packets)
    }

    @Test fun finishCompositionThenEnterInNestedBatchPreservesBothEnds() {
        val editor = Editor()
        editor.state.beginBatch(editor.connection)
        editor.state.beginBatch(editor.connection)
        editor.edit("hello", composition = true)
        editor.finish()
        assertTrue(editor.packets.isEmpty())
        editor.key(ImeDispatchCoordinator.Action.DOWN)
        editor.key(ImeDispatchCoordinator.Action.UP)
        assertEquals(2, editor.state.batchDepth)
        assertTrue(editor.state.endBatch(connection = editor.connection))
        assertTrue(editor.state.endBatch(connection = editor.connection))
        assertEquals(listOf("text:hello", "Enter:false", "Enter:true"), editor.packets)
    }

    @Test fun newlineCommitRetiresOnlyAfterAcceptedSnapshotThenNextWordSurvivesPostedWork() {
        val editor = Editor()
        editor.edit("hello\n")
        assertEquals(listOf("text:hello", "Enter:false", "Enter:true"), editor.packets)
        assertEquals(listOf("hello\n"), editor.clears) // Snapshot was accepted before retirement.
        assertEquals("", editor.state.remoteText)
        assertEquals(0, editor.restarts)
        editor.edit("next", composition = true)
        editor.runPosted()
        assertEquals("next", editor.local)
        assertEquals("next", editor.state.remoteText)
        assertEquals(0, editor.restarts)
        assertEquals("text:next", editor.packets.last())
    }

    @Test fun nextWordImmediatelyAfterEnterUpIsAnIndependentSegmentAndCancelsRestart() {
        val editor = Editor()
        editor.edit("hello")
        editor.key(ImeDispatchCoordinator.Action.DOWN)
        editor.key(ImeDispatchCoordinator.Action.UP)
        editor.edit("next", composition = true)
        editor.runPosted()
        assertEquals(listOf("text:hello", "Enter:false", "Enter:true", "text:next"), editor.packets)
        assertEquals("next", editor.local)
        assertEquals(0, editor.restarts)
    }

    @Test fun emptyEnterAndEmptyEditorActionNeedNoImeRestart() {
        val editor = Editor()
        editor.key(ImeDispatchCoordinator.Action.DOWN)
        editor.key(ImeDispatchCoordinator.Action.UP)
        assertTrue(editor.posted.isEmpty())
        assertTrue(editor.action())
        assertTrue(editor.posted.isEmpty())
        assertEquals(listOf("Enter:false", "Enter:true", "Enter:false", "Enter:true"), editor.packets)
        assertEquals(0, editor.restarts)
    }

    @Test fun latchedEditorActionUsesProductionModifierDownEnterPairModifierUpOrdering() {
        val editor = Editor()
        editor.latches += listOf("ControlLeft", "ShiftLeft")
        editor.edit("hello", composition = true)
        editor.action()
        assertEquals(listOf("text:hello", "ControlLeft:false", "ShiftLeft:false", "Enter:false",
            "Enter:true", "ShiftLeft:true", "ControlLeft:true"), editor.packets)
        assertTrue(editor.latches.isEmpty())
        assertEquals(0, editor.restarts)
    }

    @Test fun enterRepeatsAndDuplicateUpDoNotDuplicateOneGesture() {
        val editor = Editor()
        editor.edit("hello")
        editor.key(ImeDispatchCoordinator.Action.DOWN)
        editor.key(ImeDispatchCoordinator.Action.DOWN)
        editor.key(ImeDispatchCoordinator.Action.DOWN)
        editor.key(ImeDispatchCoordinator.Action.UP)
        editor.key(ImeDispatchCoordinator.Action.UP)
        assertEquals(listOf("text:hello", "Enter:false", "Enter:true"), editor.packets)
    }

    @Test fun gatedPendingTextPreventsEnterPacketsAndDoesNotLoseTheWord() {
        val editor = Editor()
        editor.state.beginBatch(editor.connection)
        editor.edit("hello", composition = true)
        editor.inputOpen = false
        assertFalse(editor.key(ImeDispatchCoordinator.Action.DOWN))
        assertEquals("hello", editor.local)
        assertEquals("", editor.state.remoteText)
        assertTrue(editor.keyAttempts.isEmpty())
        assertEquals(0, editor.restarts)
    }

    @Test fun midSendFailureRetiresUnknownContextAfterCallbackWithoutSendingEnter() {
        val editor = Editor()
        editor.state.beginBatch(editor.connection)
        editor.edit("hello", composition = true)
        editor.acceptsText = false
        assertFalse(editor.key(ImeDispatchCoordinator.Action.DOWN))
        assertTrue(editor.keyAttempts.isEmpty())
        assertEquals("", editor.local)
        assertEquals("", editor.state.remoteText)
        assertEquals(0, editor.restarts)
        editor.runPosted()
        assertEquals(1, editor.restarts)
    }

    @Test fun rejectedReconciliationAfterAcceptedDownStillAttemptsItsRelease() {
        var rejectFlush = false
        val keys = ArrayList<String>()
        val coordinator = ImeDispatchCoordinator({ true }, { !rejectFlush }, { true }, {}, {})
        coordinator.dispatch(1, 66, ImeDispatchCoordinator.Action.DOWN, true) { keys += "down"; true }
        rejectFlush = true
        coordinator.dispatch(1, 66, ImeDispatchCoordinator.Action.UP, true) { keys += "up"; true }
        assertEquals(listOf("down", "up"), keys)
    }

    @Test fun staleReleaseAndPostedResetCannotClearAReplacementConnectionsWord() {
        val editor = Editor()
        editor.edit("hello")
        editor.key(ImeDispatchCoordinator.Action.DOWN)
        val old = editor.connection
        editor.replaceConnection()
        editor.edit("next")
        editor.key(ImeDispatchCoordinator.Action.UP, token = old)
        editor.runPosted()
        assertEquals("next", editor.local)
        assertEquals("next", editor.state.remoteText)
        assertEquals(0, editor.restarts)
        assertEquals("Enter:true", editor.packets.last())
    }

    @Test fun replacingConnectionAfterCompletedUpCancelsItsPostedRestart() {
        val editor = Editor()
        editor.edit("hello")
        editor.key(ImeDispatchCoordinator.Action.DOWN)
        editor.key(ImeDispatchCoordinator.Action.UP)
        editor.replaceConnection()
        editor.edit("next")
        editor.runPosted()
        assertEquals("next", editor.local)
        assertEquals(0, editor.restarts)
    }

    @Test fun spaceRetiresUnknownRawTailWithoutRestartAndKeepsNestedBatching() {
        val editor = Editor()
        editor.state.beginBatch(editor.connection)
        editor.state.beginBatch(editor.connection)
        editor.edit("hello", composition = true)
        editor.key(ImeDispatchCoordinator.Action.DOWN, "Space")
        assertEquals("hello", editor.local)
        editor.key(ImeDispatchCoordinator.Action.UP, "Space")
        assertEquals("", editor.local)
        assertTrue(editor.posted.isEmpty())
        editor.edit("n")
        editor.edit("next")
        editor.state.endBatch(connection = editor.connection)
        assertEquals(listOf("text:hello", "Space:false", "Space:true"), editor.packets)
        editor.state.endBatch(connection = editor.connection)
        assertEquals("text:next", editor.packets.last())
        assertEquals(0, editor.restarts)
        assertFalse(ImeDispatchCoordinator.needsRestart("KeyA"))
        assertTrue(ImeDispatchCoordinator.needsRestart("ArrowLeft"))
    }

    @Test fun multipleSelfContainedEnterTapRetiresAfterBothPacketsAndPostsRestart() {
        val editor = Editor()
        editor.edit("hello")
        editor.key(ImeDispatchCoordinator.Action.MULTIPLE)
        assertTrue(editor.key(ImeDispatchCoordinator.Action.UP))
        assertEquals(listOf("text:hello", "Enter:false", "Enter:true"), editor.packets)
        assertEquals("", editor.local)
        assertEquals(0, editor.restarts)
        editor.runPosted()
        assertEquals(1, editor.restarts)
    }

    @Test fun multipleWhileEnterDownIsHeldDoesNotAddAnotherTap() {
        val editor = Editor()
        editor.edit("hello")
        editor.key(ImeDispatchCoordinator.Action.DOWN)
        editor.key(ImeDispatchCoordinator.Action.MULTIPLE)
        editor.key(ImeDispatchCoordinator.Action.UP)
        assertEquals(listOf("text:hello", "Enter:false", "Enter:true"), editor.packets)
    }

    @Test fun externalResetOutsideImePreservesImmediateRestartButInCallbackDefersIt() {
        val editor = Editor()
        editor.edit("hello")
        editor.dispatch.beginOperation()
        editor.dispatch.externalReset()
        assertEquals("hello", editor.local)
        assertEquals(0, editor.restarts)
        editor.dispatch.endOperation()
        assertEquals("", editor.local)
        assertEquals(0, editor.restarts)
        editor.runPosted()
        assertEquals(1, editor.restarts)
        editor.dispatch.externalReset()
        assertEquals(2, editor.restarts)
    }
}
