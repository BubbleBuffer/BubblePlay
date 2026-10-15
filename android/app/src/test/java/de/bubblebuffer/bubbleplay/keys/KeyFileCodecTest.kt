package de.bubblebuffer.bubbleplay.keys

import de.bubblebuffer.bubbleplay.Fixtures
import java.security.SecureRandom
import java.time.Instant
import org.json.JSONObject
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class KeyFileCodecTest {
    private val keyFiles = contractKeyFiles()
    private val plainVector = keyFiles.getJSONObject("plain")
    private val plainFile = plainVector.getJSONObject("file")
    private val protectedVector = keyFiles.getJSONObject("passphrase")
    private val protectedFile = protectedVector.getJSONObject("file")
    private val failures = Fixtures.objects(keyFiles.getJSONArray("failures"))
    private val passphrase = protectedVector.getString("passphrase")

    @Test
    fun plainFixtureParsesAndUnlocksToItsIdentity() {
        val file = KeyFileCodec.parse(plainFile.toString())
        assertFalse(file.isProtected)
        assertEquals(plainFile.getString("id"), file.id)
        assertEquals(plainFile.getString("label"), file.label)
        assertEquals(plainFile.getString("created"), file.created)
        assertArrayEquals(hex(plainVector.getString("seedHex")), file.sealedSeed)
        KeyFileCodec.unlock(file, null).useKey { key ->
            assertEquals(file.id, key.id)
            assertArrayEquals(file.publicKey, key.publicKey)
            assertTrue(key.withSeed { it.contentEquals(hex(plainVector.getString("seedHex"))) })
        }
    }

    @Test
    fun serializeReproducesTheFixtureFiles() {
        // Structural maps from the strict parser: field order is ignored, values and nesting are not.
        assertEquals(
            KeyJson.parseObject(plainFile.toString()),
            KeyJson.parseObject(KeyFileCodec.serialize(KeyFileCodec.parse(plainFile.toString()))),
        )
        assertEquals(
            KeyJson.parseObject(protectedFile.toString()),
            KeyJson.parseObject(KeyFileCodec.serialize(KeyFileCodec.parse(protectedFile.toString()))),
        )
    }

    @Test
    fun protectedFixtureAadMatchesExactUtf8AndHex() {
        val file = KeyFileCodec.parse(protectedFile.toString())
        val aad = KeyFileCodec.authenticatedData(file)
        assertEquals(protectedVector.getString("aadUtf8"), String(aad, Charsets.UTF_8))
        assertEquals(protectedVector.getString("aadHex"), aad.toHex())
    }

    @Test
    fun protectedFixtureUnlocksWithItsPassphrase() {
        val file = KeyFileCodec.parse(protectedFile.toString())
        assertTrue(file.isProtected)
        KeyFileCodec.unlock(file, passphrase).useKey { key ->
            assertEquals(file.id, key.id)
            assertTrue(key.withSeed { it.contentEquals(hex(protectedVector.getString("seedHex"))) })
        }
    }

    @Test
    fun wrongPassphraseAndTamperedAadAreAuthenticationFailures() {
        assertEquals(listOf("wrong-passphrase", "tampered-aad"), failures.map { it.getString("name") })
        for (vector in failures) {
            val file = KeyFileCodec.parse(vector.getJSONObject("file").toString())
            refusedWith(DeviceKeyException.Reason.AUTHENTICATION_FAILED) {
                KeyFileCodec.unlock(file, vector.getString("passphrase"))
            }
        }
    }

    @Test
    fun unknownFormatVersionAndTypeAreRefused() {
        refusedWith(DeviceKeyException.Reason.UNSUPPORTED_FORMAT) {
            KeyFileCodec.parse(mutatedText(plainFile) { put("format", "something-else") })
        }
        refusedWith(DeviceKeyException.Reason.UNSUPPORTED_VERSION) {
            KeyFileCodec.parse(mutatedText(plainFile) { put("version", 2) })
        }
        refusedWith(DeviceKeyException.Reason.UNSUPPORTED_VERSION) {
            KeyFileCodec.parse(mutatedText(plainFile) { put("version", "1") })
        }
        refusedWith(DeviceKeyException.Reason.UNSUPPORTED_TYPE) {
            KeyFileCodec.parse(mutatedText(plainFile) { put("type", "rsa") })
        }
        refusedWith(DeviceKeyException.Reason.UNSUPPORTED_TYPE) {
            KeyFileCodec.parse(mutatedText(protectedFile) { put("type", "x25519") })
        }
    }

    @Test
    fun strictChecksRefuseMalformedFilesWithoutUnlocking() {
        val publicKey = plainFile.getString("public")
        val plainPrivate = plainFile.getString("private")
        val malformed = listOf(
            "padded public" to mutatedText(plainFile) { put("public", publicKey + "=") },
            "standard alphabet" to mutatedText(plainFile) { put("public", publicKey.replace('-', '+')) },
            "non-canonical trailing bits" to mutatedText(plainFile) { put("public", publicKey.dropLast(1) + "x") },
            "lowercase id" to mutatedText(plainFile) { put("id", plainFile.getString("id").lowercase()) },
            "unknown field" to mutatedText(plainFile) { put("extra", "x") },
            "missing label" to mutatedText(plainFile) { remove("label") },
            "created without offset" to mutatedText(plainFile) { put("created", "2026-10-13T09:30:00") },
            "created with space separator" to mutatedText(plainFile) { put("created", "2026-10-13 09:30:00Z") },
            "created without seconds" to mutatedText(plainFile) { put("created", "2026-10-13T09:30Z") },
            "created impossible date" to mutatedText(plainFile) { put("created", "2026-02-30T09:30:00Z") },
            "plain seed of 33 bytes" to mutatedText(plainFile) {
                put("private", StrictBase64Url.encode(ByteArray(33)))
            },
            "aead without kdf" to mutatedText(plainFile) {
                put("aead", JSONObject().put("alg", "aes-256-gcm").put("nonce", "EBESExQVFhcYGRob"))
            },
            "kdf with m=1024" to mutatedText(protectedFile) { getJSONObject("kdf").put("m", 1024) },
            "kdf with extra field" to mutatedText(protectedFile) { getJSONObject("kdf").put("x", 1) },
            "protected seed of 47 bytes" to mutatedText(protectedFile) {
                put("private", StrictBase64Url.encode(ByteArray(47)))
            },
            "label with escaped lone surrogate" to
                mutatedText(plainFile) {}.replace("\"label\":\"PC test\"", "\"label\":\"\\ud800\""),
            "duplicate label" to mutatedText(plainFile) {}.replaceFirst("\"id\"", "\"label\":\"x\",\"id\""),
            "trailing data" to mutatedText(plainFile) {} + "x",
            "byte order mark" to "\uFEFF" + mutatedText(plainFile) {},
            "fractional version" to mutatedText(plainFile) {}.replace("\"version\":1", "\"version\":1.0"),
            "standard alphabet in private" to
                mutatedText(plainFile) { put("private", plainPrivate.replace('-', '/').replace('_', '+')) },
        )
        for ((name, text) in malformed) {
            try {
                KeyFileCodec.parse(text)
                throw AssertionError("accepted: $name")
            } catch (e: DeviceKeyException) {
                assertTrue("$name refused as ${e.reason}", e.reason == DeviceKeyException.Reason.MALFORMED)
            }
        }
    }

    @Test
    fun identityMismatchIsRefusedAtParse() {
        val other = contractIds().first { it.getString("id") != plainFile.getString("id") }
        refusedWith(DeviceKeyException.Reason.IDENTITY_MISMATCH) {
            KeyFileCodec.parse(mutatedText(plainFile) { put("id", other.getString("id")) })
        }
    }

    @Test
    fun seedMustMatchThePublicKeyAndIdTheFileStoresWithIt() {
        // The ID and public key agree with each other, so parse accepts; the plain seed does not match.
        val other = contractIds().first { it.getString("name") == "viewer" }
        val file = KeyFileCodec.parse(mutatedText(plainFile) {
            put("id", other.getString("id"))
            put("public", other.getString("public"))
        })
        refusedWith(DeviceKeyException.Reason.IDENTITY_MISMATCH) { KeyFileCodec.unlock(file, null) }
    }

    @Test
    fun plainFileWithAModifiedSeedIsRefusedAtUnlock() {
        // ID and public key still belong to the original fixture; only the stored seed changes.
        val viewerSeed = contractIds().first { it.getString("name") == "viewer" }.getString("seedHex")
        val file = KeyFileCodec.parse(mutatedText(plainFile) {
            put("private", StrictBase64Url.encode(hex(viewerSeed)))
        })
        assertEquals(plainFile.getString("id"), file.id)
        refusedWith(DeviceKeyException.Reason.IDENTITY_MISMATCH) { KeyFileCodec.unlock(file, null) }
    }

    @Test
    fun protectedFixtureCarriesMatchingPublicKeyAndId() {
        val file = KeyFileCodec.parse(protectedFile.toString())
        assertArrayEquals(StrictBase64Url.decode(protectedFile.getString("public"), 32), file.publicKey)
        assertEquals(protectedFile.getString("id"), DeviceId.fromPublicKey(file.publicKey))
        DeviceKey.fromSeed(hex(protectedVector.getString("seedHex"))).useKey { key ->
            assertArrayEquals(file.publicKey, key.publicKey)
            assertEquals(file.id, key.id)
        }
    }

    @Test
    fun keyFileArraysAreOwnedAndCopiedAtBothEnds() {
        val source = byteArrayOf(1, 2, 3)
        val file = KeyFile("ID", "x", "2026-10-13T09:30:00Z", ByteArray(32), null, null, source)
        source.fill(9)
        assertArrayEquals(byteArrayOf(1, 2, 3), file.sealedSeed)
        file.sealedSeed.fill(9)
        assertArrayEquals(byteArrayOf(1, 2, 3), file.sealedSeed)
        file.publicKey.fill(9)
        assertArrayEquals(ByteArray(32), file.publicKey)
    }

    @Test
    fun metadataTimestampsAcceptedAndAuthenticatedVerbatim() {
        val variants = listOf(
            "2026-10-13T09:30:00Z",
            "2026-10-13T09:30:00.5Z",
            "2026-10-13T09:30:00.000000001-05:30",
            "2026-10-13T09:30:00+02:00",
        )
        for (created in variants) {
            val file = KeyFileCodec.parse(mutatedText(plainFile) { put("created", created) })
            assertEquals(created, file.created)
        }
    }

    @Test
    fun protectedFileWithOffsetTimestampAndUnusualLabelRoundTrips() {
        val created = "2026-10-13T11:30:00.123+02:00"
        val label = "" // empty labels are valid
        val plain = KeyFileCodec.parse(mutatedText(plainFile) {
            put("created", created)
            put("label", label)
        })
        KeyFileCodec.unlock(plain, null).useKey { key ->
            val sealedFile = KeyFileCodec.rewrap(plain, key, "pw", SecureRandom())
            assertEquals(created, sealedFile.created)
            assertEquals(label, sealedFile.label)
            val text = KeyFileCodec.serialize(sealedFile)
            assertTrue(String(KeyFileCodec.authenticatedData(sealedFile), Charsets.UTF_8)
                .contains("\"created\":\"$created\""))
            KeyFileCodec.unlock(KeyFileCodec.parse(text), "pw").useKey { again ->
                assertEquals(key.id, again.id)
            }
        }
    }

    @Test
    fun longLabelsAndAnyWellFormedTextAreAccepted() {
        val long = "é".repeat(20_000)
        val file = KeyFileCodec.parse(mutatedText(plainFile) { put("label", long + "😀\u0001\"") })
        assertEquals(long + "😀\u0001\"", file.label)
        assertEquals(long + "😀\u0001\"", KeyFileCodec.parse(KeyFileCodec.serialize(file)).label)
    }

    @Test
    fun emptyPassphraseIsAValidProtectionPassphrase() {
        val created = KeyFileCodec.create("Empty", "")
        assertTrue(created.file.isProtected)
        val parsed = KeyFileCodec.parse(KeyFileCodec.serialize(created.file))
        KeyFileCodec.unlock(parsed, "").useKey { key -> assertEquals(created.key.id, key.id) }
        refusedWith(DeviceKeyException.Reason.AUTHENTICATION_FAILED) { KeyFileCodec.unlock(parsed, " ") }
        illegalArgument { KeyFileCodec.unlock(parsed, null) }
        created.key.close()
    }

    @Test
    fun passphraseIsRefusedForPlainFilesAndRequiredForProtectedOnes() {
        val plain = KeyFileCodec.parse(plainFile.toString())
        illegalArgument { KeyFileCodec.unlock(plain, "") }
        val sealed = KeyFileCodec.parse(protectedFile.toString())
        illegalArgument { KeyFileCodec.unlock(sealed, null) }
    }

    @Test
    fun createSerializeParseUnlockWithoutPassphrase() {
        val now = Instant.parse("2026-10-15T09:30:00.750Z")
        val created = KeyFileCodec.create("Test phone", null, now, SecureRandom())
        assertEquals("2026-10-15T09:30:00Z", created.file.created)
        assertFalse(created.file.isProtected)
        val parsed = KeyFileCodec.parse(KeyFileCodec.serialize(created.file))
        assertEquals(created.key.id, parsed.id)
        KeyFileCodec.unlock(parsed, null).useKey { key ->
            assertEquals(created.key.id, key.id)
            assertArrayEquals(created.key.publicKey, key.publicKey)
        }
        created.key.close()
    }

    @Test
    fun createSerializeParseUnlockWithPassphrase() {
        val random = SecureRandom()
        val created = KeyFileCodec.create("Phone", "correct horse battery staple", random = random)
        assertTrue(created.file.isProtected)
        assertEquals(16, created.file.salt?.size)
        assertEquals(12, created.file.nonce?.size)
        assertEquals(48, created.file.sealedSeed.size)
        val parsed = KeyFileCodec.parse(KeyFileCodec.serialize(created.file))
        KeyFileCodec.unlock(parsed, "correct horse battery staple").useKey { key ->
            assertEquals(created.key.id, key.id)
            assertArrayEquals(created.key.publicKey, key.publicKey)
        }
        refusedWith(DeviceKeyException.Reason.AUTHENTICATION_FAILED) {
            KeyFileCodec.unlock(parsed, "correct horse battery stapl")
        }
        created.key.close()
    }

    @Test
    fun rewrapChangesPassphraseWithFreshSaltAndNonceAndKeepsIdentity() {
        val random = SecureRandom()
        val first = KeyFileCodec.create("Viewer", "old phrase", random = random)
        val unlocked = KeyFileCodec.unlock(KeyFileCodec.parse(KeyFileCodec.serialize(first.file)), "old phrase")

        val second = KeyFileCodec.rewrap(first.file, unlocked, "new phrase", random)

        assertEquals(first.file.id, second.id)
        assertEquals(first.file.label, second.label)
        assertEquals(first.file.created, second.created)
        assertArrayEquals(first.file.publicKey, second.publicKey)
        assertNotEquals(first.file.salt!!.toHex(), second.salt!!.toHex())
        assertNotEquals(first.file.nonce!!.toHex(), second.nonce!!.toHex())
        assertNotEquals(first.file.sealedSeed.toHex(), second.sealedSeed.toHex())

        KeyFileCodec.unlock(KeyFileCodec.parse(KeyFileCodec.serialize(second)), "new phrase").useKey { key ->
            assertEquals(first.file.id, key.id)
        }
        refusedWith(DeviceKeyException.Reason.AUTHENTICATION_FAILED) { KeyFileCodec.unlock(second, "old phrase") }

        val plain = KeyFileCodec.rewrap(second, unlocked, null, random)
        assertFalse(plain.isProtected)
        assertEquals(first.file.id, plain.id)
        KeyFileCodec.unlock(KeyFileCodec.parse(KeyFileCodec.serialize(plain)), null).useKey { key ->
            assertEquals(first.file.id, key.id)
        }

        unlocked.close()
        first.key.close()
    }

    @Test
    fun rewrapRefusesAKeyFromAnotherIdentity() {
        val mine = KeyFileCodec.create("Mine", null)
        val other = KeyFileCodec.create("Other", null)
        refusedWith(DeviceKeyException.Reason.IDENTITY_MISMATCH) {
            KeyFileCodec.rewrap(mine.file, other.key, null)
        }
        mine.key.close()
        other.key.close()
    }
}
