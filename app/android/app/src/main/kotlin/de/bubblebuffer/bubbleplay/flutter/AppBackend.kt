package de.bubblebuffer.bubbleplay.flutter

import de.bubblebuffer.bubbleplay.auth.PairLink
import de.bubblebuffer.bubbleplay.auth.PairingCode
import de.bubblebuffer.bubbleplay.keys.DeviceId
import de.bubblebuffer.bubbleplay.keys.DeviceKey
import de.bubblebuffer.bubbleplay.keys.DeviceKeyException
import de.bubblebuffer.bubbleplay.keys.DeviceKeyStore
import de.bubblebuffer.bubbleplay.keys.KeyFile
import de.bubblebuffer.bubbleplay.keys.KeyFileCodec
import de.bubblebuffer.bubbleplay.keys.UnlockedKey
import de.bubblebuffer.bubbleplay.net.PairingClient
import de.bubblebuffer.bubbleplay.net.PairingFailure
import de.bubblebuffer.bubbleplay.net.PairingListener
import de.bubblebuffer.bubbleplay.net.PairingStage
import de.bubblebuffer.bubbleplay.pcs.PcEntry
import de.bubblebuffer.bubbleplay.pcs.PcStore
import org.json.JSONObject
import java.net.URI
import java.time.Instant
import java.time.ZoneId
import java.time.format.DateTimeFormatter
import java.util.Locale

/** Where the portable key text lives. Production is [DeviceKeyStore] (Keystore-wrapped). */
internal interface KeyStorage {
    /** Null only when there is no key. */
    fun load(): String?

    /** Writes the key; the caller has already checked that none exists. */
    fun save(text: String)
}

internal class StoreKeyStorage(private val store: DeviceKeyStore) : KeyStorage {
    override fun load(): String? = store.loadPortable()
    override fun save(text: String) = store.savePortable(text)
}

/** Starts one pairing at a PC. Production is [PairingClient]; the returned function cancels it. */
internal fun interface PairingRunner {
    fun start(key: DeviceKey, link: PairLink, name: String, listener: PairingListener): () -> Unit
}

internal object ClientPairingRunner : PairingRunner {
    override fun start(key: DeviceKey, link: PairLink, name: String, listener: PairingListener): () -> Unit {
        val client = PairingClient(key, link, name, listener)
        client.start()
        return client::cancel
    }
}

/**
 * The Android side of the app contract: the same snapshot / act / notify shape as the Windows
 * Rust backend (`app/rust/src/backend.rs`), over the Kotlin core (keys, auth, pcs, pairing).
 * Both ends speak the JSON of `AppState` and `Action`; see `StateJson`.
 *
 * Not thread-safe by itself: [act] and the pairing callbacks must run on one thread (the main
 * thread in the app, whatever the test chooses). [snapshot] may be read from any thread.
 *
 * M1 stops at the session boundary, like Windows: Connect opens a placeholder session.
 */
internal class AppBackend(
    private val keys: KeyStorage,
    private val pcs: PcStore,
    private val pairing: PairingRunner,
    private val deviceLabel: String,
    private val keyPathText: String,
    private val clock: () -> Long = System::currentTimeMillis,
    private val zone: ZoneId = ZoneId.systemDefault(),
    private val unlocked: UnlockedHolder = UnlockedHolder.Global,
) {
    /** Where the unlocked key is kept. Production is [UnlockedKey]. */
    interface UnlockedHolder {
        fun get(): DeviceKey?
        fun set(key: DeviceKey)

        object Global : UnlockedHolder {
            override fun get(): DeviceKey? = UnlockedKey.get()
            override fun set(key: DeviceKey) = UnlockedKey.set(key)
        }
    }

    private var notify: () -> Unit = {}
    private var deviceId = ""
    private var flow: JSONObject? = null
    private var session: JSONObject? = null
    private var stored: KeyFile? = null // a protected stored key waiting for its passphrase
    private var importPending: KeyFile? = null // an imported protected file waiting for its passphrase
    private var importName = ""
    private var cancelPairing: (() -> Unit)? = null
    private var pairGeneration = 0
    private var pairStarted = 0L
    private var pairNumber = 0
    private var pairPc: Pair<String, String>? = null // pcId to relay of the pairing in progress

    init {
        loadKey()
    }

    fun setNotifier(callback: () -> Unit) {
        notify = callback
    }

    /** The current `AppState` as JSON. */
    @Synchronized
    fun snapshot(): String {
        val entries = pcs.list().map { pc ->
            StateJson.pc(pc.id, displayName(pc), pc.relay, lastSeenText(pc.lastSeen))
        }
        val shown = flow?.let { current ->
            current.optJSONObject("PairConfirm")?.takeIf { pairStarted > 0 }?.let { confirm ->
                // The wait is counted at read time, as on Windows.
                val secs = ((clock() - pairStarted) / 1000).toInt().coerceAtLeast(0)
                JSONObject(current.toString()).also {
                    it.getJSONObject("PairConfirm").put("waiting_secs", secs)
                }
            } ?: current
        }
        return StateJson.state(deviceId, keyPathText, entries, shown, session).toString()
    }

    /** The portable key text for the user to save. Never part of a snapshot. */
    @Synchronized
    fun exportKey(): String = keys.load() ?: throw IllegalStateException("no key to export")

    @Synchronized
    fun act(actionJson: String) {
        val action = parse(actionJson)
        apply(action.first, action.second)
        notify()
    }

    // ------------------------------------------------------------------ actions

    private fun parse(text: String): Pair<String, JSONObject> {
        val trimmed = text.trim()
        if (trimmed.startsWith("\"")) {
            return org.json.JSONTokener(trimmed).nextValue().toString() to JSONObject()
        }
        val json = JSONObject(trimmed)
        val name = json.keys().asSequence().singleOrNull() ?: throw IllegalArgumentException("not an Action")
        val payload = json.get(name)
        return name to (payload as? JSONObject ?: JSONObject().put("value", payload))
    }

    private fun apply(name: String, args: JSONObject) {
        when (name) {
            "CreateKey" -> createKey()
            "ImportKeyText" -> importKey(args.getString("file_name"), args.getString("text"))
            "UnlockImport" -> unlockImport(args.getString("passphrase"))
            "CancelFlow" -> cancelFlow()
            "StartPairing" -> flow = StateJson.pairEnter(remote = false, error = null)
            "StartRemotePairing" -> flow = StateJson.pairEnter(remote = true, error = null)
            "RequestRemotePairing" ->
                flow = StateJson.pairEnter(true, "Remote pairing is not available in this version.")
            "SubmitPairing" -> submitPairing(
                link = args.optString("link").takeIf { !args.isNull("link") },
                pcId = args.optString("pc_id"),
                code = args.optString("code"),
                relay = args.optString("relay"),
            )
            "Forget" -> pcs.remove(args.getString("pc_id"))
            "Rename" -> rename(args.getString("pc_id"), args.getString("name"))
            "Connect" -> connect(args.getString("pc_id"))
            "Disconnect" -> session = null
            "SetMode" -> editSession { it.put("mode", args.getString("value")) }
            "SetQuality" -> editSession { it.put("quality", args.getString("value")) }
            "SetAudio" -> editSession { it.put("audio", args.getBoolean("value")) }
            "SetMonitor" -> editSession { s ->
                val index = args.getInt("value")
                if (index in 0 until s.getJSONArray("monitors").length()) s.put("monitor", index)
            }
            // Not backed in M1, as on Windows: retries need a real connect, vouching and key
            // replacement are not in the live clients, the This PC actions are Windows host
            // only, and the host app picks key files and calls ImportKeyText / exportKey.
            else -> Unit
        }
    }

    private fun editSession(edit: (JSONObject) -> Unit) {
        session?.let(edit)
    }

    private fun refused(fileName: String, reason: String) = StateJson.importRefused(fileName, reason)

    private fun loadKey() {
        stored = null
        val text = try {
            keys.load()
        } catch (e: Exception) {
            flow = refused(KEY_FILE_NAME, "The stored key cannot be read.")
            return
        }
        if (text == null) {
            deviceId = ""
            flow = StateJson.choose
            return
        }
        val file = try {
            KeyFileCodec.parse(text)
        } catch (e: DeviceKeyException) {
            flow = refused(KEY_FILE_NAME, "The stored key cannot be read.")
            return
        }
        deviceId = file.id
        if (file.isProtected) {
            stored = file
            flow = StateJson.importUnlock(KEY_FILE_NAME, null)
            return
        }
        try {
            unlocked.set(KeyFileCodec.unlock(file, null))
            if (flow?.has("Enroll") == true) flow = null
        } catch (e: DeviceKeyException) {
            flow = refused(KEY_FILE_NAME, "The stored key cannot be opened.")
        }
    }

    private fun createKey() {
        if (keys.load() != null) return
        try {
            val fresh = KeyFileCodec.create(deviceLabel, null)
            keys.save(KeyFileCodec.serialize(fresh.file))
            unlocked.set(fresh.key)
            deviceId = fresh.file.id
            stored = null
            flow = StateJson.created
        } catch (e: Exception) {
            flow = refused("", "The key could not be created.")
        }
    }

    private fun importKey(fileName: String, text: String) {
        if (keys.load() != null) return
        val file = try {
            KeyFileCodec.parse(text)
        } catch (e: DeviceKeyException) {
            val reason = if (e.reason == DeviceKeyException.Reason.UNSUPPORTED_FORMAT) {
                "This is not a BubblePlay key file."
            } else {
                "This key file cannot be used."
            }
            flow = refused(fileName, reason)
            return
        }
        if (file.isProtected) {
            importPending = file
            importName = fileName
            flow = StateJson.importUnlock(fileName, null)
            return
        }
        storeImported(file, null, fileName)
    }

    /** Stores an imported key without its passphrase (the Keystore keeps it), after unlocking it once. */
    private fun storeImported(file: KeyFile, passphrase: String?, fileName: String) {
        try {
            KeyFileCodec.unlock(file, passphrase).use { key ->
                val plain = KeyFileCodec.rewrap(file, key, null)
                keys.save(KeyFileCodec.serialize(plain))
            }
        } catch (e: DeviceKeyException) {
            flow = refused(fileName, "This key file cannot be used.")
            return
        } catch (e: Exception) {
            flow = refused(fileName, "The key could not be stored.")
            return
        }
        loadKey() // unlocks the stored key and clears the enroll flow
    }

    private fun unlockImport(passphrase: String) {
        importPending?.let { file ->
            val name = importName
            try {
                KeyFileCodec.unlock(file, passphrase).close()
            } catch (e: DeviceKeyException) {
                flow = if (e.reason == DeviceKeyException.Reason.AUTHENTICATION_FAILED) {
                    StateJson.importUnlock(name, "That passphrase did not open the key.")
                } else {
                    importPending = null
                    refused(name, "This key file cannot be used.")
                }
                return
            }
            importPending = null
            storeImported(file, passphrase, name)
            return
        }
        stored?.let { file ->
            // The stored key is protected: unlock it for this run; the file stays as it is.
            try {
                unlocked.set(KeyFileCodec.unlock(file, passphrase))
                stored = null
                flow = null
            } catch (e: DeviceKeyException) {
                val message = if (e.reason == DeviceKeyException.Reason.AUTHENTICATION_FAILED) {
                    "That passphrase did not open the key."
                } else {
                    "The key cannot be opened."
                }
                flow = StateJson.importUnlock(KEY_FILE_NAME, message)
            }
        }
    }

    private fun cancelFlow() {
        val current = flow
        when {
            current == null -> Unit
            current.optString("Enroll") == "Choose" -> Unit
            current.optJSONObject("Enroll")?.has("ImportUnlock") == true && stored != null -> Unit
            current.optString("Enroll") == "Created" -> flow = null
            current.has("Enroll") -> {
                importPending = null
                flow = StateJson.choose
            }
            current.has("PairConfirm") || current.has("Approval") -> {
                pairGeneration++
                cancelPairing?.invoke()
                cancelPairing = null
                flow = null
            }
            else -> flow = null
        }
    }

    private fun rename(pcId: String, name: String) {
        val trimmed = name.trim()
        if (trimmed.isNotEmpty()) pcs.rename(pcId, trimmed)
    }

    private fun connect(pcId: String) {
        val pc = pcs.get(pcId) ?: return
        session = StateJson.session(pc.id, displayName(pc), "Work", "Auto", false)
        flow = null
    }

    private fun displayName(pc: PcEntry): String = pc.name.ifEmpty { "PC " + pc.id.take(4) }

    // ------------------------------------------------------------------ pairing

    private fun submitPairing(link: String?, pcId: String, code: String, relay: String) {
        fun fail(message: String) {
            flow = StateJson.pairEnter(false, message)
        }

        var pc = pcId
        var pairCode = code
        var relayText = relay
        if (!link.isNullOrBlank()) {
            val parsed = parseLink(link) ?: return fail("This is not a BubblePlay pairing link.")
            if (parsed.pcId.isEmpty() || parsed.code.isEmpty()) {
                return fail("The pairing link has no PC ID or no code.")
            }
            pc = parsed.pcId
            pairCode = parsed.code
            if (parsed.relay.isNotEmpty()) relayText = parsed.relay
        }
        val key = unlocked.get() ?: return fail("This device has no unlocked key yet.")
        val id = try {
            DeviceId.parse(pc.filterNot { it.isWhitespace() })
        } catch (e: DeviceKeyException) {
            return fail("The PC ID is not valid. It has 16 characters, like 77VH-7M56-8GJK-Y12J.")
        }
        val normalised = PairingCode.normalise(pairCode.trim())
            ?: return fail("A pairing code has 10 characters from 0-9 and A-Z (XXXXX-XXXXX).")
        if (relayText.isBlank()) return fail("Enter the relay address shown on the PC.")
        val withScheme = if (relayText.contains("://")) relayText.trim() else "https://${relayText.trim()}"
        val origin = relayOrigin(withScheme)
            ?: return fail("The relay address is not valid. Use an https address, like https://relay.example.")
        if (pcs.get(id) != null) return fail("This PC is already in Your PCs.")

        pairGeneration++
        val generation = pairGeneration
        pairStarted = clock()
        pairNumber = 0
        pairPc = id to origin
        flow = pairConfirm(id, Outcome.WAITING)
        val listener = object : PairingListener {
            override fun onStage(stage: PairingStage) = Unit

            override fun onNumber(display: String) = ifCurrent(generation) {
                pairNumber = display.filter(Char::isDigit).toIntOrNull() ?: 0
                flow = pairConfirm(id, Outcome.WAITING)
            }

            override fun onPaired(pc: PcEntry) = ifCurrent(generation) {
                pcs.put(pc)
                pairStarted = 0
                cancelPairing = null
                flow = pairConfirm(id, Outcome.APPROVED)
            }

            override fun onDenied() = ifCurrent(generation) {
                pairStarted = 0
                cancelPairing = null
                flow = pairConfirm(id, Outcome.DENIED)
            }

            override fun onFailed(reason: PairingFailure) = ifCurrent(generation) {
                pairStarted = 0
                cancelPairing = null
                flow = when {
                    reason == PairingFailure.Timeout && pairNumber != 0 -> pairConfirm(id, Outcome.TIMED_OUT)
                    reason == PairingFailure.ConnectionLost && pairNumber != 0 -> pairConfirm(id, Outcome.CANCELLED)
                    else -> StateJson.pairEnter(false, failureText(reason))
                }
            }
        }
        cancelPairing = pairing.start(key, PairLink(origin, id, normalised), id, listener)
    }

    @Synchronized
    private fun ifCurrent(generation: Int, block: () -> Unit) {
        if (generation != pairGeneration) return
        block()
        notify()
    }

    private enum class Outcome { WAITING, APPROVED, DENIED, TIMED_OUT, CANCELLED }

    private fun pairConfirm(pcId: String, outcome: Outcome): JSONObject {
        val value: Any = when (outcome) {
            Outcome.WAITING -> "Waiting"
            Outcome.APPROVED -> StateJson.approvedByPc()
            Outcome.DENIED -> StateJson.deniedByPc()
            Outcome.TIMED_OUT -> "TimedOut"
            Outcome.CANCELLED -> "Cancelled"
        }
        return StateJson.pairConfirm(pcId, pcId, pairNumber, 0, false, value)
    }

    private fun failureText(reason: PairingFailure): String = when (reason) {
        PairingFailure.Refused ->
            "The relay did not accept the code. Check the PC ID and the code, and that the pairing window on the PC is open."
        PairingFailure.ProofMismatch -> "The PC did not prove it knows the code. Check the code and try again."
        PairingFailure.PcKeyInvalid -> "The PC's ID or key is not valid. Check the PC ID."
        PairingFailure.ConnectionLost, PairingFailure.Timeout ->
            "The relay could not be reached. Check the relay address and the network."
    }

    private fun parseLink(text: String): ParsedLink? {
        PairLink.parse(text.trim())?.let { return ParsedLink(it.relay, it.pcId, it.code) }
        // The browser's form: https://<relay>/pair#pc=<id>&code=<code>
        val uri = try {
            URI(text.trim())
        } catch (e: Exception) {
            return null
        }
        if (uri.scheme?.lowercase() !in setOf("https", "http") || uri.rawPath != "/pair") return null
        val params = (uri.rawFragment ?: "").split('&').mapNotNull {
            val parts = it.split('=', limit = 2)
            if (parts.size == 2) parts[0] to java.net.URLDecoder.decode(parts[1], "UTF-8") else null
        }.toMap()
        val origin = relayOrigin("${uri.scheme}://${uri.rawAuthority}") ?: return null
        return ParsedLink(origin, params["pc"].orEmpty(), params["code"].orEmpty())
    }

    private class ParsedLink(val relay: String, val pcId: String, val code: String)

    /** The origin of a typed relay address (http only for a loopback host), or null. */
    private fun relayOrigin(text: String): String? {
        val uri = try {
            URI(text.trim())
        } catch (e: Exception) {
            return null
        }
        val scheme = uri.scheme?.lowercase() ?: return null
        val host = uri.host?.lowercase() ?: return null
        if (uri.rawUserInfo != null) return null
        if (scheme == "http" && host != "localhost" && host != "127.0.0.1" && host != "[::1]") return null
        if (scheme != "http" && scheme != "https") return null
        return "$scheme://$host" + if (uri.port == -1) "" else ":${uri.port}"
    }

    // ------------------------------------------------------------------ display text

    /** "Today 09:12" or "2 Oct", the display text of the model. Null when never connected. */
    private fun lastSeenText(millis: Long): String? {
        if (millis <= 0) return null
        val moment = Instant.ofEpochMilli(millis).atZone(zone)
        val today = Instant.ofEpochMilli(clock()).atZone(zone).toLocalDate()
        return if (moment.toLocalDate() == today) {
            "Today " + moment.format(DateTimeFormatter.ofPattern("HH:mm", Locale.ENGLISH))
        } else {
            moment.format(DateTimeFormatter.ofPattern("d MMM", Locale.ENGLISH))
        }
    }

    private companion object {
        const val KEY_FILE_NAME = "device-key.bpkey"
    }
}
