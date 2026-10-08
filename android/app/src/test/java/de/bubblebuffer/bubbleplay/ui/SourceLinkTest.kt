package de.bubblebuffer.bubbleplay.ui

import org.junit.Assert.assertEquals
import org.junit.Test

class SourceLinkTest {
    @Test fun acceptsOperatorSource() {
        val url = "https://example.org/modified-source?ref=release"
        assertEquals(url, SourceLink.normalize(url))
    }

    @Test fun rejectsUnsafeOrMissingLinks() {
        for (url in listOf(null, "", "javascript:alert(1)", "file:///tmp/source", "https://user:password@example.org/source")) {
            assertEquals(SourceLink.DEFAULT_URL, SourceLink.normalize(url))
        }
    }
}
