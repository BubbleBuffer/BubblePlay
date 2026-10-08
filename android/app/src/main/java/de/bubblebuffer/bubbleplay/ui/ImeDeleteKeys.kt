package de.bubblebuffer.bubbleplay.ui

/** Platform-independent delete-key routing, with a platform grapheme boundary provider. */
class ImeDeleteKeys(private val precedingBoundary: (String, Int) -> Int) {
    enum class Kind { BACKWARD, FORWARD }
    enum class Action { DOWN, UP, MULTIPLE, OTHER }
    sealed interface Route {
        data object Pass : Route
        data object Swallow : Route
        data class Delete(val start: Int, val outsideBackspaces: Int, val outsideForwardDeletes: Int) : Route
    }

    fun route(kind: Kind?, action: Action, repeats: Int, current: Boolean, caretAtEnd: Boolean, text: String): Route {
        if (kind == null) return Route.Pass
        // Delete forwarding uses complete host taps. Even a replacement connection's UP
        // must be consumed, rather than enter the generic path and discard its current word.
        if (action == Action.UP) return Route.Swallow
        if (!current || !caretAtEnd || (action != Action.DOWN && action != Action.MULTIPLE)) return Route.Pass
        // Each DOWN is one deletion, including typematic downs. MULTIPLE carries its count.
        var remaining = if (action == Action.DOWN) 1 else repeats.coerceAtLeast(0)
        var start = text.length
        while (remaining > 0 && start > 0) {
            start = precedingBoundary(text, start).coerceIn(0, start - 1)
            remaining--
        }
        // In the IME's known end-caret segment both delete keys remove the preceding cluster.
        // Beyond that segment, their actual host direction is preserved. The reconciler sends
        // one Backspace per removed code point, rather than one per local grapheme or UTF-16 unit.
        return Route.Delete(start, if (kind == Kind.BACKWARD) remaining else 0,
            if (kind == Kind.FORWARD) remaining else 0)
    }
}

/** Log gate and formatting shared by production and JVM tests; values can only be counts. */
class ImeTrace(private val enabled: () -> Boolean, private val emit: (String) -> Unit) {
    enum class Method(val apiName: String) {
        CREATE("onCreateInputConnection"), RESET("resetContext"), BEGIN_BATCH("beginBatchEdit"),
        END_BATCH("endBatchEdit"), COMPOSING("setComposingText"), COMMIT("commitText"),
        REPLACE("replaceText"), FINISH("finishComposingText"), COMPOSING_REGION("setComposingRegion"),
        SELECTION("setSelection"), DELETE_UNITS("deleteSurroundingText"),
        DELETE_CODE_POINTS("deleteSurroundingTextInCodePoints"), KEY("sendKeyEvent"), KEY_DELETE("deleteKey"),
        BEFORE_CURSOR("getTextBeforeCursor"), AFTER_CURSOR("getTextAfterCursor"), SELECTED_TEXT("getSelectedText"),
        EDIT("emitEdit"), EDIT_RESULT("emitEditResult"), CLOSE("closeConnection"),
        EDITOR_ACTION("performEditorAction"), DEFERRED_RESET("requestDeferredResetContext"), RESTART("restartInput")
    }

    fun record(method: Method, metadata: () -> Map<String, Int>) {
        if (!enabled()) return
        emit(method.apiName + metadata().entries.joinToString(prefix = " ", separator = " ") { "${it.key}=${it.value}" })
    }
}
