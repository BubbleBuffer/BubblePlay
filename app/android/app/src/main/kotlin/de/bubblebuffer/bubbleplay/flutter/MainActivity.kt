package de.bubblebuffer.bubbleplay.flutter

import android.os.Build
import android.os.Bundle
import de.bubblebuffer.bubbleplay.keys.DeviceKeyStore
import de.bubblebuffer.bubbleplay.log.BpLog
import de.bubblebuffer.bubbleplay.pcs.PcStore
import io.flutter.embedding.android.FlutterActivity
import io.flutter.embedding.engine.FlutterEngine
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import java.util.concurrent.Executors

class MainActivity : FlutterActivity() {
    // Key work (Argon2id, Keystore) must not run on the main thread: actions go to one worker.
    private val worker = Executors.newSingleThreadExecutor { task -> Thread(task, "bp-app-backend") }

    override fun onCreate(savedInstanceState: Bundle?) {
        // First thing: the log and the crash handler must exist before anything else can fail.
        BpLog.init(applicationContext)
        super.onCreate(savedInstanceState)
    }

    override fun configureFlutterEngine(flutterEngine: FlutterEngine) {
        super.configureFlutterEngine(flutterEngine)
        val context = applicationContext
        val backend = try {
            AppBackend(
                keys = StoreKeyStorage(DeviceKeyStore(context)),
                pcs = PcStore.from(context),
                pairing = ClientPairingRunner,
                deviceLabel = Build.MODEL?.takeIf { it.isNotBlank() } ?: "Android viewer",
                keyPathText = "App storage, protected by the Android Keystore",
            )
        } catch (e: Throwable) {
            // Not swallowed: the engine setup fails as before, but the cause is now in the log.
            BpLog.e("MainActivity", "the app backend could not start", null, emptyMap(), e)
            BpLog.current()?.flush()
            throw e
        }
        val messenger = flutterEngine.dartExecutor.binaryMessenger
        val flutterApi = AppFlutterApi(messenger)
        backend.setNotifier { CoroutineScope(Dispatchers.Main).launch { flutterApi.changed() } }
        AppHostApi.setUp(
            messenger,
            object : AppHostApi {
                override fun snapshot(): String = backend.snapshot()

                override fun act(actionJson: String) {
                    worker.execute {
                        try {
                            backend.act(actionJson)
                        } catch (e: Exception) {
                            // Class and top frame only: a JSON error message can quote the action,
                            // which may carry a passphrase.
                            BpLog.w(
                                "MainActivity", "an app action failed", null,
                                mapOf("exception" to e.javaClass.name, "at" to e.stackTrace.firstOrNull()?.toString()),
                            )
                        }
                    }
                }

                override fun exportKey(): String = backend.exportKey()
            },
        )
    }
}
