package de.bubblebuffer.bubbleplay.log

/**
 * Redaction for log lines (logging spec section 2, rules 1 to 6). Pure and never throws to callers
 * of [BpLog]; [BpLog] wraps every call and falls back to "[redaction failed]" (rule 7).
 *
 * Never log a secret in the first place: this is the net, not the plan.
 */
object Redact {
    const val MASK = "[redacted]"
    const val BLOB = "[redacted-blob]"

    private val SENSITIVE = listOf(
        "password", "passphrase", "passwd", "secret", "token", "key", "proof", "ticket", "code",
        "pairing", "psk", "signature", "cookie", "credential", "auth",
    )
    private val KEY_ALLOWED = listOf("keyboard", "keyframe", "keycode", "keyid")
    private val CAMEL = Regex("(?<=[a-z0-9])(?=[A-Z])")
    private val SPLIT = Regex("""[_\-.\s]+""")

    // Rule 2: URL query and fragment parameters.
    private val URL_PARAM = Regex(
        """([?&#;])(token|key|secret|password|passphrase|code|ticket|proof|pairing|sig)=([^&#\s"']*)""",
        RegexOption.IGNORE_CASE,
    )

    // Rule 3.
    private val BEARER = Regex("""Bearer\s+[^\s"',;]+""", RegexOption.IGNORE_CASE)
    private val AUTH_HEADER = Regex("""Authorization\s*:[^\r\n]*""", RegexOption.IGNORE_CASE)

    // Rule 4: key=value, key: value, "key":"value".
    private val KEY_VALUE = Regex(
        """(["']?)([A-Za-z_][A-Za-z0-9_.\-]*)\1(\s*[:=]\s*)""" +
            """(\[redacted(?:-blob)?\]|"(?:[^"\\]|\\.)*"|'[^']*'|[^\s,;&)}\]"']+)""",
    )

    // Rule 6.
    private val LONG_RUN = Regex("[A-Za-z0-9+/_=-]{40,}")

    /** Rule 1: does a field or text key name a secret? Splits on _ - . whitespace and camelCase. */
    fun isSensitiveKey(key: String): Boolean {
        val tokens = key.replace(CAMEL, "_").split(SPLIT).filter { it.isNotEmpty() }.map { it.lowercase() }
        var i = 0
        while (i < tokens.size) {
            val token = tokens[i]
            if (token == "key" && i + 1 < tokens.size && tokens[i + 1] in setOf("id", "code")) {
                i += 2
                continue
            }
            var cleaned = token
            for (allowed in KEY_ALLOWED) cleaned = cleaned.replace(allowed, "")
            if (SENSITIVE.any { cleaned.contains(it) }) return true
            i++
        }
        return false
    }

    /** Rules 2 to 6 on one string. [secrets] are the registered literals (rule 5). */
    fun text(input: String, secrets: Collection<String> = emptyList()): String {
        var s = input
        for (secret in secrets.filter { it.isNotEmpty() }.sortedByDescending { it.length }) {
            s = s.replace(secret, MASK)
        }
        s = AUTH_HEADER.replace(s, "Authorization: $MASK")
        s = BEARER.replace(s, "Bearer $MASK")
        s = URL_PARAM.replace(s) { m -> "${m.groupValues[1]}${m.groupValues[2]}=$MASK" }
        s = KEY_VALUE.replace(s) { m ->
            val value = m.groupValues[4]
            if (value.startsWith("[redacted") || !isSensitiveKey(m.groupValues[2])) {
                m.value
            } else {
                val quote = value.firstOrNull()?.takeIf { it == '"' || it == '\'' }
                val masked = if (quote != null) "$quote$MASK$quote" else MASK
                "${m.groupValues[1]}${m.groupValues[2]}${m.groupValues[1]}${m.groupValues[3]}$masked"
            }
        }
        return LONG_RUN.replace(s, BLOB)
    }

    /** A field value: rule 1 on its key, rules 2 to 6 on every string inside. */
    fun field(key: String?, raw: Any?, secrets: Collection<String> = emptyList()): Any? {
        if (key != null && isSensitiveKey(key)) return MASK
        return when (raw) {
            null, is Number, is Boolean -> raw
            is String -> text(raw, secrets)
            is Map<*, *> -> raw.entries.associate { (k, v) ->
                val name = k.toString()
                text(name, secrets) to field(name, v, secrets)
            }
            is Iterable<*> -> raw.map { field(null, it, secrets) }
            is Array<*> -> raw.map { field(null, it, secrets) }
            is Throwable -> text(raw.javaClass.name + ": " + (raw.message ?: ""), secrets)
            else -> text(raw.toString(), secrets)
        }
    }
}
