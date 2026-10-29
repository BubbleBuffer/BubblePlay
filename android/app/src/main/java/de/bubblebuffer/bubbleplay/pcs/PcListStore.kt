package de.bubblebuffer.bubbleplay.pcs

import android.content.Context

/**
 * [PcStore] over one text value, the JSON from [PcListCodec]. [Backing] is the app's SharedPreferences
 * in the app and a map in the JVM tests. Every change rewrites the whole list.
 */
class PcListStore(private val backing: Backing) : PcStore {
    interface Backing {
        fun read(): String?
        fun write(text: String)
    }

    @Synchronized
    override fun list(): List<PcEntry> = PcListCodec.decode(backing.read())

    @Synchronized
    override fun get(id: String): PcEntry? = list().firstOrNull { it.id == id }

    @Synchronized
    override fun put(entry: PcEntry) {
        val current = list()
        val next = if (current.any { it.id == entry.id }) {
            current.map { if (it.id == entry.id) entry else it }
        } else {
            current + entry
        }
        save(next)
    }

    @Synchronized
    override fun rename(id: String, name: String) {
        save(list().map { if (it.id == id) it.copy(name = name) else it })
    }

    @Synchronized
    override fun touch(id: String, lastSeen: Long, problem: String?) {
        save(list().map { if (it.id == id) it.copy(lastSeen = lastSeen, problem = problem) else it })
    }

    @Synchronized
    override fun remove(id: String) {
        save(list().filterNot { it.id == id })
    }

    private fun save(entries: List<PcEntry>) = backing.write(PcListCodec.encode(entries))

    companion object {
        private const val PREFS = "pcs"
        private const val KEY_LIST = "list"

        /** The app's store: the list lives in the app's own SharedPreferences. */
        fun preferences(context: Context): PcListStore {
            val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            return PcListStore(object : Backing {
                override fun read(): String? = prefs.getString(KEY_LIST, null)
                override fun write(text: String) = prefs.edit().putString(KEY_LIST, text).apply()
            })
        }
    }
}
