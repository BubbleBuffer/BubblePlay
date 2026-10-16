package de.bubblebuffer.bubbleplay.keys

import org.junit.Assert.assertEquals
import org.junit.Test

class KeyJsonTest {
    @Test
    fun canonicalSortsKeysByUtf16CodeUnits() {
        // Code-unit order: B, a, é, then the emoji's high surrogate (D83D), then U+FF5E.
        val map = linkedMapOf<String, Any?>("\uFF5E" to 1L, "\uD83D\uDE00" to 2L, "é" to 3L, "a" to 4L, "B" to 5L)
        assertEquals("{\"B\":5,\"a\":4,\"é\":3,\"\uD83D\uDE00\":2,\"\uFF5E\":1}", KeyJson.canonical(map))
    }

    @Test
    fun canonicalEscapesOnlyWhatRfc8785Requires() {
        val text = "\"\\\b\u000C\n\r\t\u0001\u001F/é\u2028"
        val expected = "\"" + "\\\"" + "\\\\" + "\\b" + "\\f" + "\\n" + "\\r" + "\\t" +
            "\\u0001" + "\\u001f" + "/é\u2028" + "\""
        assertEquals(expected, KeyJson.canonical(text))
    }

    @Test(expected = IllegalArgumentException::class)
    fun canonicalRefusesLoneSurrogates() {
        KeyJson.canonical("\uD800")
    }

    @Test
    fun parserDecodesSurrogatePairsAndRefusesLoneOnes() {
        assertEquals("\uD83D\uDE00", (KeyJson.parseObject("{\"a\":\"\\ud83d\\ude00\"}")["a"]))
        refusedWith(DeviceKeyException.Reason.MALFORMED) { KeyJson.parseObject("{\"a\":\"\\ud83d\"}") }
        refusedWith(DeviceKeyException.Reason.MALFORMED) { KeyJson.parseObject("{\"a\":\"\\ude00\"}") }
        refusedWith(DeviceKeyException.Reason.MALFORMED) { KeyJson.parseObject("{\"a\":\"\uD83D\"}") }
    }

    @Test
    fun parserRefusesControlCharactersDuplicatesLeadingZerosAndDepth() {
        refusedWith(DeviceKeyException.Reason.MALFORMED) { KeyJson.parseObject("{\"a\":\"\u0001\"}") }
        refusedWith(DeviceKeyException.Reason.MALFORMED) { KeyJson.parseObject("{\"a\":1,\"a\":2}") }
        refusedWith(DeviceKeyException.Reason.MALFORMED) { KeyJson.parseObject("{\"a\":01}") }
        refusedWith(DeviceKeyException.Reason.MALFORMED) { KeyJson.parseObject("{\"a\":1.5}") }
        refusedWith(DeviceKeyException.Reason.MALFORMED) { KeyJson.parseObject("{\"a\":true}") }
        refusedWith(DeviceKeyException.Reason.MALFORMED) { KeyJson.parseObject("{\"a\":{\"b\":{\"c\":{\"d\":{\"e\":null}}}}}") }
        KeyJson.parseObject("{\"a\":{\"b\":{\"c\":{\"d\":null}}}}")
    }

    @Test
    fun parserRefusesTrailingDataAndByteOrderMark() {
        refusedWith(DeviceKeyException.Reason.MALFORMED) { KeyJson.parseObject("{} x") }
        refusedWith(DeviceKeyException.Reason.MALFORMED) { KeyJson.parseObject("\uFEFF{}") }
        refusedWith(DeviceKeyException.Reason.MALFORMED) { KeyJson.parseObject("[]") }
    }
}
