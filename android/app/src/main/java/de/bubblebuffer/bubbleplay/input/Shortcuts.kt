package de.bubblebuffer.bubbleplay.input

import de.bubblebuffer.bubbleplay.R

/** A full key chord sent as one action: modifiers down, key tap, modifiers up. */
data class Chord(val label: Int, val mods: List<String>, val key: String) {
    /** "Ctrl+Shift+T" for display (mono), independent of the host keyboard layout. */
    val keys: String get() = (mods.map(Shortcuts::keyName) + Shortcuts.keyName(key)).joinToString("+")
}

object Shortcuts {
    private const val CTRL = "ControlLeft"
    private const val ALT = "AltLeft"
    private const val SHIFT = "ShiftLeft"
    private const val WIN = "MetaLeft"

    /** The Shortcuts popover, top to bottom. Ctrl+Alt+Del is deliberately absent. */
    val ALL: List<Chord> = listOf(
        Chord(R.string.sc_copy, listOf(CTRL), "KeyC"),
        Chord(R.string.sc_paste, listOf(CTRL), "KeyV"),
        Chord(R.string.sc_cut, listOf(CTRL), "KeyX"),
        Chord(R.string.sc_undo, listOf(CTRL), "KeyZ"),
        Chord(R.string.sc_redo, listOf(CTRL), "KeyY"),
        Chord(R.string.sc_select_all, listOf(CTRL), "KeyA"),
        Chord(R.string.sc_save, listOf(CTRL), "KeyS"),
        Chord(R.string.sc_find, listOf(CTRL), "KeyF"),
        Chord(R.string.sc_close_tab, listOf(CTRL), "KeyW"),
        Chord(R.string.sc_new_tab, listOf(CTRL), "KeyT"),
        Chord(R.string.sc_reopen_tab, listOf(CTRL, SHIFT), "KeyT"),
        Chord(R.string.sc_switch_window, listOf(ALT), "Tab"),
        Chord(R.string.sc_close_window, listOf(ALT), "F4"),
        Chord(R.string.sc_start, emptyList(), WIN),
        Chord(R.string.sc_show_desktop, listOf(WIN), "KeyD"),
        Chord(R.string.sc_task_manager, listOf(CTRL, SHIFT), "Escape"),
        Chord(R.string.sc_escape, emptyList(), "Escape"),
        Chord(R.string.sc_tab, emptyList(), "Tab"),
        Chord(R.string.sc_enter, emptyList(), "Enter"),
    )

    /** Sticky modifiers in the Keys popover, left to right. */
    val MODIFIERS: List<String> = listOf(CTRL, ALT, SHIFT, WIN)

    fun keyName(code: String): String = when {
        code == CTRL -> "Ctrl"
        code == ALT -> "Alt"
        code == SHIFT -> "Shift"
        code == WIN -> "Win"
        code == "Escape" -> "Esc"
        code == "Delete" -> "Del"
        code == "PageUp" -> "PgUp"
        code == "PageDown" -> "PgDn"
        code == "ArrowLeft" -> "←"
        code == "ArrowUp" -> "↑"
        code == "ArrowRight" -> "→"
        code == "ArrowDown" -> "↓"
        code.startsWith("Key") && code.length == 4 -> code.substring(3)
        code.startsWith("Digit") && code.length == 6 -> code.substring(5)
        else -> code
    }
}
