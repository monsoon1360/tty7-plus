package io.tty7.mobile

import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.async
import kotlinx.coroutines.test.runTest
import kotlinx.serialization.json.*
import kotlin.test.*

class TerminalFeedTest {
    private val pane = Pane(1, "shell", null, 20, 2)
    private val cursor = obj("row" to JsonPrimitive(0), "col" to JsonPrimitive(0), "shape" to JsonPrimitive("block"),
        "visible" to JsonPrimitive(true), "blink" to JsonPrimitive(false))
    private val modes = obj("app_cursor" to JsonPrimitive(true), "app_keypad" to JsonPrimitive(false), "bracketed_paste" to JsonPrimitive(true))
    private fun frame(sequence: ULong, revision: String = "1", base: String? = null, epoch: String = "1",
        part: Int = 0, last: Boolean = true, lines: List<ScreenLine> = listOf(ScreenLine(0, "\u001b[31m界é"), ScreenLine(1, ""))) =
        TerminalFrame(revision, base, epoch, 20, 2, false, cursor, modes, lines, "subscription", sequence, part, last)
    private suspend fun TerminalFeed.receive(frame: TerminalFrame): TerminalFrame? { enqueue(frame); return stage(frames.receive()) }

    @Test fun multipartStagesCreditsButOnlyFinalAppliedStateRevealsFeed() = runTest {
        val acks = mutableListOf<ULong>()
        var reveals = 0
        val feed = TerminalFeed("workspace", pane, { reveals++ }) { _, part -> acks.add(part.sequence) }
        feed.bindSubscription("subscription")
        assertNull(feed.receive(frame(1uL, last = false, lines = listOf(ScreenLine(0, "\u001b[31m界é")))))
        assertEquals(listOf(1uL), acks)
        assertFalse(feed.synced)
        val complete = assertNotNull(feed.receive(frame(2uL, part = 1, lines = listOf(ScreenLine(1, "tail")))))
        assertEquals(listOf(ScreenLine(0, "\u001b[31m界é"), ScreenLine(1, "tail")), complete.lines)
        assertEquals(modes, complete.modes)
        assertEquals(listOf(1uL), acks)
        feed.acknowledgeParsed(complete)
        assertTrue(feed.synced)
        assertEquals(1, reveals)
        assertEquals(listOf(1uL, 2uL), acks)
        val patch = assertNotNull(feed.receive(frame(3uL, "2", "1", lines = listOf(ScreenLine(1, "changed")))))
        feed.acknowledgeParsed(patch)
        assertEquals(1, reveals)
    }

    @Test fun wrongBaseRequestsOneResyncAndFullReplacementRestoresPatches() = runTest {
        var requests = 0
        val acks = mutableListOf<ULong>()
        val feed = TerminalFeed("workspace", pane, {}, onResync = { requests++ }) { _, part -> acks.add(part.sequence) }
        feed.bindSubscription("subscription")
        feed.acknowledgeParsed(assertNotNull(feed.receive(frame(1uL))))
        assertNull(feed.receive(frame(2uL, "3", "2")))
        assertNull(feed.receive(frame(3uL, "4", "3")))
        assertEquals(1, requests)
        feed.acknowledgeParsed(assertNotNull(feed.receive(frame(4uL, "5"))))
        feed.acknowledgeParsed(assertNotNull(feed.receive(frame(5uL, "6", "5"))))
        assertEquals((1uL..5uL).toList(), acks)
        assertEquals(1, requests)
    }

    @Test fun epochGeometryAndAlternateChangesRequireFullReplacement() = runTest {
        var requests = 0
        val feed = TerminalFeed("workspace", pane, {}, onResync = { requests++ }) { _, _ -> }
        feed.bindSubscription("subscription")
        feed.acknowledgeParsed(assertNotNull(feed.receive(frame(1uL))))
        assertNull(feed.receive(frame(2uL, "2", "1", epoch = "2")))
        val resized = frame(3uL, "3", epoch = "2").copy(cols = 40, alternate = true)
        feed.acknowledgeParsed(assertNotNull(feed.receive(resized)))
        assertNull(feed.receive(frame(4uL, "4", "3", epoch = "2")))
        assertEquals(2, requests)
    }

    @Test fun canceledGenerationCannotFinishStagedStateOrAcknowledge() = runTest {
        val acks = mutableListOf<ULong>()
        val feed = TerminalFeed("workspace", pane, {}) { _, part -> acks.add(part.sequence) }
        feed.bindSubscription("subscription")
        assertNull(feed.receive(frame(1uL, last = false, lines = listOf(ScreenLine(0, "old")))))
        feed.cancel()
        assertNull(feed.stage(frame(2uL, part = 1, lines = listOf(ScreenLine(1, "stale")))))
        feed.acknowledgeParsed(frame(2uL))
        assertEquals(listOf(1uL), acks)
        assertFalse(feed.synced)
    }

    @Test fun duplicateRowsMissingRowsAndChangedPartMetadataNeverRender() = runTest {
        for (bad in listOf(
            frame(2uL, part = 1, lines = listOf(ScreenLine(0, "duplicate"))),
            frame(2uL, part = 1, lines = emptyList()),
            frame(2uL, revision = "2", part = 1, lines = listOf(ScreenLine(1, "changed revision"))),
        )) {
            var requests = 0
            val feed = TerminalFeed("workspace", pane, {}, onResync = { requests++ }) { _, _ -> }
            feed.bindSubscription("subscription")
            assertNull(feed.receive(frame(1uL, last = false, lines = listOf(ScreenLine(0, "first")))))
            assertNull(feed.receive(bad))
            assertEquals(1, requests)
            assertFalse(feed.synced)
        }
    }

    @Test fun wirePartsAndLogicalStagingHaveIndependentMemoryBounds() = runTest {
        var requests = 0
        val acks = mutableListOf<ULong>()
        val feed = TerminalFeed("workspace", pane, {}, onResync = { requests++ }) { _, part -> acks.add(part.sequence) }
        feed.bindSubscription("subscription")
        assertFailsWith<IllegalArgumentException> {
            feed.enqueue(frame(1uL, lines = listOf(ScreenLine(0, "x".repeat(128 * 1024 + 1)))))
        }
        val row = "x".repeat(128 * 1024)
        repeat(65) { part ->
            assertNull(feed.receive(frame((part + 1).toULong(), part = part, last = false,
                lines = listOf(ScreenLine(part, row))).copy(rows = 1000)))
        }
        assertEquals(1, requests)
        assertEquals(65, acks.size)
        assertFalse(feed.synced)
        feed.acknowledgeParsed(assertNotNull(feed.receive(frame(66uL, "2"))))
        assertTrue(feed.synced)
    }

    @Test fun historyBoundariesAndOpaqueCursorAreValidated() {
        fun page(lines: JsonArray, cursor: String? = null, more: Boolean = false) = obj("lines" to lines,
            "cursor" to (cursor?.let(::JsonPrimitive) ?: JsonNull), "has_more" to JsonPrimitive(more), "truncated" to JsonPrimitive(true))
        val line = obj("cols" to JsonPrimitive(20), "text" to JsonPrimitive("<script>界</script>"), "ansi" to JsonPrimitive("\u001b[31m界"))
        val valid = HistoryPage.fromJson(page(JsonArray(List(200) { line }), "opaque:next", true))
        assertEquals("opaque:next", valid.cursor)
        assertTrue(valid.truncated)
        assertEquals("<script>界</script>", valid.lines.first().jsonObject.text("text"))
        assertFailsWith<IllegalArgumentException> { HistoryPage.fromJson(page(JsonArray(List(201) { line }))) }
        assertFailsWith<IllegalArgumentException> { HistoryPage.fromJson(page(JsonArray(emptyList()), more = true)) }
        val huge = obj("cols" to JsonPrimitive(20), "text" to JsonPrimitive("x".repeat(128 * 1024)), "ansi" to JsonPrimitive(""))
        assertFailsWith<IllegalArgumentException> { HistoryPage.fromJson(page(JsonArray(listOf(huge)))) }
        assertFalse(HistoryPage.fromJson(page(JsonArray(emptyList()))).hasMore)
    }

    @Test fun canceledFeedRejectsHistoryArrivingAfterSwitch() = runTest {
        val response = CompletableDeferred<HistoryPage>()
        val requested = CompletableDeferred<String?>()
        val feed = TerminalFeed("workspace", pane, {}, onHistory = { _, cursor -> requested.complete(cursor); response.await() }) { _, _ -> }
        feed.bindSubscription("subscription")
        feed.acknowledgeParsed(assertNotNull(feed.receive(frame(1uL))))
        val result = async { runCatching { feed.history("opaque:older") } }
        assertEquals("opaque:older", requested.await())
        feed.cancel()
        response.complete(HistoryPage(JsonArray(emptyList()), null, false, false))
        assertIs<IllegalStateException>(result.await().exceptionOrNull())
    }
}
