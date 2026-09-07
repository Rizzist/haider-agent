package ai.diffforge.haider.ui.daemon

import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * `session.read` ranges start at 1 and carry at most 1,024 envelopes, so a
 * transcript is paged and can legitimately be incomplete. The UI has to say so
 * rather than present truncation as whole history.
 */
class HistoryContractTest {

    @Test
    fun `ranges start at one and never exceed the envelope cap`() {
        val ranges = TranscriptPager.ranges(headSeq = 2_500)
        assertEquals(1L, ranges.first().startSeq)
        assertEquals(2_500L, ranges.last().endSeq)
        assertTrue(ranges.all { it.endSeq - it.startSeq + 1 <= SESSION_READ_MAX_ENVELOPES })
        assertEquals(3, ranges.size)
    }

    @Test
    fun `ranges are contiguous and never overlap`() {
        val ranges = TranscriptPager.ranges(headSeq = 3_000)
        ranges.zipWithNext { a, b -> assertEquals(a.endSeq + 1, b.startSeq) }
    }

    @Test
    fun `a smaller page size is honoured for a tight byte cap`() {
        val ranges = TranscriptPager.ranges(headSeq = 10, pageSize = 4)
        assertEquals(
            listOf(1L to 4L, 5L to 8L, 9L to 10L),
            ranges.map { it.startSeq to it.endSeq },
        )
    }

    @Test
    fun `a request for more than the cap is clamped, not honoured`() {
        val ranges = TranscriptPager.ranges(headSeq = 5_000, pageSize = 100_000)
        assertTrue(ranges.all { it.endSeq - it.startSeq + 1 <= SESSION_READ_MAX_ENVELOPES })
    }

    @Test
    fun `an empty session asks for nothing`() {
        assertTrue(TranscriptPager.ranges(headSeq = 0).isEmpty())
    }

    @Test
    fun `resuming from a sequence does not re-read what is already applied`() {
        val ranges = TranscriptPager.ranges(headSeq = 2_048, fromSeq = 1_025)
        assertEquals(1_025L, ranges.first().startSeq)
        assertEquals(1, ranges.size)
    }

    @Test
    fun `partial history reports how far it actually got`() = runTest {
        val service = FakeDaemonService(FakeScenario.Populated)
        service.transcriptOverride = {
            TranscriptLoad.Partial(
                messages = emptyList(),
                reason = "an envelope exceeds the mobile limit",
                loadedThroughSeq = 900,
                headSeq = 981,
            )
        }
        val load = service.transcript("s-nav")
        assertTrue(load is TranscriptLoad.Partial)
        load as TranscriptLoad.Partial
        assertEquals(900L, load.loadedThroughSeq)
        assertEquals(981L, load.headSeq)
    }

    @Test
    fun `unavailable history is unavailable, not silently empty`() = runTest {
        val service = FakeDaemonService(FakeScenario.Populated)
        service.transcriptOverride = { TranscriptLoad.Unavailable("history could not be read") }
        val load = service.transcript("s-nav")
        assertTrue(load is TranscriptLoad.Unavailable)
        assertTrue(load.messages.isEmpty())
    }

    @Test
    fun `search says how much of the roster it has covered`() = runTest {
        val service = FakeDaemonService(FakeScenario.LargeRoster)
        service.setSearchIndex(SearchIndexState(indexedSessions = 40, totalSessions = 240, complete = false))
        val outcome = service.search("Session task 3")
        assertTrue(!outcome.complete)
        assertTrue(outcome.index.inProgress)
        assertEquals(40, outcome.index.indexedSessions)
    }

    @Test
    fun `a fully indexed roster reports complete results`() = runTest {
        val service = FakeDaemonService(FakeScenario.Populated)
        val outcome = service.search("nav")
        assertTrue(outcome.complete)
        assertTrue(outcome.hits.isNotEmpty())
    }

    @Test
    fun `paging appends pages and stops when the cursor runs out`() = runTest {
        val service = FakeDaemonService(FakeScenario.LargeRoster)
        assertEquals(60, service.sessions.value.size)
        assertTrue(service.paging.value.hasMore)
        while (service.paging.value.hasMore) {
            service.loadMoreSessions()
        }
        assertEquals(240, service.sessions.value.size)
        assertEquals(240, service.sessions.value.map { it.id }.toSet().size)
    }
}
