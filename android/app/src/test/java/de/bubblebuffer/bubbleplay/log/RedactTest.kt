package de.bubblebuffer.bubbleplay.log

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/** The shared redaction corpus (logging spec section 2, rules 1 to 6). */
class RedactTest {
    private fun text(s: String, secrets: List<String> = emptyList()) = Redact.text(s, secrets)

    @Test
    fun rule1SensitiveFieldKeysReplaceTheWholeValue() {
        for (key in listOf(
            "password", "Passphrase", "passwd", "client_secret", "accessToken", "api-key", "apiKey", "pubkey",
            "proof", "ticket", "pairingCode", "pairing", "psk", "signature", "nonce_secret", "Authorization",
            "x.auth", "cookie", "credentials", "relay_code",
        )) {
            assertEquals("key $key", Redact.MASK, Redact.field(key, "value-123"))
        }
        assertEquals(Redact.MASK, Redact.field("password", 42))
        assertEquals(Redact.MASK, Redact.field("token", mapOf("a" to "b")))
    }

    @Test
    fun rule1AllowedKeysAreKept() {
        for (key in listOf("keyboard", "keyframe", "keycode", "keyId", "device_key_id", "keyCode", "key_id", "pc", "state", "failure", "wsClose")) {
            assertEquals("key $key", "plain", Redact.field(key, "plain"))
        }
    }

    @Test
    fun rule1AppliesInsideNestedValues() {
        val out = Redact.field("outer", mapOf("token" to "t", "list" to listOf(mapOf("password" to "p", "n" to 1)), "ok" to "fine"))
        assertEquals(mapOf("token" to Redact.MASK, "list" to listOf(mapOf("password" to Redact.MASK, "n" to 1)), "ok" to "fine"), out)
    }

    @Test
    fun rule2UrlParametersAreReplaced() {
        assertEquals(
            "GET https://relay.example/ws?pc=ABCD&token=[redacted]&x=1",
            text("GET https://relay.example/ws?pc=ABCD&token=abc123&x=1"),
        )
        assertEquals("https://relay.example/pair#pc=ABCD&code=[redacted]", text("https://relay.example/pair#pc=ABCD&code=ABCDE-FGHIJ"))
        assertEquals("/x?Sig=[redacted]&key=[redacted]", text("/x?Sig=aaa&key=bbb"))
        assertEquals("/x?ticket=[redacted]&proof=[redacted]&pairing=[redacted]", text("/x?ticket=1&proof=2&pairing=3"))
    }

    @Test
    fun rule3BearerAndAuthorizationHeaders() {
        assertEquals("Bearer [redacted]", text("Bearer abc.def-ghi"))
        assertEquals("sent Bearer [redacted] ok", text("sent bearer abc.def ok"))
        assertEquals("Authorization: [redacted]\nnext line", text("Authorization: Basic zzz yyy\nnext line"))
        assertFalse(text("authorization: Digest user=\"x\", response=\"abc\"").contains("abc"))
    }

    @Test
    fun rule4KeyValueInFreeText() {
        assertEquals("login password=[redacted] ok", text("login password=hunter2 ok"))
        assertEquals("passphrase: \"[redacted]\" next", text("passphrase: \"my long phrase\" next"))
        assertEquals("{\"ticket\":\"[redacted]\",\"n\":1}", text("{\"ticket\":\"abc\",\"n\":1}"))
        assertEquals("pairing code: [redacted]", text("pairing code: ABCDE-FGHIJ"))
        assertEquals("token='[redacted]'", text("token='abc'"))
        assertEquals("state: AuthHost count=3 keyboard=us device_key_id=XYZ", text("state: AuthHost count=3 keyboard=us device_key_id=XYZ"))
    }

    @Test
    fun rule5RegisteredSecretsLongestFirst() {
        assertEquals("x[redacted]x", text("xabcdefx", listOf("abc", "abcdef")))
        assertEquals("[redacted] and [redacted]", text("hunter2 and hunter2", listOf("hunter2")))
        assertEquals("kept", text("kept", listOf("")))
    }

    @Test
    fun rule6LongRunsBecomeBlobs() {
        val blob40 = "A".repeat(40)
        val blob39 = "A".repeat(39)
        assertEquals("proof ${Redact.BLOB} end", text("proof $blob40 end"))
        assertEquals("proof $blob39 end", text("proof $blob39 end"))
        assertEquals(Redact.BLOB, Redact.field("note", "abcd1234+/_=-".repeat(5)))
        val base64 = "dGhpcyBpcyBhIHNlY3JldCBrZXkgdGhhdCBzaG91bGQgbmV2ZXIgYmUgbG9nZ2Vk"
        assertFalse(text("seed $base64").contains(base64))
    }

    @Test
    fun alreadyRedactedTextIsStable() {
        val once = text("password=hunter2 /x?token=abc Bearer q")
        assertEquals(once, text(once))
        assertTrue(once.contains(Redact.MASK))
    }

    @Test
    fun numbersAndBooleansPassThroughOnSafeKeys() {
        assertEquals(7, Redact.field("count", 7))
        assertEquals(true, Redact.field("admitted", true))
        assertEquals(null, Redact.field("detail", null))
    }

    @Test
    fun closeCodeIsAPlainNumberAndPassesButOtherCodesStayRedacted() {
        assertEquals(1006, Redact.field("closeCode", 1006))
        assertEquals("signaling down: closed 1001 (closeCode=1001)", text("signaling down: closed 1001 (closeCode=1001)"))
        assertEquals("code=" + Redact.MASK, text("code=1006"))
        assertEquals(Redact.MASK, Redact.field("code", 1006))
        assertEquals(Redact.MASK, Redact.field("relay_code", 1006))
        assertEquals(Redact.MASK, Redact.field("closeToken", "abc"))
        assertEquals(Redact.MASK, Redact.field("authCloseCode", 1))
    }
}
