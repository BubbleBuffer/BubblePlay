package de.bubblebuffer.bubbleplay.keys

/**
 * An unlocked device key. The 32-byte Ed25519 seed stays in memory until [close]; [id] and
 * [publicKey] are public and remain readable after close, while anything that needs the seed
 * ([sign], and [KeyFileCodec] rewrap) refuses with CLOSED.
 */
class DeviceKey private constructor(
    private val seed: SeedBox,
    val id: String,
    publicKey: ByteArray,
) : AutoCloseable {
    private val publicBytes = publicKey.copyOf()

    /** A copy; changing the returned array does not affect the key. */
    val publicKey: ByteArray get() = publicBytes.copyOf()

    val isClosed: Boolean get() = seed.isClosed

    /** Runs [block] with the seed under the holder's lock. The array must not leave [block]. */
    internal fun <T> withSeed(block: (ByteArray) -> T): T = seed.read(block)

    /** Ed25519 signature over [message] (raw bytes, no context prefix added here). */
    fun sign(message: ByteArray): ByteArray = seed.read { bytes -> Ed25519.sign(bytes, message) }

    override fun close() = seed.close()

    companion object {
        /** Keeps its own copy of [seed]; the caller still owns the array and should zero it. */
        internal fun fromSeed(seed: ByteArray): DeviceKey {
            val publicKey = Ed25519.publicKey(seed)
            return DeviceKey(SeedBox(seed.copyOf()), DeviceId.fromPublicKey(publicKey), publicKey)
        }
    }
}

/**
 * Owns a seed buffer: zeroes it on [close] and refuses reads afterwards. Reads and close are
 * serialised by the box lock, so a signing call never sees a half-wiped seed.
 */
internal class SeedBox(private val bytes: ByteArray) : AutoCloseable {
    @Volatile
    var isClosed = false
        private set

    @Synchronized
    fun <T> read(block: (ByteArray) -> T): T {
        if (isClosed) refuse(DeviceKeyException.Reason.CLOSED, "device key is closed")
        return block(bytes)
    }

    @Synchronized
    override fun close() {
        bytes.fill(0)
        isClosed = true
    }
}

/**
 * Ed25519 through BouncyCastle's lightweight API. Key and signer objects are local to each call
 * and are never stored; no provider is registered.
 */
private object Ed25519 {
    fun publicKey(seed: ByteArray): ByteArray {
        require(seed.size == 32) { "Ed25519 seeds are 32 bytes" }
        return org.bouncycastle.crypto.params.Ed25519PrivateKeyParameters(seed, 0).generatePublicKey().encoded
    }

    fun sign(seed: ByteArray, message: ByteArray): ByteArray {
        require(seed.size == 32) { "Ed25519 seeds are 32 bytes" }
        val key = org.bouncycastle.crypto.params.Ed25519PrivateKeyParameters(seed, 0)
        val signer = org.bouncycastle.crypto.signers.Ed25519Signer()
        signer.init(true, key)
        signer.update(message, 0, message.size)
        return signer.generateSignature()
    }
}
