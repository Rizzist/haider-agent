package ai.diffforge.haider.ui.fleet

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.daemon.FleetAgentState
import ai.diffforge.haider.ui.daemon.FleetStateView
import ai.diffforge.haider.ui.daemon.SessionVisualState
import ai.diffforge.haider.ui.theme.Forge
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.res.stringResource

/**
 * How a published agent state is spoken and coloured.
 *
 * The labels are the daemon's own words in sentence case — "Live", not
 * "Running" — because a client that renames a state has started reporting
 * something the daemon did not say. A state this build does not recognise keeps
 * its raw string verbatim behind "unrecognised state:", and a state the daemon
 * never published says exactly that.
 */
object FleetTone {

    @Composable
    fun label(state: FleetStateView): String = when {
        state.raw == null -> stringResource(R.string.fleet_state_unpublished)
        state.state == FleetAgentState.Unknown ->
            stringResource(R.string.fleet_state_unrecognised, state.raw)
        else -> stringResource(known(state.state))
    }

    /** The short word for a chip, where the row already carries the detail. */
    @Composable
    fun shortLabel(state: FleetStateView): String = when {
        state.raw == null -> stringResource(R.string.fleet_state_unpublished_short)
        state.state == FleetAgentState.Unknown -> state.raw
        else -> stringResource(known(state.state))
    }

    private fun known(state: FleetAgentState): Int = when (state) {
        FleetAgentState.Queued -> R.string.fleet_state_queued
        FleetAgentState.Live -> R.string.fleet_state_live
        FleetAgentState.Waiting -> R.string.fleet_state_waiting
        FleetAgentState.Done -> R.string.fleet_state_done
        FleetAgentState.Failed -> R.string.fleet_state_failed
        FleetAgentState.Cancelled -> R.string.fleet_state_cancelled
        FleetAgentState.Unknown -> R.string.fleet_state_unknown
    }

    /**
     * The dot colour. Unknown and unpublished share the neutral tone the
     * session rail uses: neither is allowed to read green.
     */
    @Composable
    fun color(state: FleetStateView): Color {
        val colors = Forge.colors
        return when (state.state) {
            FleetAgentState.Live -> colors.stateRunning
            FleetAgentState.Waiting -> colors.stateNeedsInput
            FleetAgentState.Queued -> colors.textMuted
            FleetAgentState.Failed -> colors.stateErrored
            FleetAgentState.Done -> colors.stateIdle
            FleetAgentState.Cancelled -> colors.textDisabled
            FleetAgentState.Unknown -> colors.stateUnknown
        }
    }

    /** The same channel for a roster row's folded state. */
    @Composable
    fun color(state: SessionVisualState): Color {
        val colors = Forge.colors
        return when (state) {
            SessionVisualState.Running -> colors.stateRunning
            SessionVisualState.NeedsInput -> colors.stateNeedsInput
            SessionVisualState.WaitingForNetwork -> colors.amber
            SessionVisualState.Errored -> colors.stateErrored
            SessionVisualState.Idle -> colors.stateIdle
            SessionVisualState.Unknown -> colors.stateUnknown
        }
    }

    /** The word a folded family dot speaks, so state is never colour-only. */
    @Composable
    fun word(state: SessionVisualState): String = stringResource(
        when (state) {
            SessionVisualState.Running -> R.string.state_running
            SessionVisualState.NeedsInput -> R.string.state_needs_input
            SessionVisualState.WaitingForNetwork -> R.string.state_waiting_network
            SessionVisualState.Errored -> R.string.state_errored
            SessionVisualState.Idle -> R.string.state_idle
            SessionVisualState.Unknown -> R.string.state_unknown
        },
    )
}
