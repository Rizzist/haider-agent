package ai.diffforge.haider.ui.chat

import ai.diffforge.haider.ui.daemon.FakeDaemonService
import ai.diffforge.haider.ui.daemon.FakeScenario
import ai.diffforge.haider.ui.state.SelectionRefusalCodes
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.test.StandardTestDispatcher
import kotlinx.coroutines.test.advanceUntilIdle
import kotlinx.coroutines.test.resetMain
import kotlinx.coroutines.test.runTest
import kotlinx.coroutines.test.setMain
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Before
import org.junit.Test

/**
 * A free-text model id reaches the daemon, and a refusal reaches the screen —
 * without anything in between manufacturing the user's consent.
 *
 * `confirm_new_epoch` has exactly one caller, and it is the button a person
 * pressed. These pin that at the seam, where a rendered assertion cannot see
 * the difference between "not confirmed" and "confirmed and refused anyway".
 */
@OptIn(ExperimentalCoroutinesApi::class)
class SelectionRefusalFlowTest {

    private val dispatcher = StandardTestDispatcher()

    @Before fun installDispatcher() = Dispatchers.setMain(dispatcher)

    @After fun restoreDispatcher() = Dispatchers.resetMain()

    private fun model(service: FakeDaemonService) = ChatViewModel(service, searchDebounceMs = 0)

    @Test
    fun `a typed id is sent to the daemon exactly as typed`() = runTest(dispatcher) {
        val service = FakeDaemonService(FakeScenario.Populated)
        val viewModel = model(service)
        viewModel.selectModel("local-lab", "router-experimental")
        advanceUntilIdle()

        assertEquals(
            listOf("selectModel:local-lab/router-experimental"),
            service.calls.filter { it.startsWith("selectModel:") },
        )
        // Accepted, so there is nothing to answer.
        assertNull(viewModel.state.value.selectionRefusal)
        // And nothing confirmed anything on the user's behalf.
        assertEquals(0, service.confirmedSelections)
    }

    @Test
    fun `an unknown model surfaces the daemon's code and confirms nothing`() =
        runTest(dispatcher) {
            val service = FakeDaemonService(FakeScenario.Populated)
            val viewModel = model(service)
            service.failNextSelection(SelectionRefusalCodes.MODEL_UNKNOWN)
            viewModel.selectModel("local-lab", "router-nope")
            advanceUntilIdle()

            val refusal = viewModel.state.value.selectionRefusal!!
            assertEquals(SelectionRefusalCodes.MODEL_UNKNOWN, refusal.code)
            // The coordinates the panel needs to say what the code means.
            assertEquals("local-lab", refusal.provider)
            assertEquals("router-nope", refusal.model)
            assertEquals(0, service.confirmedSelections)
        }

    @Test
    fun `only the confirm path sets confirm_new_epoch, and only once`() = runTest(dispatcher) {
        val service = FakeDaemonService(FakeScenario.Populated)
        val viewModel = model(service)
        service.failNextSelection(SelectionRefusalCodes.CACHE_EPOCH_CONFIRMATION_REQUIRED)
        viewModel.selectModel("local-lab", "router-experimental")
        advanceUntilIdle()
        assertEquals(0, service.confirmedSelections)

        viewModel.confirmRefusedSelection()
        advanceUntilIdle()

        assertEquals(1, service.confirmedSelections)
        assertNull(viewModel.state.value.selectionRefusal)
    }

    @Test
    fun `dismissing a refusal leaves the selection alone`() = runTest(dispatcher) {
        val service = FakeDaemonService(FakeScenario.Populated)
        val viewModel = model(service)
        service.failNextSelection(SelectionRefusalCodes.MODEL_UNKNOWN)
        viewModel.selectModel("local-lab", "router-nope")
        advanceUntilIdle()

        viewModel.dismissSelectionRefusal()
        advanceUntilIdle()

        assertNull(viewModel.state.value.selectionRefusal)
        assertEquals(0, service.confirmedSelections)
        // The refused pair was never committed to the catalog.
        assertEquals("claude-sonnet-4-5", service.models.value?.current?.model)
    }
}
