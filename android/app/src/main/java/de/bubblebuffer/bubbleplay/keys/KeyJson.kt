package de.bubblebuffer.bubbleplay.keys

/**
 * The JSON subset key files use: objects, strings, non-negative integers and null.
 *
 * [parseObject] is strict: no duplicate keys, no trailing data, no BOM, no lone surrogates.
 * [canonical] is RFC 8785 for this subset only: keys sorted by UTF-16 code unit, minimal string
 * escaping, integers written in decimal. Other values are refused rather than canonicalised.
 */
internal object KeyJson {
    private const val MAX_DEPTH = 4
    private const val HEX = "0123456789abcdef"

    /** Parses a key-file object. Values are String, Long, null, or Map<String, Any?>. */
    @Suppress("UNCHECKED_CAST")
    fun parseObject(text: String): Map<String, Any?> {
        val parser = Parser(text)
        val root = parser.value(0)
        parser.end()
        return root as? Map<String, Any?> ?: malformed("key file is not a JSON object")
    }

    /** RFC 8785 text of [value]: sorted keys, no whitespace. */
    fun canonical(value: Any?): String {
        val out = StringBuilder()
        writeCanonical(out, value)
        return out.toString()
    }

    /** File text: fields in map order, two-space indent, final LF. */
    fun pretty(fields: Map<String, Any?>): String {
        val out = StringBuilder()
        writePretty(out, fields, 0)
        return out.append('\n').toString()
    }

    fun isWellFormed(text: CharSequence): Boolean {
        var i = 0
        while (i < text.length) {
            val c = text[i]
            when {
                Character.isHighSurrogate(c) -> {
                    if (i + 1 >= text.length || !Character.isLowSurrogate(text[i + 1])) return false
                    i += 2
                }
                Character.isLowSurrogate(c) -> return false
                else -> i++
            }
        }
        return true
    }

    private fun writeCanonical(out: StringBuilder, value: Any?) {
        if (value !is Map<*, *>) {
            writeScalar(out, value)
            return
        }
        out.append('{')
        value.entries.sortedBy { it.key as String }.forEachIndexed { index, entry ->
            if (index > 0) out.append(',')
            writeString(out, entry.key as String)
            out.append(':')
            writeCanonical(out, entry.value)
        }
        out.append('}')
    }

    private fun writePretty(out: StringBuilder, value: Any?, depth: Int) {
        if (value !is Map<*, *>) {
            writeScalar(out, value)
            return
        }
        out.append("{\n")
        value.entries.forEachIndexed { index, entry ->
            if (index > 0) out.append(",\n")
            repeat(depth + 1) { out.append("  ") }
            writeString(out, entry.key as String)
            out.append(": ")
            writePretty(out, entry.value, depth + 1)
        }
        out.append('\n')
        repeat(depth) { out.append("  ") }
        out.append('}')
    }

    private fun writeScalar(out: StringBuilder, value: Any?) {
        when (value) {
            null -> out.append("null")
            is String -> writeString(out, value)
            is Long -> out.append(value)
            is Int -> out.append(value)
            else -> throw IllegalArgumentException("unsupported key-file value")
        }
    }

    private fun writeString(out: StringBuilder, text: String) {
        require(isWellFormed(text)) { "lone surrogate in key-file string" }
        out.append('"')
        for (c in text) {
            when (c) {
                '"' -> out.append("\\\"")
                '\\' -> out.append("\\\\")
                '\b' -> out.append("\\b")
                '\u000C' -> out.append("\\f")
                '\n' -> out.append("\\n")
                '\r' -> out.append("\\r")
                '\t' -> out.append("\\t")
                else -> {
                    if (c < ' ') {
                        out.append("\\u00").append(HEX[c.code shr 4]).append(HEX[c.code and 0xF])
                    } else {
                        out.append(c)
                    }
                }
            }
        }
        out.append('"')
    }

    private class Parser(private val text: String) {
        private var pos = 0

        fun value(level: Int): Any? {
            skipSpace()
            return when (peek()) {
                '{' -> obj(level + 1)
                '"' -> string()
                'n' -> {
                    literal("null")
                    null
                }
                in '0'..'9' -> number()
                else -> malformed("unsupported JSON value")
            }
        }

        fun end() {
            skipSpace()
            if (pos != text.length) malformed("trailing data after the key file")
        }

        private fun obj(level: Int): Map<String, Any?> {
            if (level > MAX_DEPTH) malformed("key file nests too deeply")
            pos++
            val members = LinkedHashMap<String, Any?>()
            skipSpace()
            if (peek() == '}') {
                pos++
                return members
            }
            while (true) {
                skipSpace()
                if (peek() != '"') malformed("expected a key")
                val key = string()
                if (members.containsKey(key)) malformed("duplicate key '$key'")
                skipSpace()
                if (peek() != ':') malformed("expected ':'")
                pos++
                members[key] = value(level)
                skipSpace()
                when (peek()) {
                    ',' -> pos++
                    '}' -> {
                        pos++
                        return members
                    }
                    else -> malformed("expected ',' or '}'")
                }
            }
        }

        private fun string(): String {
            pos++
            val out = StringBuilder()
            loop@ while (true) {
                if (pos >= text.length) malformed("unterminated string")
                val c = text[pos++]
                when {
                    c == '"' -> break@loop
                    c == '\\' -> escape(out)
                    c < ' ' -> malformed("control character in string")
                    else -> out.append(c)
                }
            }
            if (!KeyJson.isWellFormed(out)) malformed("invalid UTF-16 in string")
            return out.toString()
        }

        private fun escape(out: StringBuilder) {
            if (pos >= text.length) malformed("unterminated escape")
            when (val c = text[pos++]) {
                '"', '\\', '/' -> out.append(c)
                'b' -> out.append('\b')
                'f' -> out.append('\u000C')
                'n' -> out.append('\n')
                'r' -> out.append('\r')
                't' -> out.append('\t')
                'u' -> {
                    if (pos + 4 > text.length) malformed("short unicode escape")
                    val hex = text.substring(pos, pos + 4)
                    if (!hex.all { it in '0'..'9' || it in 'a'..'f' || it in 'A'..'F' }) {
                        malformed("bad unicode escape")
                    }
                    out.append(hex.toInt(16).toChar())
                    pos += 4
                }
                else -> malformed("bad escape in string")
            }
        }

        private fun number(): Long {
            val start = pos
            if (peek() == '0') {
                pos++
            } else {
                while (peek() in '0'..'9') pos++
            }
            return text.substring(start, pos).toLongOrNull() ?: malformed("number is out of range")
        }

        private fun literal(word: String) {
            if (!text.startsWith(word, pos)) malformed("unsupported JSON value")
            pos += word.length
        }

        private fun skipSpace() {
            while (pos < text.length && text[pos] in " \t\n\r") pos++
        }

        private fun peek(): Char = if (pos < text.length) text[pos] else END

        private companion object {
            const val END = '\u0000'
        }
    }
}
