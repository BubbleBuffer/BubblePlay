package de.bubblebuffer.bubbleplay.ui

import de.bubblebuffer.bubbleplay.protocol.KeyMap

/** Converts a reconciled IME tail into ordered Unicode text and complete key taps. */
internal class ImeTextSender(
    private val text: (String) -> Boolean,
    private val rawTap: (String) -> Boolean,
    private val keyTap: (String) -> Boolean,
    private val hasLatch: () -> Boolean,
    private val requestReset: () -> Unit,
) {
    fun send(deleteCount: Int, insert: String): Boolean {
        fun failed(): Boolean {
            // A prefix may already be queued; its unknown tail must not be replayed.
            requestReset()
            return false
        }
        repeat(deleteCount) { if (!rawTap("Backspace")) return failed() }
        if (insert.isEmpty()) return true
        if (hasLatch() && insert.length == 1) {
            val code = KeyMap.codeForShortcutChar(insert[0])
            if (code != null) {
                val ok = keyTap(code)
                requestReset()
                return ok
            }
        }
        val pending = StringBuilder()
        fun flush(): Boolean {
            if (pending.isEmpty()) return true
            val ok = text(pending.toString())
            pending.setLength(0)
            return ok
        }
        var navigation = false
        for (ch in insert) when (ch) {
            '\n', '\t' -> {
                if (!flush() || !keyTap(if (ch == '\n') "Enter" else "Tab")) return failed()
                navigation = true
            }
            else -> pending.append(ch)
        }
        if (!flush()) return failed()
        if (navigation) requestReset()
        return true
    }
}
