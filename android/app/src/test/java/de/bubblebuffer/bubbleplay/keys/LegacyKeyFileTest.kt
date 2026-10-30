package de.bubblebuffer.bubbleplay.keys

import de.bubblebuffer.bubbleplay.Fixtures
import java.security.SecureRandom
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/** Files written under the old format name: still read and unlocked, converted to the shared name, never written. */
class LegacyKeyFileTest {
    private val legacy = legacyKeyFiles()
    private val plainVector = legacy.getJSONObject("plain")
    private val plainFile = plainVector.getJSONObject("file")
    private val protectedVector = legacy.getJSONObject("passphrase")
    private val protectedFile = protectedVector.getJSONObject("file")
    private val passphrase = protectedVector.getString("passphrase")
    private val sharedPlainFile = sharedKeyFiles().getJSONObject("plain").getJSONObject("file")

    @Test
    fun legacyFilesParseAndAreMarkedAsLegacy() {
        val plain = KeyFileCodec.parse(plainFile.toString())
        assertTrue(plain.isLegacyFormat)
        assertEquals(KeyFileCodec.LEGACY_FORMAT, plain.format)
        assertTrue(KeyFileCodec.parse(protectedFile.toString()).isLegacyFormat)
        assertFalse(KeyFileCodec.parse(sharedPlainFile.toString()).isLegacyFormat)
    }

    @Test
    fun legacyPlainFileUnlocksToItsIdentity() {
        val file = KeyFileCodec.parse(plainFile.toString())
        KeyFileCodec.unlock(file, null).useKey { key ->
            assertEquals(file.id, key.id)
            assertTrue(key.withSeed { it.contentEquals(hex(plainVector.getString("seedHex"))) })
        }
    }

    @Test
    fun legacyProtectedFileUnlocksWithItsAadUnderTheLegacyName() {
        val file = KeyFileCodec.parse(protectedFile.toString())
        val aad = KeyFileCodec.authenticatedData(file)
        assertEquals(protectedVector.getString("aadUtf8"), String(aad, Charsets.UTF_8))
        assertEquals(protectedVector.getString("aadHex"), aad.toHex())
        assertTrue(String(aad, Charsets.UTF_8).contains("\"format\":\"${KeyFileCodec.LEGACY_FORMAT}\""))
        KeyFileCodec.unlock(file, passphrase).useKey { key ->
            assertEquals(file.id, key.id)
            assertTrue(key.withSeed { it.contentEquals(hex(protectedVector.getString("seedHex"))) })
        }
    }

    @Test
    fun legacyFailuresAreAuthenticationFailures() {
        val failures = Fixtures.objects(legacy.getJSONArray("failures"))
        assertEquals(listOf("wrong-passphrase", "tampered-aad"), failures.map { it.getString("name") })
        for (vector in failures) {
            val file = KeyFileCodec.parse(vector.getJSONObject("file").toString())
            refusedWith(DeviceKeyException.Reason.AUTHENTICATION_FAILED) {
                KeyFileCodec.unlock(file, vector.getString("passphrase"))
            }
        }
    }

    @Test
    fun serializeWritesTheSharedFormatForAPlainLegacyFile() {
        val text = KeyFileCodec.serialize(KeyFileCodec.parse(plainFile.toString()))
        assertEquals(KeyFileCodec.FORMAT, KeyJson.parseObject(text)["format"])
        assertFalse(KeyFileCodec.parse(text).isLegacyFormat)
    }

    @Test
    fun serializeRefusesAProtectedLegacyFileBecauseItsSealIsBoundToTheOldName() {
        illegalArgument { KeyFileCodec.serialize(KeyFileCodec.parse(protectedFile.toString())) }
    }

    @Test
    fun convertLegacyPlainKeepsIdentityLabelTimeAndSeedInTheSharedFormat() {
        val file = KeyFileCodec.parse(plainFile.toString())
        val converted = KeyFileCodec.parse(KeyFileCodec.convertLegacy(file, null))
        assertFalse(converted.isLegacyFormat)
        assertFalse(converted.isProtected)
        assertEquals(file.id, converted.id)
        assertEquals(file.label, converted.label)
        assertEquals(file.created, converted.created)
        assertArrayEquals(file.publicKey, converted.publicKey)
        KeyFileCodec.unlock(converted, null).useKey { key ->
            assertTrue(key.withSeed { it.contentEquals(hex(plainVector.getString("seedHex"))) })
        }
    }

    @Test
    fun convertLegacyProtectedKeepsThePassphraseUnderAFreshSaltAndNonce() {
        val file = KeyFileCodec.parse(protectedFile.toString())
        val converted = KeyFileCodec.parse(KeyFileCodec.convertLegacy(file, passphrase, SecureRandom()))
        assertFalse(converted.isLegacyFormat)
        assertTrue(converted.isProtected)
        assertEquals(file.id, converted.id)
        assertEquals(file.label, converted.label)
        assertEquals(file.created, converted.created)
        assertNotEquals(file.salt!!.toHex(), converted.salt!!.toHex())
        assertNotEquals(file.nonce!!.toHex(), converted.nonce!!.toHex())
        KeyFileCodec.unlock(converted, passphrase).useKey { key ->
            assertEquals(file.id, key.id)
            assertTrue(key.withSeed { it.contentEquals(hex(protectedVector.getString("seedHex"))) })
        }
    }

    @Test
    fun convertLegacyWithTheWrongPassphraseIsRefused() {
        val file = KeyFileCodec.parse(protectedFile.toString())
        refusedWith(DeviceKeyException.Reason.AUTHENTICATION_FAILED) {
            KeyFileCodec.convertLegacy(file, "not the passphrase", SecureRandom())
        }
    }

    @Test
    fun convertLegacyRefusesAFileThatIsAlreadyInTheSharedFormat() {
        illegalArgument { KeyFileCodec.convertLegacy(KeyFileCodec.parse(sharedPlainFile.toString()), null) }
    }
}
