package de.bubblebuffer.bubbleplay.keys

/**
 * A parsed `.bpkey` file. It holds only public values and the seed in sealed form; the seed of an
 * unprotected file is the plain 32-byte seed. Build one with [KeyFileCodec] only.
 * Constructor arguments are copied, and every array property returns a fresh copy.
 */
class KeyFile internal constructor(
    val id: String,
    val label: String,
    val created: String,
    publicKey: ByteArray,
    salt: ByteArray?,
    nonce: ByteArray?,
    sealedSeed: ByteArray,
    /** The format name the file was read in. [KeyFileCodec.authenticatedData] builds the AAD from it. */
    val format: String,
) {
    private val publicBytes = publicKey.copyOf()
    private val saltBytes = salt?.copyOf()
    private val nonceBytes = nonce?.copyOf()
    private val sealed = sealedSeed.copyOf()

    /** Raw 32-byte Ed25519 public key. */
    val publicKey: ByteArray get() = publicBytes.copyOf()

    /** Argon2id salt; null for an unprotected file. */
    val salt: ByteArray? get() = saltBytes?.copyOf()

    /** AES-GCM nonce; null for an unprotected file. */
    val nonce: ByteArray? get() = nonceBytes?.copyOf()

    /** Plain 32-byte seed, or AES-GCM ciphertext followed by its 16-byte tag. */
    val sealedSeed: ByteArray get() = sealed.copyOf()

    val isProtected: Boolean get() = saltBytes != null

    /** True for a file in the old format name. [KeyFileCodec.serialize] never writes it. */
    val isLegacyFormat: Boolean get() = format == KeyFileCodec.LEGACY_FORMAT
}
