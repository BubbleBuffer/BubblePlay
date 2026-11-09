package de.bubblebuffer.bubbleplay.flutter

import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.Bundle
import de.bubblebuffer.bubbleplay.keys.DeviceKeyStore
import de.bubblebuffer.bubbleplay.log.BpLog
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

    override fun onCreate(savedInstanceState: Bundle?) {
        // First thing: the log and the crash handler must exist before anything else can fail.
        BpLog.init(applicationContext)
        super.onCreate(savedInstanceState)
    }

    override fun configureFlutterEngine(flutterEngine: FlutterEngine) {
        super.configureFlutterEngine(flutterEngine)
        val context = applicationContext
        val messenger = flutterEngine.dartExecutor.binaryMessenger
        // The log upload's ctl bridge: events go to Dart on the main thread, calls come from it.
        val ctlFlutterApi = CtlFlutterApi(messenger)
        val ctlShell = CtlShell { kind, json ->
            CoroutineScope(Dispatchers.Main).launch {
                try {
                    ctlFlutterApi.ctlEvent(kind, json)
                } catch (_: Throwable) {
                }
            }
        }
        val controller = StreamController(flutterEngine.renderer, onCtl = ctlShell::attach)
        stream = controller
        val backend = try {
            AppBackend(
                keys = StoreKeyStorage(DeviceKeyStore(context)),
                pcs = PcStore.from(context),
                pairing = ClientPairingRunner,
                deviceLabel = Build.MODEL?.takeIf { it.isNotBlank() } ?: "Android viewer",
                keyPathText = "App storage, protected by the Android Keystore",
                stream = controller,
            )
        } catch (e: Throwable) {
            // Not swallowed: the engine setup fails as before, but the cause is now in the log.
            BpLog.e("MainActivity", "the app backend could not start", null, emptyMap(), e)
            BpLog.current()?.flush()
            throw e
        }
        this.backend = backend
        val runner = ActRunner(backend, worker)
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

                override suspend fun act(actionJson: String) = runner.act(actionJson)

                override fun exportKey(): String = backend.exportKey()
            },
        )
        CtlHostApi.setUp(
            messenger,
            object : CtlHostApi {
                override suspend fun ctlSend(json: String): Boolean = ctlShell.send(json)

                override fun ctlState(): CtlState = ctlShell.state()
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

    // No foreground service. In the background the session closes media at once, keeps signaling for
    // 5 minutes (BackgroundGrace) and then disconnects; the stats timer stops with it.
    override fun onStart() {
        super.onStart()
        stream?.setForeground(true)
        if (backend?.hasSession == true) {
            main.removeCallbacks(statsTick)
            main.postDelayed(statsTick, STATS_MS)
        }
    }

    override fun onStop() {
        main.removeCallbacks(statsTick)
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
