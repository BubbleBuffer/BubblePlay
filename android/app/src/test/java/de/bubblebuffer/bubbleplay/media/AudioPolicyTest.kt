package de.bubblebuffer.bubbleplay.media

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class AudioPolicyTest {
    @Test
    fun queuedFramesAreWrittenMinusPlayed() {
        assertEquals(480L, AudioPolicy.queuedFrames(writtenFrames = 960L, playbackHead = 480))
        assertEquals(0L, AudioPolicy.queuedFrames(writtenFrames = 960L, playbackHead = 960))
    }

    @Test
    fun queuedFramesSurviveThePlaybackHeadWrap() {
        // The head is an unsigned 32-bit counter: -1 as an int is 4294967295.
        assertEquals(3L, AudioPolicy.queuedFrames(writtenFrames = 0x1_0000_0002L, playbackHead = -1))
    }

    @Test
    fun framesConvertToMilliseconds() {
        assertEquals(10L, AudioPolicy.framesToMs(480))
        assertEquals(150L, AudioPolicy.framesToMs(7_200))
    }

    @Test
    fun dropsAboveHighWaterUntilBackUnderLowWater() {
        val policy = AudioDropPolicy()
        assertFalse(policy.shouldDrop(150)) // exactly 150 ms is not "more than 150"
        assertTrue(policy.shouldDrop(151))
        assertTrue(policy.shouldDrop(120)) // draining, still above 60
        assertTrue(policy.shouldDrop(60))
        assertFalse(policy.shouldDrop(59)) // back under 60: play again
        assertFalse(policy.shouldDrop(100)) // below the high mark, not dropping
        assertFalse(policy.dropping)
    }

    @Test
    fun resetClearsTheDropState() {
        val policy = AudioDropPolicy()
        assertTrue(policy.shouldDrop(200))
        policy.reset()
        assertFalse(policy.shouldDrop(100))
    }

    @Test
    fun firstPacketIsNeverAGap() {
        assertFalse(AudioSequence().observe(7))
    }

    @Test
    fun consecutiveSequenceNumbersAreNotGaps() {
        val seq = AudioSequence()
        assertFalse(seq.observe(7))
        assertFalse(seq.observe(8))
        assertFalse(seq.observe(9))
    }

    @Test
    fun u32WrapIsNotAGap() {
        val seq = AudioSequence()
        assertFalse(seq.observe(4_294_967_295L))
        assertFalse(seq.observe(0))
        assertFalse(seq.observe(1))
    }

    @Test
    fun skippedSequenceNumberIsAGap() {
        val seq = AudioSequence()
        assertFalse(seq.observe(5))
        assertTrue(seq.observe(7))
        assertFalse(seq.observe(8))
    }

    @Test
    fun resetForgetsThePreviousSequence() {
        val seq = AudioSequence()
        seq.observe(5)
        seq.reset()
        assertFalse(seq.observe(100))
    }
}
