package ai.diffforge.haider.daemon

import org.junit.Assert.*
import org.junit.Test

class SessionNotificationObserverTest {
    private val events = mutableListOf<String>()
    private val observer = SessionNotificationObserver(object : SessionNotificationSink {
        override fun attention(session: NotificationSession) { events += "input:${session.sessionId}:${session.input}" }
        override fun clearAttention(sessionId: String) { events += "clear:$sessionId" }
        override fun completion(session: NotificationSession) { events += "complete:${session.sessionId}" }
    })
    private fun row(head: Long = 1, generation: Long = 2, state: String? = "running", run: String? = "run-1", input: NotificationInput? = null) =
        NotificationSession("session-1", head, generation, run, state, input)
    @Test fun baselineAndReconnectNeverCreateHistoricalCompletion() {
        observer.accept(RosterUpdate.Baseline(listOf(row(state = "idle", run = null))))
        assertTrue(events.isEmpty())
        observer.accept(RosterUpdate.Changes(listOf(row(head = 2))))
        observer.accept(RosterUpdate.Reset)
        observer.accept(RosterUpdate.Changes(listOf(row(head = 3, state = "idle", run = null))))
        observer.accept(RosterUpdate.Baseline(listOf(row(head = 3, state = "idle", run = null))))
        assertTrue(events.isEmpty())
    }
    @Test fun completionRequiresKnownRunSameGenerationAndForwardHead() {
        observer.accept(RosterUpdate.Baseline(listOf(row())))
        observer.accept(RosterUpdate.Changes(listOf(row(head = 2, state = "idle", run = null))))
        observer.accept(RosterUpdate.Changes(listOf(row(head = 2, state = "idle", run = null))))
        observer.accept(RosterUpdate.Changes(listOf(row(head = 1))))
        assertEquals(listOf("complete:session-1"), events)
    }
    @Test fun unknownOrReplacedRunAndGenerationAreNotCompletionEvidence() {
        observer.accept(RosterUpdate.Baseline(listOf(row(run = null))))
        observer.accept(RosterUpdate.Changes(listOf(row(head = 2, state = "idle", run = null))))
        observer.accept(RosterUpdate.Changes(listOf(row(head = 3))))
        observer.accept(RosterUpdate.Changes(listOf(row(head = 4, state = "cancelled", run = "run-2"))))
        observer.accept(RosterUpdate.Changes(listOf(row(head = 5))))
        observer.accept(RosterUpdate.Changes(listOf(row(head = 6, generation = 3, state = "errored"))))
        assertTrue(events.isEmpty())
    }
    @Test fun inputDedupesFullCoordinatesAndClearsWhenResolved() {
        val input = NotificationInput("menu-1", 8, 2)
        observer.accept(RosterUpdate.Baseline(listOf(row(input = input))))
        observer.accept(RosterUpdate.Changes(listOf(row(head = 2, input = input))))
        assertEquals(1, events.size)
        observer.accept(RosterUpdate.Changes(listOf(row(head = 3, input = input.copy(requestSeq = 9)))))
        assertEquals(2, events.size)
        observer.accept(RosterUpdate.Changes(listOf(row(head = 4))))
        assertEquals("clear:session-1", events.last())
    }
    @Test fun inputRequiresCoordinatesFromSameGeneration() {
        observer.accept(RosterUpdate.Baseline(listOf(row(input = NotificationInput("menu", 1, 99)))))
        assertTrue(events.isEmpty())
    }
}
