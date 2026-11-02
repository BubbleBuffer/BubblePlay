package de.bubblebuffer.bubbleplay.flutter

import de.bubblebuffer.bubbleplay.log.BpLog
import kotlinx.coroutines.suspendCancellableCoroutine
import java.util.concurrent.Executor
import kotlin.coroutines.resume
import kotlin.coroutines.resumeWithException

/**
 * Runs app actions on [executor] (key work must stay off the main thread) and gives the caller
 * the outcome: [act] returns when the backend has run the action, and throws a [FlutterError]
 * when it failed, so Dart sees the failure instead of an immediate success.
 */
internal class ActRunner(private val backend: AppBackend, private val executor: Executor) {
    suspend fun act(actionJson: String) {
        suspendCancellableCoroutine { done ->
            executor.execute {
                try {
                    backend.act(actionJson)
                    done.resume(Unit)
                } catch (e: Exception) {
                    // Class and top frame only: a JSON error message can quote the action,
                    // which may carry a passphrase.
                    BpLog.e(
                        "ActRunner", "an app action failed", null,
                        mapOf("exception" to e.javaClass.name, "at" to e.stackTrace.firstOrNull()?.toString()),
                    )
                    done.resumeWithException(FlutterError(FAILED, "The action failed (${e.javaClass.simpleName}).", null))
                }
            }
        }
    }

    companion object {
        const val FAILED = "action_failed"
    }
}
