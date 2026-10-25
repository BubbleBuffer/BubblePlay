package de.bubblebuffer.bubbleplay.auth

import org.junit.Assert.assertEquals
import org.junit.Test

class AuthJsonTest {
    @Test
    fun parsesAndWritesTheSubsetBackUnchanged() {
        val text = """{"a":"x\"y\\z\n","b":[1,-2,true,false,null,{"c":[]}],"d":1.5e3,"e":{}}"""
        val parsed = AuthJson.parseObject(text)
        assertEquals(1L, (parsed["b"] as List<*>)[0])
        assertEquals(AuthJson.JsonNumber("1.5e3"), parsed["d"])
        assertEquals(text, AuthJson.write(parsed))
        assertEquals("""{"k":"\u0001é"}""", AuthJson.write(mapOf("k" to "\u0001é")))
        assertEquals(mapOf("a" to "b"), AuthJson.parseObject(" {\n\"a\" : \"b\" } "))
    }

    @Test
    fun refusesWhatOrgJsonImplementationsMightDisagreeOn() {
        val bad = listOf(
            """{"a":1,"a":2}""", """{"a":1} x""", """{"a":01}""", """{"a":"\ud800"}""", """{"a":tru}""",
            """[1]""", """{"a":"x""", """{'a':1}""", """{"a":1,}""", "", """{"a":"\x"}""",
            "{\"a\":\"\u0001\"}", "[".repeat(5) + "{" + "[".repeat(40),
        )
        for (text in bad) assertThrows<AuthJson.Malformed> { AuthJson.parseObject(text) }
    }
}
