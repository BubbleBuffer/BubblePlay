package de.bubblebuffer.bubbleplay.protocol

import android.view.KeyEvent

/**
 * Physical keys by DOM KeyboardEvent.code -> Windows set-1 scan code, the same
 * table as worker/public/input.js SCAN_TABLE (checked against
 * fixtures/input-contract.json by the unit tests). The host injects scan codes
 * so its own keyboard layout produces the character (AltGr, AZERTY, ...).
 */
object KeyMap {
    data class Key(val scan: Int, val extended: Boolean, val vk: Int, val legacyVk: Int?)

    val SCAN_CODES: Map<String, Pair<Int, Boolean>> = buildMap {
        fun s(code: String, scan: Int, extended: Boolean = false) = put(code, scan to extended)
        s("Escape", 0x01); s("Digit1", 0x02); s("Digit2", 0x03); s("Digit3", 0x04); s("Digit4", 0x05); s("Digit5", 0x06)
        s("Digit6", 0x07); s("Digit7", 0x08); s("Digit8", 0x09); s("Digit9", 0x0a); s("Digit0", 0x0b); s("Minus", 0x0c)
        s("Equal", 0x0d); s("Backspace", 0x0e); s("Tab", 0x0f); s("KeyQ", 0x10); s("KeyW", 0x11); s("KeyE", 0x12)
        s("KeyR", 0x13); s("KeyT", 0x14); s("KeyY", 0x15); s("KeyU", 0x16); s("KeyI", 0x17); s("KeyO", 0x18); s("KeyP", 0x19)
        s("BracketLeft", 0x1a); s("BracketRight", 0x1b); s("Enter", 0x1c); s("ControlLeft", 0x1d); s("KeyA", 0x1e)
        s("KeyS", 0x1f); s("KeyD", 0x20); s("KeyF", 0x21); s("KeyG", 0x22); s("KeyH", 0x23); s("KeyJ", 0x24); s("KeyK", 0x25)
        s("KeyL", 0x26); s("Semicolon", 0x27); s("Quote", 0x28); s("Backquote", 0x29); s("ShiftLeft", 0x2a)
        s("Backslash", 0x2b); s("KeyZ", 0x2c); s("KeyX", 0x2d); s("KeyC", 0x2e); s("KeyV", 0x2f); s("KeyB", 0x30)
        s("KeyN", 0x31); s("KeyM", 0x32); s("Comma", 0x33); s("Period", 0x34); s("Slash", 0x35); s("ShiftRight", 0x36)
        s("NumpadMultiply", 0x37); s("AltLeft", 0x38); s("Space", 0x39); s("CapsLock", 0x3a); s("F1", 0x3b); s("F2", 0x3c)
        s("F3", 0x3d); s("F4", 0x3e); s("F5", 0x3f); s("F6", 0x40); s("F7", 0x41); s("F8", 0x42); s("F9", 0x43); s("F10", 0x44)
        s("ScrollLock", 0x46); s("Numpad7", 0x47); s("Numpad8", 0x48); s("Numpad9", 0x49); s("NumpadSubtract", 0x4a)
        s("Numpad4", 0x4b); s("Numpad5", 0x4c); s("Numpad6", 0x4d); s("NumpadAdd", 0x4e); s("Numpad1", 0x4f)
        s("Numpad2", 0x50); s("Numpad3", 0x51); s("Numpad0", 0x52); s("NumpadDecimal", 0x53); s("IntlBackslash", 0x56)
        s("F11", 0x57); s("F12", 0x58); s("NumpadEqual", 0x59); s("F13", 0x64); s("F14", 0x65); s("F15", 0x66); s("F16", 0x67)
        s("F17", 0x68); s("F18", 0x69); s("F19", 0x6a); s("F20", 0x6b); s("F21", 0x6c); s("F22", 0x6d); s("F23", 0x6e)
        s("KanaMode", 0x70); s("Lang2", 0x71); s("Lang1", 0x72); s("IntlRo", 0x73); s("F24", 0x76); s("Convert", 0x79)
        s("NonConvert", 0x7b); s("IntlYen", 0x7d); s("NumpadComma", 0x7e)
        // E0-prefixed (KEYEVENTF_EXTENDEDKEY) keys.
        s("MediaTrackPrevious", 0x10, true); s("MediaTrackNext", 0x19, true); s("NumpadEnter", 0x1c, true)
        s("ControlRight", 0x1d, true); s("AudioVolumeMute", 0x20, true); s("LaunchApp2", 0x21, true)
        s("MediaPlayPause", 0x22, true); s("MediaStop", 0x24, true); s("AudioVolumeDown", 0x2e, true)
        s("AudioVolumeUp", 0x30, true); s("BrowserHome", 0x32, true); s("NumpadDivide", 0x35, true)
        s("PrintScreen", 0x37, true); s("AltRight", 0x38, true); s("Home", 0x47, true); s("ArrowUp", 0x48, true)
        s("PageUp", 0x49, true); s("ArrowLeft", 0x4b, true); s("ArrowRight", 0x4d, true); s("End", 0x4f, true)
        s("ArrowDown", 0x50, true); s("PageDown", 0x51, true); s("Insert", 0x52, true); s("Delete", 0x53, true)
        s("MetaLeft", 0x5b, true); s("MetaRight", 0x5c, true); s("ContextMenu", 0x5d, true)
        s("BrowserSearch", 0x65, true); s("BrowserFavorites", 0x66, true); s("BrowserRefresh", 0x67, true)
        s("BrowserStop", 0x68, true); s("BrowserForward", 0x69, true); s("BrowserBack", 0x6a, true)
        s("LaunchApp1", 0x6b, true); s("LaunchMail", 0x6c, true); s("MediaSelect", 0x6d, true)
    }

    /** Windows VK per physical key (legacy hosts, and keys without a scan code). */
    val VK_CODES: Map<String, Int> = buildMap {
        putAll(
            mapOf(
                "Backspace" to 0x08, "Tab" to 0x09, "Enter" to 0x0D, "NumpadEnter" to 0x0D, "ShiftLeft" to 0xA0, "ShiftRight" to 0xA1,
                "ControlLeft" to 0xA2, "ControlRight" to 0xA3, "AltLeft" to 0xA4, "AltRight" to 0xA5,
                "CapsLock" to 0x14, "Escape" to 0x1B, "Space" to 0x20, "PageUp" to 0x21, "PageDown" to 0x22,
                "End" to 0x23, "Home" to 0x24, "ArrowLeft" to 0x25, "ArrowUp" to 0x26, "ArrowRight" to 0x27,
                "ArrowDown" to 0x28, "PrintScreen" to 0x2C, "Insert" to 0x2D, "Delete" to 0x2E,
                "MetaLeft" to 0x5B, "MetaRight" to 0x5C, "ContextMenu" to 0x5D,
                "NumpadMultiply" to 0x6A, "NumpadAdd" to 0x6B, "NumpadSubtract" to 0x6D, "NumpadDecimal" to 0x6E, "NumpadDivide" to 0x6F,
                "Semicolon" to 0xBA, "Equal" to 0xBB, "Comma" to 0xBC, "Minus" to 0xBD, "Period" to 0xBE,
                "Slash" to 0xBF, "Backquote" to 0xC0, "BracketLeft" to 0xDB, "Backslash" to 0xDC,
                "BracketRight" to 0xDD, "Quote" to 0xDE, "IntlBackslash" to 0xE2,
                "NumLock" to 0x90, "ScrollLock" to 0x91, "Pause" to 0x13,
            ),
        )
        for (i in 0 until 26) put("Key" + ('A' + i), 0x41 + i)
        for (i in 0 until 10) { put("Digit$i", 0x30 + i); put("Numpad$i", 0x60 + i) }
        for (i in 1..24) put("F$i", 0x70 + (i - 1))
    }

    private val LEGACY_VK = mapOf(
        "ShiftLeft" to 0x10, "ShiftRight" to 0x10, "ControlLeft" to 0x11, "ControlRight" to 0x11, "AltLeft" to 0x12, "AltRight" to 0x12,
    )

    /** input.js keyDescriptor: { scan, extended, vk, legacyVk } for a code, or null. */
    fun descriptor(code: String): Key? {
        val scan = SCAN_CODES[code]
        val vk = VK_CODES[code]
        if (scan == null && vk == null) return null
        return Key(scan?.first ?: 0, scan?.second ?: false, vk ?: 0, LEGACY_VK[code] ?: vk)
    }

    /**
     * Packet for a physical key transition (InputController.physicalKey): a
     * scan-code packet on scan-code hosts, a legacy VK packet otherwise.
     */
    fun packet(code: String, up: Boolean, scancodeHost: Boolean): ByteArray? {
        val key = descriptor(code) ?: return null
        if (!scancodeHost) return key.legacyVk?.let { InputPackets.key(it, up) }
        if (key.scan == 0 && key.vk == 0) return null
        return InputPackets.scanKey(key.scan, key.extended, key.vk, up)
    }

    /** Android KeyEvent keycode -> DOM KeyboardEvent.code (physical position). */
    val ANDROID_TO_CODE: Map<Int, String> = buildMap {
        for (i in 0 until 26) put(KeyEvent.KEYCODE_A + i, "Key" + ('A' + i))
        for (i in 0 until 10) {
            put(KeyEvent.KEYCODE_0 + i, "Digit$i")
            put(KeyEvent.KEYCODE_NUMPAD_0 + i, "Numpad$i")
        }
        for (i in 0 until 12) put(KeyEvent.KEYCODE_F1 + i, "F${i + 1}")
        putAll(
            mapOf(
                KeyEvent.KEYCODE_ESCAPE to "Escape", KeyEvent.KEYCODE_TAB to "Tab", KeyEvent.KEYCODE_ENTER to "Enter",
                KeyEvent.KEYCODE_NUMPAD_ENTER to "NumpadEnter", KeyEvent.KEYCODE_DEL to "Backspace",
                KeyEvent.KEYCODE_FORWARD_DEL to "Delete", KeyEvent.KEYCODE_SPACE to "Space",
                KeyEvent.KEYCODE_MINUS to "Minus", KeyEvent.KEYCODE_EQUALS to "Equal",
                KeyEvent.KEYCODE_LEFT_BRACKET to "BracketLeft", KeyEvent.KEYCODE_RIGHT_BRACKET to "BracketRight",
                KeyEvent.KEYCODE_BACKSLASH to "Backslash", KeyEvent.KEYCODE_SEMICOLON to "Semicolon",
                KeyEvent.KEYCODE_APOSTROPHE to "Quote", KeyEvent.KEYCODE_GRAVE to "Backquote",
                KeyEvent.KEYCODE_COMMA to "Comma", KeyEvent.KEYCODE_PERIOD to "Period", KeyEvent.KEYCODE_SLASH to "Slash",
                KeyEvent.KEYCODE_SHIFT_LEFT to "ShiftLeft", KeyEvent.KEYCODE_SHIFT_RIGHT to "ShiftRight",
                KeyEvent.KEYCODE_CTRL_LEFT to "ControlLeft", KeyEvent.KEYCODE_CTRL_RIGHT to "ControlRight",
                KeyEvent.KEYCODE_ALT_LEFT to "AltLeft", KeyEvent.KEYCODE_ALT_RIGHT to "AltRight",
                KeyEvent.KEYCODE_META_LEFT to "MetaLeft", KeyEvent.KEYCODE_META_RIGHT to "MetaRight",
                KeyEvent.KEYCODE_CAPS_LOCK to "CapsLock", KeyEvent.KEYCODE_SCROLL_LOCK to "ScrollLock",
                KeyEvent.KEYCODE_NUM_LOCK to "NumLock", KeyEvent.KEYCODE_BREAK to "Pause",
                KeyEvent.KEYCODE_SYSRQ to "PrintScreen", KeyEvent.KEYCODE_INSERT to "Insert",
                KeyEvent.KEYCODE_MOVE_HOME to "Home", KeyEvent.KEYCODE_MOVE_END to "End",
                KeyEvent.KEYCODE_PAGE_UP to "PageUp", KeyEvent.KEYCODE_PAGE_DOWN to "PageDown",
                KeyEvent.KEYCODE_DPAD_UP to "ArrowUp", KeyEvent.KEYCODE_DPAD_DOWN to "ArrowDown",
                KeyEvent.KEYCODE_DPAD_LEFT to "ArrowLeft", KeyEvent.KEYCODE_DPAD_RIGHT to "ArrowRight",
                KeyEvent.KEYCODE_NUMPAD_DIVIDE to "NumpadDivide", KeyEvent.KEYCODE_NUMPAD_MULTIPLY to "NumpadMultiply",
                KeyEvent.KEYCODE_NUMPAD_SUBTRACT to "NumpadSubtract", KeyEvent.KEYCODE_NUMPAD_ADD to "NumpadAdd",
                KeyEvent.KEYCODE_NUMPAD_DOT to "NumpadDecimal", KeyEvent.KEYCODE_NUMPAD_COMMA to "NumpadComma",
                KeyEvent.KEYCODE_NUMPAD_EQUALS to "NumpadEqual", KeyEvent.KEYCODE_MENU to "ContextMenu",
                KeyEvent.KEYCODE_YEN to "IntlYen", KeyEvent.KEYCODE_RO to "IntlRo", KeyEvent.KEYCODE_KANA to "KanaMode",
                KeyEvent.KEYCODE_HENKAN to "Convert", KeyEvent.KEYCODE_MUHENKAN to "NonConvert",
                KeyEvent.KEYCODE_MEDIA_PLAY_PAUSE to "MediaPlayPause", KeyEvent.KEYCODE_MEDIA_NEXT to "MediaTrackNext",
                KeyEvent.KEYCODE_MEDIA_PREVIOUS to "MediaTrackPrevious", KeyEvent.KEYCODE_MEDIA_STOP to "MediaStop",
            ),
        )
    }

    /** Modifier codes the overlay can latch. */
    val MODIFIERS = setOf("ShiftLeft", "ShiftRight", "ControlLeft", "ControlRight", "AltLeft", "AltRight", "MetaLeft", "MetaRight")

    /**
     * US-position code for an ASCII letter/digit typed by the IME while a
     * modifier is latched (Ctrl+C from the soft keyboard), else null.
     */
    fun codeForShortcutChar(ch: Char): String? = when (ch) {
        in 'a'..'z' -> "Key" + ch.uppercaseChar()
        in 'A'..'Z' -> "Key$ch"
        in '0'..'9' -> "Digit$ch"
        ' ' -> "Space"
        '\n' -> "Enter"
        '\t' -> "Tab"
        else -> null
    }
}
