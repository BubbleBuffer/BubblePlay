package de.bubblebuffer.bubbleplay.flutter

import android.os.Build
import android.os.Handler
import android.os.Looper
import android.util.Log
import de.bubblebuffer.bubbleplay.keys.DeviceKeyStore
import de.bubblebuffer.bubbleplay.pcs.PcStore
import io.flutter.embedding.android.FlutterActivity
import io.flutter.embedding.engine.FlutterEngine
import io.flutter.plugin.common.MethodChannel
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import java.util.concurrent.Executors

class MainActivity : FlutterActivity() {
    // Key work (Argon2id, Keystore) must not run on the main thread: actions go to one worker.
    private val worker = Executors.newSingleThreadExecutor { task -> Thread(task, "bp-app-backend") }
    private val main = Handler(Looper.getMainLooper())
    private var backend: AppBackend? = null
    private var stream: StreamController? = null

    // While a session is open the snapshot's live numbers (fps, RTT, ...) are refreshed once a second.
    private val statsTick = object : Runnable {
        override fun run() {
            val current = backend ?: return
            current.refreshStats()
            if (current.hasSession) main.postDelayed(this, STATS_MS)
        }
    }

    override fun configureFlutterEngine(flutterEngine: FlutterEngine) {
        super.configureFlutterEngine(flutterEngine)
        val context = applicationContext
        val controller = StreamController(flutterEngine.renderer)
        stream = controller
        val backend = AppBackend(
            keys = StoreKeyStorage(DeviceKeyStore(context)),
            pcs = PcStore.from(context),
            pairing = ClientPairingRunner,
            deviceLabel = Build.MODEL?.takeIf { it.isNotBlank() } ?: "Android viewer",
            keyPathText = "App storage, protected by the Android Keystore",
            stream = controller,
        )
        this.backend = backend
        val messenger = flutterEngine.dartExecutor.binaryMessenger
        val flutterApi = AppFlutterApi(messenger)
        backend.setNotifier {
            CoroutineScope(Dispatchers.Main).launch {
                flutterApi.changed()
                if (backend.hasSession) {
                    main.removeCallbacks(statsTick)
                    main.postDelayed(statsTick, STATS_MS)
                }
            }
        }
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
        // The video seam (app/lib/src/session/stream_channel.dart): the texture id of the session
        // video, and touch and key input from Dart.
        MethodChannel(messenger, "bubbleplay/stream").setMethodCallHandler { call, result ->
            val args = (call.arguments as? Map<*, *>)?.entries?.associate { it.key.toString() to it.value }
            try {
                val value = controller.handle(call.method, args)
                if (value === StreamChannelRouter.NOT_IMPLEMENTED) result.notImplemented() else result.success(value)
            } catch (e: Exception) {
                result.error("stream", e.javaClass.simpleName, null)
            }
        }
    }

    // No foreground service in this milestone: in the background Android may stop the network, so
    // the session can end with it. The decoder and the sound follow the foreground state.
    override fun onStart() {
        super.onStart()
        stream?.setForeground(true)
    }

    override fun onStop() {
        stream?.setForeground(false)
        super.onStop()
    }

    override fun onDestroy() {
        main.removeCallbacks(statsTick)
        if (isFinishing) stream?.close()
        super.onDestroy()
    }

    private companion object {
        const val STATS_MS = 1000L
    }
}
