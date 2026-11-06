package de.bubblebuffer.bubbleplay.net

import okhttp3.OkHttpClient
import java.util.concurrent.TimeUnit

/** Longest the signaling open plus WebSocket upgrade may take. The Windows client bounds its handshake at the same 10 s. */
internal const val SIGNALING_HANDSHAKE_MS = 10_000L

/**
 * A client for the signaling socket: the whole open (DNS, connect, TLS, upgrade) is cut off after
 * [handshakeMs] and fails like any other connection error, so the normal reconnect handles it.
 * OkHttp's call timeout stops applying once the upgrade succeeded (`timeoutEarlyExit`), and the
 * upgraded socket's read timeout is cleared, so an established quiet socket is never cut.
 * Shares the connection pool and dispatcher with the receiver.
 */
internal fun OkHttpClient.forSignaling(handshakeMs: Long = SIGNALING_HANDSHAKE_MS): OkHttpClient =
    newBuilder().callTimeout(handshakeMs, TimeUnit.MILLISECONDS).build()
