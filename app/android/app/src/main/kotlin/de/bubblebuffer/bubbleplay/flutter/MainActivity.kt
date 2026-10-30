package de.bubblebuffer.bubbleplay.flutter

import android.os.Build
import android.util.Log
import de.bubblebuffer.bubbleplay.keys.DeviceKeyStore
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

    override fun configureFlutterEngine(flutterEngine: FlutterEngine) {
        super.configureFlutterEngine(flutterEngine)
        val context = applicationContext
        val backend = AppBackend(
            keys = StoreKeyStorage(DeviceKeyStore(context)),
            pcs = PcStore.from(context),
            pairing = ClientPairingRunner,
            deviceLabel = Build.MODEL?.takeIf { it.isNotBlank() } ?: "Android viewer",
            keyPathText = "App storage, protected by the Android Keystore",
        )
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
                            Log.w("BubblePlay", "action failed: ${e.javaClass.simpleName}")
                        }
                    }
                }

                override fun exportKey(): String = backend.exportKey()
            },
        )
    }
}
