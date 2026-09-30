package de.bubblebuffer.bubbleplay.ui

import android.annotation.SuppressLint
import android.app.Activity
import android.content.Intent
import android.graphics.Color
import android.os.Bundle
import android.view.ViewGroup
import android.webkit.CookieManager
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.webkit.WebView
import android.webkit.WebViewClient
import android.widget.Button
import android.widget.LinearLayout
import android.widget.TextView
import de.bubblebuffer.bubbleplay.R
import de.bubblebuffer.bubbleplay.net.ServerConfig
import java.io.ByteArrayInputStream

/**
 * Sign-in through the existing Worker gate, unchanged: the WebView opens
 * `<server>/`, which is held until the owner approves the ntfy request, then
 * serves login.html. After the password POST the page navigates back to `/`;
 * at that point both gate cookies exist and the navigation is intercepted
 * (the browser receiver never loads here) and handed to [StreamActivity].
 */
class LoginActivity : Activity() {
    private lateinit var web: WebView
    private lateinit var hint: TextView
    private lateinit var config: ServerConfig
    private var handedOff = false

    @SuppressLint("SetJavaScriptEnabled")
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        config = Prefs(this).serverConfig() ?: run { finish(); return }
        val cookies = CookieManager.getInstance()
        cookies.setAcceptCookie(true)
        if (intent.getBooleanExtra(EXTRA_FORCE, false)) expireAuthCookie()

        hint = TextView(this).apply {
            setText(R.string.login_waiting)
            setTextColor(0xFFDDE3EA.toInt())
            setPadding(dp(16), dp(12), dp(16), dp(12))
        }
        val retry = Button(this).apply {
            setText(R.string.retry)
            setOnClickListener { web.loadUrl(config.base.toString()) }
        }
        web = WebView(this).apply {
            settings.javaScriptEnabled = true
            settings.domStorageEnabled = true
            setBackgroundColor(Color.BLACK)
            webViewClient = GateClient()
        }
        cookies.setAcceptThirdPartyCookies(web, false)
        val top = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            addView(hint, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f))
            addView(retry)
        }
        setContentView(LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(0xFF10141A.toInt())
            fitsSystemWindows = true
            addView(top)
            addView(web, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f))
        })
        web.loadUrl(config.base.toString())
    }

    /** Drops the password session so a stale one cannot short-circuit the login page. */
    private fun expireAuthCookie() {
        val cookies = CookieManager.getInstance()
        val url = config.base.toString()
        cookies.setCookie(url, "${ServerConfig.AUTH_COOKIE}=; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=0")
        cookies.flush()
        if (ServerConfig.hasSessionCookies(cookies.getCookie(url))) {
            // Fall back to a clean slate (the approval gate will ask again).
            cookies.removeAllCookies(null)
            cookies.flush()
        }
    }

    private fun handOff() {
        if (handedOff) return
        handedOff = true
        CookieManager.getInstance().flush()
        startActivity(Intent(this, StreamActivity::class.java))
        finish()
    }

    override fun onDestroy() {
        if (::web.isInitialized) web.destroy()
        super.onDestroy()
    }

    private inner class GateClient : WebViewClient() {
        override fun shouldInterceptRequest(view: WebView, request: WebResourceRequest): WebResourceResponse? {
            val url = request.url
            val sameOrigin = url.host == config.base.host && url.scheme == config.base.scheme
            if (!sameOrigin) return null
            val path = url.path ?: "/"
            // The browser receiver must never start inside this WebView.
            if (path == "/receiver.js") return empty("application/javascript")
            if (request.isForMainFrame && path == "/" &&
                ServerConfig.hasSessionCookies(CookieManager.getInstance().getCookie(config.base.toString()))
            ) {
                runOnUiThread { handOff() }
                return empty("text/html")
            }
            return null
        }

        override fun shouldOverrideUrlLoading(view: WebView, request: WebResourceRequest): Boolean {
            val url = request.url
            // Stay on the relay origin; nothing else belongs in this sign-in view.
            return !(url.host == config.base.host && url.scheme == config.base.scheme)
        }

        override fun onPageFinished(view: WebView, url: String) {
            hint.setText(if (view.title?.contains("Sign in", ignoreCase = true) == true) R.string.login_password else R.string.login_waiting)
        }

        private fun empty(mime: String) = WebResourceResponse(mime, "utf-8", ByteArrayInputStream(ByteArray(0)))
    }

    private fun dp(value: Int) = (value * resources.displayMetrics.density).toInt()

    companion object {
        const val EXTRA_FORCE = "forceLogin"
    }
}
