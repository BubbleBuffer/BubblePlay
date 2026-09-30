package de.bubblebuffer.bubbleplay.ui

import android.content.Context
import android.text.InputType
import android.view.KeyEvent
import android.view.View
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection

/**
 * Invisible IME target. Key events from the soft keyboard go to [Sink.onImeKey];
 * text the IME composes or commits without key events (swipe typing,
 * suggestions, voice) is mirrored to the host as an end-of-text edit:
 * backspaces for the replaced tail, then the new tail as Unicode text.
 */
class KeyboardInputView(context: Context, private val sink: Sink) : View(context) {
    interface Sink {
        fun onImeKey(event: KeyEvent): Boolean
        /** Delete [deleteCount] characters before the host caret, then type [insert]. */
        fun onImeEdit(deleteCount: Int, insert: String)
        fun onImeDeleteForward(count: Int)
    }

    init {
        isFocusable = true
        isFocusableInTouchMode = true
    }

    override fun onCheckIsTextEditor() = true

    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection {
        outAttrs.inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_AUTO_CORRECT
        outAttrs.imeOptions = EditorInfo.IME_FLAG_NO_EXTRACT_UI or EditorInfo.IME_FLAG_NO_FULLSCREEN or
            EditorInfo.IME_ACTION_NONE or EditorInfo.IME_FLAG_NO_PERSONALIZED_LEARNING
        outAttrs.initialSelStart = 0
        outAttrs.initialSelEnd = 0
        return Connection()
    }

    private inner class Connection : BaseInputConnection(this, false) {
        private var composing = ""

        override fun setComposingText(text: CharSequence, newCursorPosition: Int): Boolean {
            replaceComposing(text.toString())
            composing = text.toString()
            return true
        }

        override fun commitText(text: CharSequence, newCursorPosition: Int): Boolean {
            replaceComposing(text.toString())
            composing = ""
            return true
        }

        override fun finishComposingText(): Boolean {
            composing = ""
            return true
        }

        override fun setComposingRegion(start: Int, end: Int): Boolean = true

        override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
            if (beforeLength > 0) sink.onImeEdit(beforeLength, "")
            if (afterLength > 0) sink.onImeDeleteForward(afterLength)
            return true
        }

        override fun deleteSurroundingTextInCodePoints(beforeLength: Int, afterLength: Int) =
            deleteSurroundingText(beforeLength, afterLength)

        override fun sendKeyEvent(event: KeyEvent): Boolean {
            composing = ""
            return sink.onImeKey(event)
        }

        override fun getTextBeforeCursor(n: Int, flags: Int): CharSequence = composing.takeLast(n)
        override fun getTextAfterCursor(n: Int, flags: Int): CharSequence = ""

        private fun replaceComposing(next: String) {
            val edit = TextEdit.reconcile(composing, next)
            if (edit.deleteCount > 0 || edit.insert.isNotEmpty()) sink.onImeEdit(edit.deleteCount, edit.insert)
        }
    }
}

/** input.js reconcileTextEdit: keep the common prefix, delete the old tail, insert the new tail (by code point). */
object TextEdit {
    data class Edit(val deleteCount: Int, val insert: String)

    fun reconcile(previous: String, next: String): Edit {
        val before = previous.codePoints().toArray()
        val after = next.codePoints().toArray()
        var prefix = 0
        while (prefix < before.size && prefix < after.size && before[prefix] == after[prefix]) prefix++
        val insert = StringBuilder()
        for (i in prefix until after.size) insert.appendCodePoint(after[i])
        return Edit(before.size - prefix, insert.toString())
    }
}
