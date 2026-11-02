package de.bubblebuffer.bubbleplay.flutter

/**
 * USB HID keyboard usages (usage page 0x07) to the DOM `KeyboardEvent.code` names that
 * `KeyMap` takes. Flutter's `PhysicalKeyboardKey.usbHidUsage` carries the page in the high
 * bits (0x70004 is A), so only the low 16 bits count.
 */
internal object HidKeys {
    private val table: Map<Int, String> = buildMap {
        for (i in 0 until 26) put(0x04 + i, "Key" + ('A' + i))
        for (i in 1..9) put(0x1D + i, "Digit$i")
        put(0x27, "Digit0")
        putAll(
            mapOf(
                0x28 to "Enter", 0x29 to "Escape", 0x2A to "Backspace", 0x2B to "Tab", 0x2C to "Space",
                0x2D to "Minus", 0x2E to "Equal", 0x2F to "BracketLeft", 0x30 to "BracketRight",
                0x31 to "Backslash", 0x33 to "Semicolon", 0x34 to "Quote", 0x35 to "Backquote",
                0x36 to "Comma", 0x37 to "Period", 0x38 to "Slash", 0x39 to "CapsLock",
                0x46 to "PrintScreen", 0x47 to "ScrollLock", 0x48 to "Pause", 0x49 to "Insert",
                0x4A to "Home", 0x4B to "PageUp", 0x4C to "Delete", 0x4D to "End", 0x4E to "PageDown",
                0x4F to "ArrowRight", 0x50 to "ArrowLeft", 0x51 to "ArrowDown", 0x52 to "ArrowUp",
                0x53 to "NumLock", 0x54 to "NumpadDivide", 0x55 to "NumpadMultiply",
                0x56 to "NumpadSubtract", 0x57 to "NumpadAdd", 0x58 to "NumpadEnter",
                0x62 to "Numpad0", 0x63 to "NumpadDecimal", 0x64 to "IntlBackslash", 0x65 to "ContextMenu",
                0x67 to "NumpadEqual", 0x85 to "NumpadComma", 0x87 to "IntlRo", 0x88 to "KanaMode",
                0x89 to "IntlYen", 0x8A to "Convert", 0x8B to "NonConvert",
                0xE0 to "ControlLeft", 0xE1 to "ShiftLeft", 0xE2 to "AltLeft", 0xE3 to "MetaLeft",
                0xE4 to "ControlRight", 0xE5 to "ShiftRight", 0xE6 to "AltRight", 0xE7 to "MetaRight",
            ),
        )
        for (i in 1..12) put(0x39 + i, "F$i")
        for (i in 1..9) put(0x58 + i, "Numpad$i")
        for (i in 13..24) put(0x68 + (i - 13), "F$i")
    }

    /** The DOM code of a HID usage, or null for a key the host path has no name for. */
    fun code(usage: Int): String? = table[usage and 0xFFFF]
}
