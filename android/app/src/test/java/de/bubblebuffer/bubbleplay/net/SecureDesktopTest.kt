package de.bubblebuffer.bubbleplay.net

import de.bubblebuffer.bubbleplay.ui.SecureBanner
import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Test

class SecureDesktopTest {
    private fun msg(kind: String?, vararg caps: String) = JSONObject().put("type", "desktop-state").put("desktop", "secure").apply {
        if (kind != null) put("secureKind", kind)
        put("secureCapabilities", JSONArray(caps.toList()))
    }

    @Test fun parsesKindAndKnownCapabilities() {
        assertEquals(SecureDesktop("uac", setOf("view", "deny")), SecureDesktop.parse(msg("uac", "view", "deny", "approve")))
    }

    @Test fun unknownOrMissingKindIsOtherWithoutCapabilities() {
        assertEquals(SecureDesktop("other", emptySet()), SecureDesktop.parse(JSONObject().put("desktop", "secure")))
        assertEquals("other", SecureDesktop.parse(msg("weird")).kind)
    }

    @Test fun bannerTextFollowsCapabilities() {
        assertEquals("Admin prompt open on the PC" to "View only. Turn on Admin mode on the PC to approve prompts.",
            SecureBanner.text(SecureDesktop("uac", setOf("view", "deny")), false, ""))
        assertEquals("PC is locked" to "You can sign in with the keyboard.",
            SecureBanner.text(SecureDesktop("lock", setOf("view", "input")), false, ""))
        assertEquals("BubblePlay can't show this screen. The stream resumes when it closes.",
            SecureBanner.text(SecureDesktop("other", emptySet()), false, "").second)
        assertEquals("Couldn't close the prompt (timeout).",
            SecureBanner.text(SecureDesktop("uac", setOf("view", "deny")), false, "timeout").second)
    }
}
