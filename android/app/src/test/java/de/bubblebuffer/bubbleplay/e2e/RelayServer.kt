package de.bubblebuffer.bubbleplay.e2e

import org.junit.Assume.assumeTrue
import org.junit.rules.TestRule
import org.junit.runner.Description
import org.junit.runners.model.Statement
import java.io.File
import java.net.InetSocketAddress
import java.net.ServerSocket
import java.net.Socket
import java.nio.file.Files
import java.util.concurrent.TimeUnit

/**
 * One real bp-relay-server process (relay-rs, `bp-relay serve`) on a free 127.0.0.1 port with a
 * temp state dir and a TOKEN. The binary comes from BP_RELAY_SERVER_BIN. With BP_E2E_REQUIRED=1
 * (CI) a missing binary fails the test; otherwise the e2e tests are skipped.
 *
 * Use it as a JUnit rule: each test gets a fresh relay, killed afterwards. A failing test gets
 * the relay's log appended to its output.
 */
class RelayServer : TestRule {
    lateinit var origin: String
        private set
    var port: Int = 0
        private set
    private var process: Process? = null
    private var dir: File? = null
    private val log: File? get() = dir?.let { File(it, "relay.log") }

    override fun apply(base: Statement, description: Description): Statement = object : Statement() {
        override fun evaluate() {
            val bin = binary()
            start(bin)
            try {
                base.evaluate()
            } catch (t: Throwable) {
                System.err.println("---- bp-relay log for ${description.displayName} ----\n${logText()}\n---- end ----")
                throw t
            } finally {
                stop()
            }
        }
    }

    fun logText(): String = log?.takeIf { it.exists() }?.readText()?.takeLast(20_000) ?: "(no log)"

    private fun start(bin: File) {
        val root = Files.createTempDirectory("bp-relay-e2e").toFile()
        dir = root
        val assets = File(root, "assets").apply { mkdirs() }
        File(assets, "index.html").writeText("e2e")
        val state = File(root, "state").apply { mkdirs() }
        port = freePort()
        origin = "http://127.0.0.1:$port"
        val config = File(root, "relay.json")
        config.writeText(
            """
            {
              "public_origin": "$origin",
              "listen": "127.0.0.1:$port",
              "assets_dir": ${json(assets.absolutePath)},
              "state_dir": ${json(state.absolutePath)},
              "token": "$TOKEN",
              "admin_token": "e2e-admin-token-0123456789-0123456789-abcdef"
            }
            """.trimIndent(),
        )
        val builder = ProcessBuilder(bin.absolutePath, "serve", "--config", config.absolutePath)
            .redirectErrorStream(true)
            .redirectOutput(log!!)
        builder.environment().remove("BP_RELAY_TOKEN")
        builder.environment().remove("BP_RELAY_ADMIN_TOKEN")
        builder.environment()["RUST_LOG"] = "info"
        val started = builder.start()
        process = started
        val deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(20)
        while (true) {
            if (!started.isAlive) error("bp-relay exited with ${started.exitValue()} before listening:\n${logText()}")
            if (listening()) return
            if (System.nanoTime() > deadline) error("bp-relay did not listen on $port within 20 s:\n${logText()}")
            Thread.sleep(50)
        }
    }

    private fun listening(): Boolean = try {
        Socket().use { it.connect(InetSocketAddress("127.0.0.1", port), 200) }
        true
    } catch (_: java.io.IOException) {
        false
    }

    private fun stop() {
        process?.let { p ->
            p.destroy()
            if (!p.waitFor(5, TimeUnit.SECONDS)) {
                p.destroyForcibly()
                p.waitFor(5, TimeUnit.SECONDS)
            }
            check(!p.isAlive) { "bp-relay did not exit" }
        }
        process = null
        dir?.deleteRecursively()
        dir = null
    }

    companion object {
        const val TOKEN = "e2e-first-registration-token-0123456789-abcdef"
        const val ENV_BIN = "BP_RELAY_SERVER_BIN"
        const val ENV_REQUIRED = "BP_E2E_REQUIRED"

        /** The relay binary, or a skipped test (failed when BP_E2E_REQUIRED=1). */
        fun binary(): File {
            val path = System.getenv(ENV_BIN)?.takeIf { it.isNotBlank() }
            val file = path?.let(::File)
            val usable = file != null && file.isFile && file.canExecute()
            if (System.getenv(ENV_REQUIRED) == "1") {
                check(usable) { "$ENV_REQUIRED=1 but $ENV_BIN does not name an executable bp-relay ($path)" }
            } else {
                assumeTrue("$ENV_BIN not set: relay e2e tests skipped", usable)
            }
            return file!!
        }

        private fun freePort(): Int = ServerSocket(0, 1, java.net.InetAddress.getByName("127.0.0.1")).use { it.localPort }

        private fun json(text: String): String =
            "\"" + text.replace("\\", "\\\\").replace("\"", "\\\"") + "\""
    }
}
