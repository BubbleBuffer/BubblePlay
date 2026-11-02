package de.bubblebuffer.bubbleplay.log

import org.json.JSONObject
import java.util.PriorityQueue
import java.util.concurrent.atomic.AtomicLong

/** Runs the uploader's tasks on the test thread, against a fake clock. */
internal class ManualScheduler(private val now: AtomicLong) : UploadScheduler {
    private class Task(val at: Long, val seq: Long, val fn: () -> Unit)

    private val tasks = PriorityQueue<Task>(compareBy<Task>({ it.at }, { it.seq }))
    private var seq = 0L
    @Volatile var shutDown = false

    override fun post(delayMs: Long, task: () -> Unit) {
        synchronized(this) {
            if (!shutDown) tasks.add(Task(now.get() + delayMs.coerceAtLeast(0), seq++, task))
        }
    }

    override fun shutdown() {
        synchronized(this) {
            shutDown = true
            tasks.clear()
        }
    }

    private fun nextDue(limit: Long): Task? = synchronized(this) {
        tasks.peek()?.takeIf { it.at <= limit }?.also { tasks.poll() }
    }

    /** Runs tasks that are due now. */
    fun runDue() {
        while (true) (nextDue(now.get()) ?: break).fn()
    }

    /** Moves the clock forward, running tasks at their own time. */
    fun advance(ms: Long) {
        val target = now.get() + ms
        while (true) {
            val task = nextDue(target) ?: break
            if (task.at > now.get()) now.set(task.at)
            task.fn()
        }
        now.set(target)
        runDue()
    }
}

internal class FakeTransport : LogUploadTransport {
    @Volatile var current: ((JSONObject) -> Unit)? = null
    val sent = ArrayList<JSONObject>()
    var accept = true
    var throwOnSend = false
    @Volatile override var bufferedBytes: Long = 0
    @Volatile override var sinceLastInputMs: Long = Long.MAX_VALUE

    override fun setListener(listener: ((JSONObject) -> Unit)?) {
        current = listener
    }

    override fun send(message: JSONObject): Boolean {
        if (throwOnSend) throw IllegalStateException("boom")
        if (!accept) return false
        synchronized(this) { sent.add(JSONObject(message.toString())) }
        return true
    }

    fun sentCount(): Int = synchronized(this) { sent.size }

    fun deliver(text: String) {
        current?.invoke(JSONObject(text))
    }

    fun deliver(message: JSONObject) {
        current?.invoke(JSONObject(message.toString()))
    }
}
