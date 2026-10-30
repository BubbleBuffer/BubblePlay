package de.bubblebuffer.bubbleplay.ui

import android.app.Activity
import android.graphics.Bitmap
import android.graphics.Canvas
import android.os.Looper
import android.security.keystore.KeyGenParameterSpec
import android.view.View
import android.view.ViewGroup
import android.widget.TextView
import de.bubblebuffer.bubbleplay.keys.DeviceKey
import de.bubblebuffer.bubbleplay.keys.DeviceKeyStore
import de.bubblebuffer.bubbleplay.keys.KeyFileCodec
import de.bubblebuffer.bubbleplay.keys.UnlockedKey
import de.bubblebuffer.bubbleplay.keys.useKey
import de.bubblebuffer.bubbleplay.pcs.PcEntry
import de.bubblebuffer.bubbleplay.pcs.PcStore
import org.junit.After
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.Robolectric
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode
import java.io.File
import java.io.IOException
import java.io.InputStream
import java.io.OutputStream
import java.security.InvalidParameterException
import java.security.Key
import java.security.KeyStoreException
import java.security.KeyStoreSpi
import java.security.Provider
import java.security.SecureRandom
import java.security.Security
import java.security.cert.Certificate
import java.security.spec.AlgorithmParameterSpec
import java.util.Base64
import java.util.Collections
import java.util.Date
import java.util.Enumeration
import javax.crypto.KeyGeneratorSpi
import javax.crypto.SecretKey
import javax.crypto.spec.SecretKeySpec

/**
 * Renders three real screens with Robolectric and writes them as PNGs to the `bubbleplay.screenshots`
 * folder (CI uploads it as the `android-screenshots` artifact). Each test also checks that the render
 * is not one flat colour and that the screen shows its expected text.
 */
@RunWith(RobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
@Config(sdk = [35], qualifiers = "w393dp-h852dp-xhdpi")
class ReadmeScreenshotTest {
    private val app get() = RuntimeEnvironment.getApplication()

    @Before
    fun setUp() {
        FakeAndroidKeyStore.install()
        UnlockedKey.clear()
    }

    @After
    fun tearDown() {
        UnlockedKey.clear()
        FakeAndroidKeyStore.uninstall()
    }

    @Test
    fun setupScreenWithoutDeviceKey() {
        val controller = Robolectric.buildActivity(MainActivity::class.java).setup()
        try {
            val activity = controller.get()
            awaitText(activity, "This phone needs a device key")
            assertShows(activity, "BubblePlay", "Create key")
            capture(activity, "android-setup.png")
        } finally {
            controller.destroy()
        }
    }

    @Test
    fun pcListWithUnlockedKey() {
        seedUnlockedPhoneKey()
        seedPcs()
        val controller = Robolectric.buildActivity(MainActivity::class.java).setup()
        try {
            val activity = controller.get()
            awaitText(activity, "Studio PC")
            assertShows(activity, "PCs", "Studio PC", "Living room", "Office tower")
            capture(activity, "android-pcs.png")
        } finally {
            controller.destroy()
        }
    }

    @Test
    fun pairScreenFromPcList() {
        seedUnlockedPhoneKey()
        seedPcs()
        val controller = Robolectric.buildActivity(PairActivity::class.java).setup()
        try {
            val activity = controller.get()
            awaitText(activity, RELAY_HOST)
            assertShows(activity, "Pair a PC", "Relay address", RELAY_HOST)
            capture(activity, "android-pair.png")
        } finally {
            controller.destroy()
        }
    }

    // ---- fixtures ---------------------------------------------------------------------------------

    /** Stores a device key the way the app does (wrapped by the Keystore) and unlocks it for the process. */
    private fun seedUnlockedPhoneKey() {
        val created = KeyFileCodec.create("This phone", null)
        DeviceKeyStore(app).savePortable(KeyFileCodec.serialize(created.file))
        UnlockedKey.set(created.key)
    }

    private fun seedPcs() {
        val store = PcStore.from(app)
        val now = System.currentTimeMillis()
        store.put(pcEntry(seed = 0x10, name = "Studio PC", paired = now - 40 * DAY, lastSeen = now - 2 * MINUTE))
        store.put(pcEntry(seed = 0x20, name = "Living room", paired = now - 12 * DAY, lastSeen = now - 3 * HOUR))
        store.put(pcEntry(seed = 0x30, name = "Office tower", paired = now - 3 * DAY, lastSeen = now - DAY))
    }

    /** A PC entry with an ID derived from a fixed public key, as the app derives PC IDs. */
    private fun pcEntry(seed: Int, name: String, paired: Long, lastSeen: Long): PcEntry =
        DeviceKey.fromSeed(ByteArray(32) { (seed + it).toByte() }).useKey { key ->
            PcEntry(
                id = key.id,
                publicKey = Base64.getUrlEncoder().withoutPadding().encodeToString(key.publicKey),
                relay = RELAY,
                name = name,
                paired = paired,
                lastSeen = lastSeen,
            )
        }

    // ---- rendering and checks ---------------------------------------------------------------------

    /** Waits (idling the main looper) until [expected] is on screen, then lets the screen settle. */
    private fun awaitText(activity: Activity, expected: String) {
        val looper = shadowOf(Looper.getMainLooper())
        val deadline = System.currentTimeMillis() + 5_000
        while (texts(activity.window.decorView).none { it.contains(expected) }) {
            if (System.currentTimeMillis() > deadline) {
                fail("\"$expected\" is not on ${activity.javaClass.simpleName} after 5 s")
            }
            looper.idle()
            Thread.sleep(20)
        }
        repeat(15) {
            looper.idle()
            Thread.sleep(20)
        }
    }

    private fun assertShows(activity: Activity, vararg expected: String) {
        val shown = texts(activity.window.decorView)
        for (text in expected) {
            assertTrue("\"$text\" is not on ${activity.javaClass.simpleName}", shown.any { it.contains(text) })
        }
    }

    /** Draws the decor view at the full window size into an ARGB bitmap and writes it as a PNG if asked. */
    private fun capture(activity: Activity, fileName: String) {
        val decor = activity.window.decorView
        val metrics = activity.resources.displayMetrics
        val width = metrics.widthPixels
        val height = metrics.heightPixels
        decor.measure(
            View.MeasureSpec.makeMeasureSpec(width, View.MeasureSpec.EXACTLY),
            View.MeasureSpec.makeMeasureSpec(height, View.MeasureSpec.EXACTLY),
        )
        decor.layout(0, 0, width, height)
        val bitmap = Bitmap.createBitmap(width, height, Bitmap.Config.ARGB_8888)
        decor.draw(Canvas(bitmap))

        val pixels = IntArray(width * height)
        bitmap.getPixels(pixels, 0, width, 0, 0, width, height)
        assertTrue("$fileName is a single colour", pixels.any { it != pixels[0] })

        System.getProperty("bubbleplay.screenshots")?.let { folder ->
            val dir = File(folder).apply { mkdirs() }
            File(dir, fileName).outputStream().use { bitmap.compress(Bitmap.CompressFormat.PNG, 100, it) }
        }
    }

    private fun texts(view: View): List<String> = when (view) {
        is ViewGroup -> (0 until view.childCount).flatMap { texts(view.getChildAt(it)) }
        is TextView -> listOf(view.text.toString())
        else -> emptyList()
    }

    private companion object {
        const val RELAY = "https://play.bubblebuffer.net"
        const val RELAY_HOST = "play.bubblebuffer.net"
        const val MINUTE = 60_000L
        const val HOUR = 60 * MINUTE
        const val DAY = 24 * HOUR
    }
}

/**
 * Stands in for the AndroidKeyStore provider, which the JVM does not have. KeystoreKeyWrap asks for
 * "AndroidKeyStore" by name, so this provider serves a key store and an AES generator from memory for
 * the length of one test. Test code only; the production wrap is unchanged.
 */
private object FakeAndroidKeyStore {
    private const val NAME = "AndroidKeyStore"
    private val keys = HashMap<String, SecretKey>()

    fun install() {
        Security.removeProvider(NAME)
        Security.insertProviderAt(FakeProvider(), 1)
    }

    fun uninstall() {
        Security.removeProvider(NAME)
        synchronized(keys) { keys.clear() }
    }

    private fun lookup(alias: String): SecretKey? = synchronized(keys) { keys[alias] }

    private fun store(alias: String, key: SecretKey) = synchronized(keys) { keys[alias] = key }

    private fun forget(alias: String) = synchronized(keys) { keys.remove(alias) }

    private fun names(): List<String> = synchronized(keys) { keys.keys.toList() }

    private class FakeProvider : Provider(NAME, "1.0", "in-memory stand-in for AndroidKeyStore") {
        init {
            putService(object : Provider.Service(this, "KeyStore", NAME, "", emptyList(), emptyMap()) {
                override fun newInstance(constructorParameter: Any?): Any = FakeKeyStore()
            })
            putService(object : Provider.Service(this, "KeyGenerator", "AES", "", emptyList(), emptyMap()) {
                override fun newInstance(constructorParameter: Any?): Any = FakeAesGenerator()
            })
        }
    }

    private class FakeKeyStore : KeyStoreSpi() {
        override fun engineGetKey(alias: String?, password: CharArray?): Key? = alias?.let { lookup(it) }

        override fun engineGetCertificateChain(alias: String?): Array<Certificate>? = null

        override fun engineGetCertificate(alias: String?): Certificate? = null

        override fun engineGetCreationDate(alias: String?): Date? = null

        override fun engineSetKeyEntry(
            alias: String?,
            key: Key?,
            password: CharArray?,
            chain: Array<out Certificate>?,
        ) {
            throw KeyStoreException("the test key store takes keys only from the generator")
        }

        override fun engineSetKeyEntry(alias: String?, key: ByteArray?, chain: Array<out Certificate>?) {
            throw KeyStoreException("the test key store takes keys only from the generator")
        }

        override fun engineSetCertificateEntry(alias: String?, cert: Certificate?) {
            throw KeyStoreException("the test key store holds no certificates")
        }

        override fun engineDeleteEntry(alias: String?) {
            alias?.let { forget(it) }
        }

        override fun engineAliases(): Enumeration<String> = Collections.enumeration(names())

        override fun engineContainsAlias(alias: String?): Boolean = alias != null && lookup(alias) != null

        override fun engineSize(): Int = names().size

        override fun engineIsKeyEntry(alias: String?): Boolean = engineContainsAlias(alias)

        override fun engineIsCertificateEntry(alias: String?): Boolean = false

        override fun engineGetCertificateAlias(cert: Certificate?): String? = null

        override fun engineStore(stream: OutputStream?, password: CharArray?) {
            throw IOException("the test key store is not persisted")
        }

        override fun engineLoad(stream: InputStream?, password: CharArray?) = Unit
    }

    private class FakeAesGenerator : KeyGeneratorSpi() {
        private var alias: String? = null

        override fun engineInit(random: SecureRandom?) = Unit

        override fun engineInit(keySize: Int, random: SecureRandom?) = Unit

        override fun engineInit(params: AlgorithmParameterSpec?, random: SecureRandom?) {
            alias = (params as? KeyGenParameterSpec)?.keystoreAlias
                ?: throw InvalidParameterException("the test generator needs a KeyGenParameterSpec")
        }

        override fun engineGenerateKey(): SecretKey {
            val name = alias ?: throw IllegalStateException("the test generator was not initialised")
            val key = SecretKeySpec(ByteArray(32).also { SecureRandom().nextBytes(it) }, "AES")
            store(name, key)
            return key
        }
    }
}
