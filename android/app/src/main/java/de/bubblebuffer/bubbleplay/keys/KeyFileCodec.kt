package de.bubblebuffer.bubbleplay.keys

import de.bubblebuffer.bubbleplay.keys.DeviceKeyException.Reason
import de.bubblebuffer.bubbleplay.log.BpLog
import java.security.GeneralSecurityException
import java.security.MessageDigest
import java.security.SecureRandom
import java.time.Instant
import java.time.OffsetDateTime
import java.time.format.DateTimeParseException
import java.time.temporal.ChronoUnit
import javax.crypto.Cipher
import javax.crypto.spec.GCMParameterSpec
import javax.crypto.spec.SecretKeySpec
import org.bouncycastle.crypto.generators.Argon2BytesGenerator
import org.bouncycastle.crypto.params.Argon2Parameters

/** A new key: the file to store and the unlocked key to use straight away. */
class NewDeviceKey(val file: KeyFile, val key: DeviceKey)

/**
 * Creates, parses, unlocks, rewraps and serialises `.bpkey` files (format in
 * docs/architecture/device-keys.md). Passphrases are UTF-8 with no normalisation.
 * Parsing checks everything it can before the Argon2id step, which only [unlock] and
 * [create]/[rewrap] with a passphrase run.
 */
object KeyFileCodec {
    /** The shared format name; the only one [parse] accepts and [serialize] writes. */
    internal const val FORMAT = "bubblebuffer-device-key"
    private const val TYPE = "ed25519"
    private const val VERSION = 1L
    private const val KDF_ALG = "argon2id"
    private const val AEAD_ALG = "aes-256-gcm"
    private const val ARGON2_MEMORY_KIB = 65536
    private const val ARGON2_PASSES = 3
    private const val ARGON2_LANES = 1
    private const val KEY_BYTES = 32
    private const val SALT_BYTES = 16
    private const val NONCE_BYTES = 12
    private const val TAG_BYTES = 16
    private const val TAG_BITS = 128
    private val FIELDS = setOf("format", "version", "type", "id", "label", "created", "public", "kdf", "aead", "private")
    private val KDF_FIELDS = setOf("alg", "m", "t", "p", "salt")
    private val AEAD_FIELDS = setOf("alg", "nonce")
    /** ISO 8601 date-time with a required offset; fractions of a second are optional. */
    private val CREATED = Regex("""\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})""")

    /** Strictly parses key-file text (decode the file as UTF-8 before calling). */
    fun parse(text: String): KeyFile {
        val root = KeyJson.parseObject(text)
        if (root["format"] != FORMAT) refuse(Reason.UNSUPPORTED_FORMAT, "not a BubblePlay device key")
        if (root["version"] != VERSION) refuse(Reason.UNSUPPORTED_VERSION, "unsupported key file version")
        if (root["type"] != TYPE) refuse(Reason.UNSUPPORTED_TYPE, "unsupported key type")
        if (root.keys != FIELDS) malformed("unexpected or missing key file fields")

        val id = root.string("id")
        if (!DeviceId.isCanonical(id)) malformed("device ID is not in canonical form")
        val label = root.string("label")
        checkLabel(label)
        val created = root.string("created")
        checkCreated(created)
        // label and created are kept verbatim: they are authenticated as written.
        val publicKey = StrictBase64Url.decode(root.string("public"), KEY_BYTES)
        if (DeviceId.fromPublicKey(publicKey) != id) {
            refuse(Reason.IDENTITY_MISMATCH, "device ID does not match the public key")
        }
        val sealed = StrictBase64Url.decode(root.string("private"))

        try {
            // Everything below is checked before any Argon2id work.
            if (root["kdf"] == null) {
                if (root["aead"] != null) malformed("aead is set but kdf is not")
                if (sealed.size != KEY_BYTES) malformed("an unprotected key holds exactly a 32-byte seed")
                return KeyFile(id, label, created, publicKey, null, null, sealed)
            }
            val kdf = asObject(root["kdf"]) ?: malformed("kdf must be an object or null")
            if (kdf.keys != KDF_FIELDS) malformed("unexpected kdf fields")
            if (kdf["alg"] != KDF_ALG || kdf["m"] != ARGON2_MEMORY_KIB.toLong() ||
                kdf["t"] != ARGON2_PASSES.toLong() || kdf["p"] != ARGON2_LANES.toLong()
            ) {
                malformed("unsupported Argon2id parameters")
            }
            val aead = asObject(root["aead"]) ?: malformed("aead must be an object when kdf is set")
            if (aead.keys != AEAD_FIELDS || aead["alg"] != AEAD_ALG) malformed("unsupported AEAD fields")
            val salt = StrictBase64Url.decode(kdf.string("salt"), SALT_BYTES)
            val nonce = StrictBase64Url.decode(aead.string("nonce"), NONCE_BYTES)
            if (sealed.size != KEY_BYTES + TAG_BYTES) malformed("a protected key holds a 32-byte seed plus its tag")
            return KeyFile(id, label, created, publicKey, salt, nonce, sealed)
        } finally {
            // KeyFile owns a copy; this decoded buffer may contain a plain seed.
            sealed.fill(0)
        }
    }

    /** Key-file text in the documented field order. */
    fun serialize(file: KeyFile): String {
        val fields = header(file)
        fields["aead"] = file.nonce?.let { nonce ->
            linkedMapOf<String, Any?>("alg" to AEAD_ALG, "nonce" to StrictBase64Url.encode(nonce))
        }
        val privateBytes = file.sealedSeed
        try {
            fields["private"] = StrictBase64Url.encode(privateBytes)
        } finally {
            privateBytes.fill(0)
        }
        return KeyJson.pretty(fields)
    }

    /**
     * Makes a new key. With a [passphrase] the seed is sealed under Argon2id and AES-256-GCM;
     * without one it is stored as the plain seed. Returns the file and the unlocked key.
     */
    fun create(
        label: String,
        passphrase: String?,
        now: Instant = Instant.now(),
        random: SecureRandom = SecureRandom(),
    ): NewDeviceKey {
        checkLabel(label)
        val created = now.truncatedTo(ChronoUnit.SECONDS).toString()
        val seed = ByteArray(KEY_BYTES).also { random.nextBytes(it) }
        return try {
            val key = DeviceKey.fromSeed(seed)
            try {
                NewDeviceKey(protect(key.id, label, created, key.publicKey, passphrase, seed, random), key).also {
                    BpLog.setDeviceId(key.id)
                    BpLog.i("KeyFileCodec", "device key created", null, mapOf("sealed" to (passphrase != null)))
                }
            } catch (e: Throwable) {
                key.close()
                throw e
            }
        } finally {
            seed.fill(0)
        }
    }

    /**
     * Unlocks [file]. Pass null for an unprotected file and the passphrase otherwise; an empty
     * string is a valid passphrase for a protected file.
     */
    fun unlock(file: KeyFile, passphrase: String?): DeviceKey {
        val salt = file.salt
        val nonce = file.nonce
        if (salt == null || nonce == null) {
            require(passphrase == null) { "an unprotected key takes no passphrase" }
            val plainSeed = file.sealedSeed
            try {
                return verified(file, DeviceKey.fromSeed(plainSeed)).also {
                    BpLog.i("KeyFileCodec", "device key unlocked", null, mapOf("sealed" to false))
                }
            } finally {
                plainSeed.fill(0)
            }
        }
        val phrase = passphrase ?: throw IllegalArgumentException("a protected key needs its passphrase")
        val aad = authenticatedData(file)
        val wrapKey = deriveKey(phrase, salt)
        val seed = try {
            decrypt(wrapKey, nonce, aad, file.sealedSeed)
        } catch (e: GeneralSecurityException) {
            // refuse() below logs the refusal.
            refuse(Reason.AUTHENTICATION_FAILED, "wrong passphrase or altered key file")
        } finally {
            wrapKey.fill(0)
        }
        try {
            if (seed.size != KEY_BYTES) malformed("decrypted seed has the wrong size")
            return verified(file, DeviceKey.fromSeed(seed)).also {
                BpLog.i("KeyFileCodec", "device key unlocked", null, mapOf("sealed" to true))
            }
        } finally {
            seed.fill(0)
        }
    }

    /**
     * Re-seals an unlocked [key] under [passphrase] (null for plain), with a fresh salt and nonce.
     * ID, public key, label and created time are kept. [key] must belong to [file].
     */
    fun rewrap(
        file: KeyFile,
        key: DeviceKey,
        passphrase: String?,
        random: SecureRandom = SecureRandom(),
    ): KeyFile {
        if (key.id != file.id || !MessageDigest.isEqual(key.publicKey, file.publicKey)) {
            refuse(Reason.IDENTITY_MISMATCH, "key does not belong to this key file")
        }
        return key.withSeed { seed ->
            protect(file.id, file.label, file.created, file.publicKey, passphrase, seed, random)
        }
    }

    /** RFC 8785 AAD: every header field except `private` and `aead.nonce`. */
    internal fun authenticatedData(file: KeyFile): ByteArray =
        KeyJson.canonical(header(file)).toByteArray(Charsets.UTF_8)

    private fun header(file: KeyFile): LinkedHashMap<String, Any?> {
        val fields = LinkedHashMap<String, Any?>()
        fields["format"] = FORMAT
        fields["version"] = VERSION
        fields["type"] = TYPE
        fields["id"] = file.id
        fields["label"] = file.label
        fields["created"] = file.created
        fields["public"] = StrictBase64Url.encode(file.publicKey)
        fields["kdf"] = file.salt?.let { salt ->
            LinkedHashMap<String, Any?>().apply {
                put("alg", KDF_ALG)
                put("m", ARGON2_MEMORY_KIB.toLong())
                put("t", ARGON2_PASSES.toLong())
                put("p", ARGON2_LANES.toLong())
                put("salt", StrictBase64Url.encode(salt))
            }
        }
        fields["aead"] = if (file.isProtected) linkedMapOf<String, Any?>("alg" to AEAD_ALG) else null
        return fields
    }

    private fun protect(
        id: String,
        label: String,
        created: String,
        publicKey: ByteArray,
        passphrase: String?,
        seed: ByteArray,
        random: SecureRandom,
    ): KeyFile {
        // KeyFile copies its arguments, so passing the caller's seed does not hand out a second owner.
        if (passphrase == null) return KeyFile(id, label, created, publicKey, null, null, seed)
        val salt = ByteArray(SALT_BYTES).also { random.nextBytes(it) }
        val nonce = ByteArray(NONCE_BYTES).also { random.nextBytes(it) }
        // The AAD does not depend on `private`, so it can be computed before sealing.
        val draft = KeyFile(id, label, created, publicKey, salt, nonce, ByteArray(0))
        val wrapKey = deriveKey(passphrase, salt)
        try {
            val sealed = encrypt(wrapKey, nonce, authenticatedData(draft), seed)
            return KeyFile(id, label, created, publicKey, salt, nonce, sealed)
        } finally {
            wrapKey.fill(0)
        }
    }

    private fun verified(file: KeyFile, key: DeviceKey): DeviceKey {
        if (key.id != file.id || !MessageDigest.isEqual(key.publicKey, file.publicKey)) {
            key.close()
            refuse(Reason.IDENTITY_MISMATCH, "seed does not match the key file")
        }
        return key
    }

    private fun deriveKey(passphrase: String, salt: ByteArray): ByteArray {
        require(KeyJson.isWellFormed(passphrase)) { "passphrase is not well-formed text" }
        val password = passphrase.toByteArray(Charsets.UTF_8)
        val key = ByteArray(KEY_BYTES)
        try {
            val params = Argon2Parameters.Builder(Argon2Parameters.ARGON2_id)
                .withVersion(Argon2Parameters.ARGON2_VERSION_13)
                .withIterations(ARGON2_PASSES)
                .withMemoryAsKB(ARGON2_MEMORY_KIB)
                .withParallelism(ARGON2_LANES)
                .withSalt(salt)
                .build()
            val generator = Argon2BytesGenerator()
            generator.init(params)
            generator.generateBytes(password, key)
        } catch (e: Throwable) {
            // A failed generator may already have written part of the output.
            key.fill(0)
            throw e
        } finally {
            password.fill(0)
        }
        return key
    }

    private fun encrypt(key: ByteArray, nonce: ByteArray, aad: ByteArray, plaintext: ByteArray): ByteArray {
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.ENCRYPT_MODE, SecretKeySpec(key, "AES"), GCMParameterSpec(TAG_BITS, nonce))
        cipher.updateAAD(aad)
        return cipher.doFinal(plaintext)
    }

    private fun decrypt(key: ByteArray, nonce: ByteArray, aad: ByteArray, sealed: ByteArray): ByteArray {
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.DECRYPT_MODE, SecretKeySpec(key, "AES"), GCMParameterSpec(TAG_BITS, nonce))
        cipher.updateAAD(aad)
        return cipher.doFinal(sealed)
    }

    /** Any well-formed text; it is authenticated exactly as written. */
    private fun checkLabel(label: String) {
        if (!KeyJson.isWellFormed(label)) malformed("label is not well-formed text")
    }

    /**
     * ISO 8601 date-time with an offset and optional fractions. The text is kept verbatim for the
     * AAD, so it is validated but never re-formatted.
     */
    private fun checkCreated(created: String) {
        if (!CREATED.matches(created)) malformed("created must be an ISO 8601 date-time with offset")
        try {
            OffsetDateTime.parse(created)
        } catch (e: DateTimeParseException) {
            malformed("created is not a valid date-time")
        }
    }

    @Suppress("UNCHECKED_CAST")
    private fun asObject(value: Any?): Map<String, Any?>? = value as? Map<String, Any?>

    private fun Map<String, Any?>.string(key: String): String =
        this[key] as? String ?: malformed("$key must be a string")
}
