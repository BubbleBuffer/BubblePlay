package de.bubblebuffer.bubbleplay.ui

import android.view.PointerIcon
import de.bubblebuffer.bubbleplay.protocol.CursorKind

/** Android system pointers for a mouse hovering over the stream (API 24+, app minimum 29). */
object CursorAppearance {
    fun systemType(kind: CursorKind): Int = when (kind) {
        CursorKind.Arrow, CursorKind.UpArrow -> PointerIcon.TYPE_ARROW
        CursorKind.Text -> PointerIcon.TYPE_TEXT
        CursorKind.Pointer -> PointerIcon.TYPE_HAND
        CursorKind.Crosshair -> PointerIcon.TYPE_CROSSHAIR
        CursorKind.Move -> PointerIcon.TYPE_ALL_SCROLL
        CursorKind.ResizeNS -> PointerIcon.TYPE_VERTICAL_DOUBLE_ARROW
        CursorKind.ResizeEW -> PointerIcon.TYPE_HORIZONTAL_DOUBLE_ARROW
        CursorKind.ResizeNESW -> PointerIcon.TYPE_TOP_RIGHT_DIAGONAL_DOUBLE_ARROW
        CursorKind.ResizeNWSE -> PointerIcon.TYPE_TOP_LEFT_DIAGONAL_DOUBLE_ARROW
        // Android does not offer separate progress or upward-arrow system icons.
        CursorKind.Wait, CursorKind.Progress -> PointerIcon.TYPE_WAIT
        CursorKind.NotAllowed -> PointerIcon.TYPE_NO_DROP
        CursorKind.Help -> PointerIcon.TYPE_HELP
        CursorKind.Hidden -> PointerIcon.TYPE_NULL
    }
}
