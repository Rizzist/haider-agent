package ai.diffforge.haider.ui.chat

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.daemon.ShellAvailability
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.stringResource

const val SHELL_VIEW_TAG = "shell_view"

/**
 * The session's shell view.
 *
 * On this platform it has nothing to drive. The android-standalone tool policy
 * disables `ProcessExec` — neither advertised nor dispatchable (contracts-v1
 * C4) — so there is no PTY to adopt and no command to run. Rather than present
 * an inert terminal that swallows keystrokes, the tab says what is true and
 * names the reason the daemon gave.
 *
 * When a later lane ships an on-device shell, [ShellAvailability.available]
 * flips and the terminal takes this space; nothing above here changes.
 */
@Composable
fun ShellView(
    availability: ShellAvailability,
    modifier: Modifier = Modifier,
) {
    val colors = Forge.colors
    val type = Forge.type
    Column(
        modifier = modifier
            .fillMaxSize()
            .padding(horizontal = ForgeSpace.xl, vertical = ForgeSpace.xl),
        verticalArrangement = Arrangement.spacedBy(ForgeSpace.md),
    ) {
        if (availability.available) {
            Text(
                stringResource(R.string.shell_ready),
                style = type.sessionTitle,
                color = colors.text,
                modifier = Modifier.testTag(SHELL_VIEW_TAG),
            )
            return@Column
        }
        Column(
            Modifier
                .fillMaxWidth()
                .clip(ForgeShapes.card)
                .background(colors.surface)
                .border(ForgeSize.hairline, colors.border, ForgeShapes.card)
                .padding(ForgeSpace.xl)
                .testTag(SHELL_VIEW_TAG),
            verticalArrangement = Arrangement.spacedBy(ForgeSpace.md),
        ) {
            Text(
                stringResource(R.string.shell_unavailable_title),
                style = type.sessionTitle,
                color = colors.text,
            )
            Text(
                stringResource(R.string.shell_unavailable_body),
                style = type.sessionMeta,
                color = colors.textMuted,
            )
            availability.reason?.let { reason ->
                // The daemon's own code, in the one type face that is allowed
                // to be monospace: this is machine output (addition F, G1).
                // mono: the daemon's own reason code, quoted verbatim.
                Text(reason, style = type.toolRow, color = colors.textMuted)
            }
        }
    }
}
