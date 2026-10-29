package de.bubblebuffer.bubbleplay.keys

import org.bouncycastle.crypto.params.Ed25519PublicKeyParameters
import org.bouncycastle.crypto.signers.Ed25519Signer
import org.bouncycastle.math.ec.rfc8032.Ed25519 as Rfc8032

/**
 * Strict Ed25519 verification for peer signatures (handshake, relay proofs).
 *
 * At least as strict as ed25519-dalek's `verify_strict`, which the Rust side uses: the public key
 * and the signature's R must be canonical points of prime order L (no small-order or mixed-order
 * points), S must be canonical (BouncyCastle checks S < L), and the equation is the cofactorless
 * one. Honest keys and signatures always pass; wrong lengths return false, never throw.
 */
object Ed25519Verify {
    fun verify(publicKey: ByteArray, message: ByteArray, signature: ByteArray): Boolean {
        if (publicKey.size != 32 || signature.size != 64) return false
        if (!Rfc8032.validatePublicKeyFull(publicKey, 0)) return false
        // R is the first half of the signature; an honest R = rB has prime order.
        if (!Rfc8032.validatePublicKeyFull(signature, 0)) return false
        return try {
            val signer = Ed25519Signer()
            signer.init(false, Ed25519PublicKeyParameters(publicKey, 0))
            signer.update(message, 0, message.size)
            signer.verifySignature(signature)
        } catch (e: IllegalArgumentException) {
            false
        }
    }
}
