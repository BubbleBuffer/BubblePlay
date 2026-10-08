package de.bubblebuffer.bubbleplay.ui

import android.content.Context
import android.annotation.TargetApi
import android.icu.text.BreakIterator
import android.os.SystemClock
import android.text.Editable
import android.text.InputType
import android.text.Selection
import android.text.SpannableStringBuilder
import android.util.Log
import android.view.KeyEvent
import android.view.KeyCharacterMap
import android.view.View
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager
import android.view.inputmethod.TextAttribute
import de.bubblebuffer.bubbleplay.protocol.KeyMap

/** Invisible full editor for IME suggestions, reconversion and gesture typing. */
class KeyboardInputView(context: Context, private val sink: Sink) : View(context) {
    interface Sink {
        fun onImeKey(event: KeyEvent): Boolean
        /** True only when the entire tail edit was queued. Reset context on partial failure. */
        fun onImeEdit(deleteCount: Int, insert: String): Boolean
        fun onImeDeleteForward(count: Int): Boolean
    }

    private val editable: Editable = SpannableStringBuilder()
    private val imm get() = context.getSystemService(InputMethodManager::class.java)
    private val imeTrace = ImeTrace({ Log.isLoggable("BubblePlayIme", Log.DEBUG) }, { Log.d("BubblePlayIme", it) })
    private val state = ImeReconciler { edit, forward ->
        trace(ImeTrace.Method.EDIT, "deleteCount" to edit.deleteCount, "insertLength" to edit.insert.length,
            "forwardCount" to forward)
        val hasEdit = edit.deleteCount > 0 || edit.insert.isNotEmpty()
        val accepted = if (hasEdit && !sink.onImeEdit(edit.deleteCount, edit.insert)) false
        else if (forward > 0 && !sink.onImeDeleteForward(forward)) {
            // The preceding tail edit was already queued; its old snapshot cannot be retried.
            if (hasEdit) resetContext()
            false
        } else true
        trace(ImeTrace.Method.EDIT_RESULT, "accepted" to if (accepted) 1 else 0)
        accepted
    }
    private val dispatch by lazy {
        ImeDispatchCoordinator(state::ownsConnection,
            { connection -> caretAtEnd() && state.beforeKeyEvent(connection) },
            { invalidate ->
                val needed = editable.isNotEmpty() || state.hasPendingEdits ||
                    BaseInputConnection.getComposingSpanStart(editable) >= 0
                trace(ImeTrace.Method.RESET, "invalidateConnection" to if (invalidate) 1 else 0)
                state.reset(invalidateConnection = invalidate, preserveBatch = !invalidate)
                clearEditableContext()
                needed
            },
            {
                trace(ImeTrace.Method.RESTART)
                imm?.restartInput(this)
            },
            { action -> post { action() } })
    }

    init {
        isFocusable = true
        isFocusableInTouchMode = true
        Selection.setSelection(editable, 0)
    }

    /** Call after remote caret navigation, clicks, paste, desktop changes or signaling loss. */
    fun resetContext() {
        dispatch.externalReset()
    }

    /** Sink callbacks retire the segment after reconciliation, then post only the IME restart. */
    fun requestDeferredResetContext() {
        trace(ImeTrace.Method.DEFERRED_RESET)
        dispatch.requestReset()
    }

    private fun clearEditableContext() {
        editable.clear()
        BaseInputConnection.removeComposingSpans(editable)
        Selection.setSelection(editable, 0)
        updateSelection()
    }

    override fun onCheckIsTextEditor() = true

    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection {
        dispatch.connectionChanged()
        trace(ImeTrace.Method.CREATE)
        outAttrs.inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_AUTO_CORRECT
        outAttrs.imeOptions = EditorInfo.IME_FLAG_NO_EXTRACT_UI or EditorInfo.IME_FLAG_NO_FULLSCREEN or
            EditorInfo.IME_ACTION_NONE or EditorInfo.IME_FLAG_NO_PERSONALIZED_LEARNING
        outAttrs.initialSelStart = Selection.getSelectionStart(editable)
        outAttrs.initialSelEnd = Selection.getSelectionEnd(editable)
        return Connection(state.openConnection())
    }

    private fun updateSelection() {
        imm?.updateSelection(this, Selection.getSelectionStart(editable), Selection.getSelectionEnd(editable),
            BaseInputConnection.getComposingSpanStart(editable), BaseInputConnection.getComposingSpanEnd(editable))
    }

    private fun trace(method: ImeTrace.Method, vararg details: Pair<String, Int>) {
        imeTrace.record(method) {
            linkedMapOf("length" to editable.length, "selectionStart" to Selection.getSelectionStart(editable),
                "selectionEnd" to Selection.getSelectionEnd(editable),
                "compositionStart" to BaseInputConnection.getComposingSpanStart(editable),
                "compositionEnd" to BaseInputConnection.getComposingSpanEnd(editable),
                "batchDepth" to state.batchDepth).apply { putAll(details) }
        }
    }

    private fun caretAtEnd() = Selection.getSelectionStart(editable) == editable.length &&
        Selection.getSelectionEnd(editable) == editable.length

    private fun settled() {
        if (state.batchDepth != 0) return
        // A local non-tail caret has no corresponding host navigation contract.
        if (!caretAtEnd()) {
            resetContext()
            return
        }
        val trim = ImeReconciler.trimCount(editable.toString())
        if (trim > 0) {
            if (state.hasPendingEdits) {
                // Bound disconnected/failed oversized compositions instead of retaining them indefinitely.
                resetContext()
                return
            }
            state.trimPrefix(trim)
            editable.delete(0, trim)
        }
        updateSelection()
    }

    private inner class Connection(private val generation: Int) : BaseInputConnection(this@KeyboardInputView, true) {
        private var operationDepth = 0
        private val staleEditable: Editable = SpannableStringBuilder().also { Selection.setSelection(it, 0) }
        private val deleteKeys = ImeDeleteKeys { text, cursor ->
            BreakIterator.getCharacterInstance().apply { setText(text) }.preceding(cursor)
        }
        private fun current() = state.ownsConnection(generation)
        override fun getEditable(): Editable = if (current()) this@KeyboardInputView.editable else staleEditable

        override fun beginBatchEdit(): Boolean {
            trace(ImeTrace.Method.BEGIN_BATCH)
            return state.beginBatch(generation)
        }

        override fun endBatchEdit(): Boolean {
            trace(ImeTrace.Method.END_BATCH)
            if (!current() || state.batchDepth == 0) return false
            dispatch.beginOperation()
            try {
                if (operationDepth == 0 && state.batchDepth == 1 && !caretAtEnd()) {
                    requestDeferredResetContext()
                    state.endBatch(reconcile = false, connection = generation)
                } else state.endBatch(reconcile = operationDepth == 0, connection = generation)
                if (operationDepth == 0 && current()) settled()
                return true
            } finally { dispatch.endOperation() }
        }

        private inline fun change(method: ImeTrace.Method, vararg details: Pair<String, Int>, operation: () -> Boolean): Boolean {
            trace(method, *details)
            if (!current()) return false
            dispatch.beginOperation(editorEdit = operationDepth == 0)
            operationDepth++
            var result = false
            try {
                result = operation()
                return result
            } finally {
                operationDepth--
                if (operationDepth == 0 && current()) {
                    if (state.batchDepth == 0 && !caretAtEnd()) resetContext()
                    else {
                        state.changed(editable.toString(), connection = generation)
                        if (current()) settled()
                    }
                }
                trace(method, *details, "result" to if (result) 1 else 0)
                dispatch.endOperation()
            }
        }

        override fun setComposingText(text: CharSequence, newCursorPosition: Int): Boolean =
            change(ImeTrace.Method.COMPOSING, "textLength" to text.length, "cursor" to newCursorPosition) {
                super.setComposingText(text, newCursorPosition)
            }

        override fun commitText(text: CharSequence, newCursorPosition: Int): Boolean =
            change(ImeTrace.Method.COMMIT, "textLength" to text.length, "cursor" to newCursorPosition) {
                super.commitText(text, newCursorPosition)
            }

        @TargetApi(34)
        override fun replaceText(start: Int, end: Int, text: CharSequence, newCursorPosition: Int,
            textAttribute: TextAttribute?): Boolean =
            change(ImeTrace.Method.REPLACE, "start" to start, "end" to end, "textLength" to text.length,
                "cursor" to newCursorPosition) { super.replaceText(start, end, text, newCursorPosition, textAttribute) }

        override fun finishComposingText(): Boolean = change(ImeTrace.Method.FINISH) { super.finishComposingText() }

        override fun setComposingRegion(start: Int, end: Int): Boolean =
            change(ImeTrace.Method.COMPOSING_REGION, "start" to start, "end" to end) { super.setComposingRegion(start, end) }

        override fun setSelection(start: Int, end: Int): Boolean {
            if (!current()) return false
            if (start < 0 || end < 0 || start > editable.length || end > editable.length) return false
            return change(ImeTrace.Method.SELECTION, "start" to start, "end" to end) { super.setSelection(start, end) }
        }

        override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean =
            deleteAroundCaret(beforeLength, afterLength, false)

        override fun deleteSurroundingTextInCodePoints(beforeLength: Int, afterLength: Int): Boolean =
            deleteAroundCaret(beforeLength, afterLength, true)

        private fun deleteAroundCaret(before: Int, after: Int, codePoints: Boolean): Boolean {
            trace(if (codePoints) ImeTrace.Method.DELETE_CODE_POINTS else ImeTrace.Method.DELETE_UNITS,
                "before" to before, "after" to after)
            if (!current() || before < 0 || after < 0) return false
            dispatch.beginOperation(editorEdit = true)
            try {
                // IME caret movement starts a fresh segment, never a mid-text host replacement.
                if (!caretAtEnd()) { resetContext(); return true }
                val text = editable.toString()
                val start = ImeReconciler.beforeStart(text, text.length, before, codePoints) ?: return false
                val available = if (codePoints) text.codePointCount(0, text.length) else text.length
                editable.delete(start, editable.length)
                state.changed(editable.toString(), (before - available).coerceAtLeast(0), after, connection = generation)
                if (current()) settled()
                return true
            } finally { dispatch.endOperation() }
        }

        @Suppress("DEPRECATION") // Some IMEs still use ACTION_MULTIPLE for delete repeats.
        override fun sendKeyEvent(event: KeyEvent): Boolean {
            trace(ImeTrace.Method.KEY, "action" to event.action, "keyCode" to event.keyCode,
                "repeatCount" to event.repeatCount, "metaState" to event.metaState)
            dispatch.beginOperation()
            val accepted = try { handleImeKey(event) } finally { dispatch.endOperation() }
            trace(ImeTrace.Method.KEY, "action" to event.action, "keyCode" to event.keyCode,
                "repeatCount" to event.repeatCount, "result" to if (accepted) 1 else 0)
            return accepted
        }

        private fun handleImeKey(event: KeyEvent): Boolean {
            val kind = when (event.keyCode) {
                KeyEvent.KEYCODE_DEL -> ImeDeleteKeys.Kind.BACKWARD
                KeyEvent.KEYCODE_FORWARD_DEL -> ImeDeleteKeys.Kind.FORWARD
                else -> null
            }
            val action = when (event.action) {
                KeyEvent.ACTION_DOWN -> ImeDeleteKeys.Action.DOWN
                KeyEvent.ACTION_UP -> ImeDeleteKeys.Action.UP
                KeyEvent.ACTION_MULTIPLE -> ImeDeleteKeys.Action.MULTIPLE
                else -> ImeDeleteKeys.Action.OTHER
            }
            when (val route = deleteKeys.route(kind, action, event.repeatCount, current(), caretAtEnd(), editable.toString())) {
                ImeDeleteKeys.Route.Swallow -> return true
                is ImeDeleteKeys.Route.Delete -> {
                    dispatch.editorEdited()
                    state.beforeDeleteKey(generation)
                    // Gated input can still edit the local word. A partial-send reset,
                    // however, invalidates this connection and must prevent stale mutation.
                    if (!current() || dispatch.resetRequested) return false
                    trace(ImeTrace.Method.KEY_DELETE, "removedUnits" to (editable.length - route.start),
                        "outsideBackspaces" to route.outsideBackspaces, "outsideForward" to route.outsideForwardDeletes)
                    editable.delete(route.start, editable.length)
                    val accepted = state.deletedByKey(editable.toString(), route.outsideBackspaces,
                        route.outsideForwardDeletes, generation)
                    if (current()) {
                        settled()
                        if (state.batchDepth > 0) updateSelection()
                    }
                    // The editor handled this key even if remote admission is gated.
                    // Report an invalid connection only if a failure reset its lease.
                    val handled = current()
                    trace(ImeTrace.Method.KEY_DELETE, "accepted" to if (accepted) 1 else 0,
                        "result" to if (handled) 1 else 0)
                    return handled
                }
                ImeDeleteKeys.Route.Pass -> Unit
            }
            val code = KeyMap.ANDROID_TO_CODE[event.keyCode]
            return dispatch.dispatch(generation, event.keyCode, when (event.action) {
                KeyEvent.ACTION_DOWN -> ImeDispatchCoordinator.Action.DOWN
                KeyEvent.ACTION_UP -> ImeDispatchCoordinator.Action.UP
                KeyEvent.ACTION_MULTIPLE -> ImeDispatchCoordinator.Action.MULTIPLE
                else -> ImeDispatchCoordinator.Action.OTHER
            }, resetAfter = code !in KeyMap.MODIFIERS, restartAfter = ImeDispatchCoordinator.needsRestart(code),
                deduplicateDown = event.keyCode == KeyEvent.KEYCODE_ENTER || event.keyCode == KeyEvent.KEYCODE_NUMPAD_ENTER) {
                sink.onImeKey(event)
            }
        }

        override fun performEditorAction(actionCode: Int): Boolean {
            trace(ImeTrace.Method.EDITOR_ACTION, "actionCode" to actionCode)
            val time = SystemClock.uptimeMillis()
            val down = KeyEvent(time, time, KeyEvent.ACTION_DOWN, KeyEvent.KEYCODE_ENTER, 0, 0,
                KeyCharacterMap.VIRTUAL_KEYBOARD, 0, KeyEvent.FLAG_SOFT_KEYBOARD or KeyEvent.FLAG_KEEP_TOUCH_MODE)
            val result = dispatch.editorAction(generation, KeyEvent.KEYCODE_ENTER,
                { sink.onImeKey(down) }, { sink.onImeKey(KeyEvent.changeAction(down, KeyEvent.ACTION_UP)) })
            trace(ImeTrace.Method.EDITOR_ACTION, "actionCode" to actionCode, "result" to if (result) 1 else 0)
            return result
        }

        override fun closeConnection() {
            trace(ImeTrace.Method.CLOSE)
            if (!current()) return
            dispatch.beginOperation()
            try {
                super.closeConnection()
                if (!current()) return
                // Finish the last owned batch once, then retire this connection's lease.
                state.abortBatches(generation)
                if (caretAtEnd()) {
                    state.changed(editable.toString(), connection = generation)
                    if (current()) settled()
                } else resetContext()
                state.closeConnection(generation)
            } finally { dispatch.endOperation() }
        }

        override fun getTextBeforeCursor(length: Int, flags: Int): CharSequence? {
            trace(ImeTrace.Method.BEFORE_CURSOR, "requestedLength" to length, "flags" to flags)
            return super.getTextBeforeCursor(length, flags)
        }

        override fun getTextAfterCursor(length: Int, flags: Int): CharSequence? {
            trace(ImeTrace.Method.AFTER_CURSOR, "requestedLength" to length, "flags" to flags)
            return super.getTextAfterCursor(length, flags)
        }

        override fun getSelectedText(flags: Int): CharSequence? {
            trace(ImeTrace.Method.SELECTED_TEXT, "flags" to flags)
            return super.getSelectedText(flags)
        }
    }
}
