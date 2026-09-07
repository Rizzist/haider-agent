package ai.diffforge.haider.ui.chat

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.components.ForgeChip
import ai.diffforge.haider.ui.state.SessionViewTab
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.stringResource

const val SESSION_VIEW_HEADER_TAG = "session_view_header"

/**
 * The session surface's own slim header: a Chat | Shell segmented switch.
 *
 * The desktop puts this on the one header row it has (`SessionSurface.jsx:127`
 * — "ONE header row on every tab: small session title + Chat|Shell|Traj
 * segmented toggle"). A phone keeps its top bar, and 360 dp has no room for
 * the toggle beside the menu, theme and overflow controls, so the switch gets
 * the slim row the spec allows instead. The title is not repeated here — the
 * top bar already carries it (addition F, G4).
 *
 * There is no Traj tab: it is not a phone-sized problem.
 */
@Composable
fun SessionViewHeader(
    tab: SessionViewTab,
    shellAvailable: Boolean,
    onSelect: (SessionViewTab) -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = Forge.colors
    val type = Forge.type
    Row(
        modifier = modifier
            .fillMaxWidth()
            .heightIn(min = ForgeSize.touch)
            .padding(horizontal = ForgeSpace.lg)
            .testTag(SESSION_VIEW_HEADER_TAG),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(ForgeSpace.md),
    ) {
        SessionViewTab.entries.forEach { candidate ->
            val label = stringResource(
                when (candidate) {
                    SessionViewTab.Chat -> R.string.tab_chat
                    SessionViewTab.Shell -> R.string.tab_shell
                },
            )
            ForgeChip(
                onClick = { onSelect(candidate) },
                height = ForgeSize.segmented,
                selected = tab == candidate,
                contentDescription = label,
            ) {
                Text(
                    label,
                    style = type.chip,
                    color = when {
                        tab == candidate -> colors.accent
                        // Shell is reachable and honest about being unavailable;
                        // it is not hidden, because hiding it would leave the
                        // user wondering whether the app has one at all.
                        candidate == SessionViewTab.Shell && !shellAvailable -> colors.textMuted
                        else -> colors.textSoft
                    },
                )
            }
        }
    }
}
