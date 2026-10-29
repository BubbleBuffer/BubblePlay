package de.bubblebuffer.bubbleplay.auth

/**
 * The JSON the auth messages use, parsed strictly and written compactly in insertion order.
 *
 * Values: [Map] (String keys, insertion order), [List], [String], [Long] (integers only),
 * [JsonNumber] (any other number, kept as its text), [Boolean] and null. Parsing refuses
 * duplicate keys, trailing data, lone surrogates, leading zeros and nesting deeper than
 * [MAX_DEPTH], so Android's org.json and the JVM test library cannot disagree about a message.
 */
internal object AuthJson {
    private const val MAX_DEPTH = 16

    /** A non-integer number, kept verbatim (never used by the auth messages themselves). */
    data class JsonNumber(val text: String)

    class Malformed(message: String) : Exception(message)

    /** Parses [text] as one JSON object; throws [Malformed] otherwise. */
    @Suppress("UNCHECKED_CAST")
    fun parseObject(text: String): Map<String, Any?> {
        val parser = Parser(text)
        parser.skipSpace()
        val value = parser.value(0)
        parser.skipSpace()
        if (parser.pos != text.length) throw Malformed("trailing data")
        return value as? Map<String, Any?> ?: throw Malformed("not a JSON object")
    }

    /** Compact JSON text of [value]. */
    fun write(value: Any?): String = StringBuilder().also { write(it, value) }.toString()

    /** A JSON object with exactly the keys in [keys]: all present, nothing else. */
    fun requireExactKeys(m: Map<String, Any?>, vararg keys: String) {
        if (m.size != keys.size || keys.any { it !in m }) throw Malformed("unexpected keys ${m.keys}")
    }

    fun string(m: Map<String, Any?>, key: String): String =
        m[key] as? String ?: throw Malformed("$key is not a string")

    private fun write(out: StringBuilder, value: Any?) {
        when (value) {
            null -> out.append("null")
            is String -> writeString(out, value)
            is Boolean -> out.append(value)
            is Long -> out.append(value)
            is Int -> out.append(value)
            is JsonNumber -> out.append(value.text)
            is Map<*, *> -> {
                out.append('{')
                var first = true
                for ((k, v) in value) {
                    if (!first) out.append(',')
                    first = false
                    writeString(out, k as String)
                    out.append(':')
                    write(out, v)
                }
                out.append('}')
            }
            is List<*> -> {
                out.append('[')
                value.forEachIndexed { i, v ->
                    if (i > 0) out.append(',')
                    write(out, v)
                }
                out.append(']')
            }
            else -> throw IllegalArgumentException("not a JSON value: ${value::class.java.name}")
        }
    }

    private fun writeString(out: StringBuilder, s: String) {
        out.append('"')
        for (c in s) {
            when {
                c == '"' -> out.append("\\\"")
                c == '\\' -> out.append("\\\\")
                c == '\n' -> out.append("\\n")
                c == '\r' -> out.append("\\r")
                c == '\t' -> out.append("\\t")
                c == '\b' -> out.append("\\b")
                c == '\u000C' -> out.append("\\f")
                c < ' ' -> out.append("\\u").append(Integer.toHexString(c.code).padStart(4, '0'))
                else -> out.append(c)
            }
        }
        out.append('"')
    }

    private class Parser(val text: String) {
        var pos = 0

        fun skipSpace() {
            while (pos < text.length && text[pos] in " \t\n\r") pos++
        }

        fun value(depth: Int): Any? {
            if (depth > MAX_DEPTH) throw Malformed("nested too deeply")
            if (pos >= text.length) throw Malformed("unexpected end")
            return when (val c = text[pos]) {
                '{' -> obj(depth)
                '[' -> array(depth)
                '"' -> string()
                't' -> literal("true", true)
                'f' -> literal("false", false)
                'n' -> literal("null", null)
                else -> if (c == '-' || c in '0'..'9') number() else throw Malformed("unexpected '$c'")
            }
        }

        private fun literal(word: String, result: Any?): Any? {
            if (!text.startsWith(word, pos)) throw Malformed("bad literal")
            pos += word.length
            return result
        }

        private fun obj(depth: Int): Map<String, Any?> {
            pos++
            val out = LinkedHashMap<String, Any?>()
            skipSpace()
            if (peek() == '}') {
                pos++
                return out
            }
            while (true) {
                skipSpace()
                if (peek() != '"') throw Malformed("expected a key")
                val key = string()
                skipSpace()
                expect(':')
                skipSpace()
                if (key in out) throw Malformed("duplicate key $key")
                out[key] = value(depth + 1)
                skipSpace()
                when (next()) {
                    ',' -> continue
                    '}' -> return out
                    else -> throw Malformed("expected , or }")
                }
            }
        }

        private fun array(depth: Int): List<Any?> {
            pos++
            val out = ArrayList<Any?>()
            skipSpace()
            if (peek() == ']') {
                pos++
                return out
            }
            while (true) {
                skipSpace()
                out.add(value(depth + 1))
                skipSpace()
                when (next()) {
                    ',' -> continue
                    ']' -> return out
                    else -> throw Malformed("expected , or ]")
                }
            }
        }

        private fun string(): String {
            expect('"')
            val out = StringBuilder()
            while (true) {
                val c = next()
                when {
                    c == '"' -> break
                    c == '\\' -> when (val e = next()) {
                        '"' -> out.append('"')
                        '\\' -> out.append('\\')
                        '/' -> out.append('/')
                        'b' -> out.append('\b')
                        'f' -> out.append('\u000C')
                        'n' -> out.append('\n')
                        'r' -> out.append('\r')
                        't' -> out.append('\t')
                        'u' -> {
                            if (pos + 4 > text.length) throw Malformed("bad escape")
                            val hex = text.substring(pos, pos + 4)
                            if (!hex.all { it in '0'..'9' || it in 'a'..'f' || it in 'A'..'F' }) throw Malformed("bad escape")
                            out.append(hex.toInt(16).toChar())
                            pos += 4
                        }
                        else -> throw Malformed("bad escape \\$e")
                    }
                    c < ' ' -> throw Malformed("control character in string")
                    else -> out.append(c)
                }
            }
            val s = out.toString()
            if (!wellFormed(s)) throw Malformed("lone surrogate")
            return s
        }

        private fun number(): Any {
            val start = pos
            if (peek() == '-') pos++
            if (peek() == '0') {
                pos++
            } else if (peek() in '1'..'9') {
                while (peek() in '0'..'9') pos++
            } else {
                throw Malformed("bad number")
            }
            var integer = true
            if (peek() == '.') {
                integer = false
                pos++
                if (peek() !in '0'..'9') throw Malformed("bad number")
                while (peek() in '0'..'9') pos++
            }
            if (peek() == 'e' || peek() == 'E') {
                integer = false
                pos++
                if (peek() == '+' || peek() == '-') pos++
                if (peek() !in '0'..'9') throw Malformed("bad number")
                while (peek() in '0'..'9') pos++
            }
            val raw = text.substring(start, pos)
            if (integer) raw.toLongOrNull()?.let { return it }
            return JsonNumber(raw)
        }

        private fun peek(): Char = if (pos < text.length) text[pos] else '\u0000'

        private fun next(): Char {
            if (pos >= text.length) throw Malformed("unexpected end")
            return text[pos++]
        }

        private fun expect(c: Char) {
            if (next() != c) throw Malformed("expected '$c'")
        }

        private fun wellFormed(s: String): Boolean {
            var i = 0
            while (i < s.length) {
                val c = s[i]
                when {
                    Character.isHighSurrogate(c) -> {
                        if (i + 1 >= s.length || !Character.isLowSurrogate(s[i + 1])) return false
                        i += 2
                    }
                    Character.isLowSurrogate(c) -> return false
                    else -> i++
                }
            }
            return true
        }
    }
}
