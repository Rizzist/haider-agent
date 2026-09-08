package ai.diffforge.haider.ui.fleet

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.chat.InputRequiredCard
import ai.diffforge.haider.ui.chat.Transcript
import ai.diffforge.haider.ui.components.ForgeIconButton
import ai.diffforge.haider.ui.components.StateDot
import ai.diffforge.haider.ui.daemon.FleetModel
import ai.diffforge.haider.ui.daemon.FleetNode
import ai.diffforge.haider.ui.daemon.MenuCoordinates
import ai.diffforge.haider.ui.daemon.SessionRow
import ai.diffforge.haider.ui.state.ChildTranscriptState
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawing
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.ChevronLeft
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.key
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextOverflow

/** The screen root, so a pin can assert what is and is not inside it. */
const val CHILD_TRANSCRIPT_TAG = "child_transcript"

/**
 * One descendant's own transcript, read-only.
 *
 * The feed is the **child's** — `session.attach`/`session.read` on the child's
 * own session id, mounted through the same [Transcript] the parent uses — never
 * reconstructed from the parent's rows and never given an invented title. The
 * header shows the daemon's callsign, or a visibly-marked agent-id fallback.
 *
 * There is deliberately **no composer here**. Delegation is the parent's turn:
 * a message to a child goes through `agent.message` addressed by
 * `(parent session, agent)`, which this lane does not ship, so a send box would
 * be a control with nothing behind it. What the screen *does* carry is the
 * child's own input-required card, because a child parked on a human is stuck
 * until someone answers it, and the answer is a compare-and-set against the
 * child's own coordinates.
 */
@Composable
fun ChildTranscriptScreen(
    state: ChildTranscriptState,
    node: FleetNode?,
    row: SessionRow?,
    parentTitle: String?,
    nowMs: Long,
    answeredElsewhere: Set<String>,
    onBack: () -> Unit,
    onAnswer: (MenuCoordinates, String, Int, String?) -> Unit,
    onAnswerSecret: (MenuCoordinates, String, Int, CharArray) -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = Forge.colors
    val type = Forge.type
    // Only the daemon's callsign counts as an identity. The session's *title*
    // is not one: it is the conversation's name, and using it here would dress
    // a parent-side label up as a delegation identity the daemon never
    // assigned. With no callsign the fallback is the agent id, marked as one.
    val label = FleetModel.label(node?.callsign, state.agentId ?: state.sessionId)
    val parent = parentTitle ?: state.parentSessionId
    Column(
        modifier = modifier
            .testTag(CHILD_TRANSCRIPT_TAG)
            .fillMaxSize()
            .background(colors.bg)
            // A full screen behind an early return owns its own insets, the
            // way Settings and Accounts do: the scaffold's padding is on a
            // Column this branch never reaches, so without this the header
            // sits under the status bar on a real device.
            .windowInsetsPadding(WindowInsets.safeDrawing),
    ) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .heightIn(min = ForgeSize.header)
                .padding(end = ForgeSpace.lg),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            ForgeIconButton(
                onClick = onBack,
                contentDescription = if (parent != null) {
                    stringResource(R.string.cd_back_to_parent, parent)
                } else {
                    stringResource(R.string.cd_back_to_parent_unknown)
                },
                visual = ForgeSize.headerControl,
            ) {
                Icon(
                    Icons.Rounded.ChevronLeft,
                    contentDescription = null,
                    tint = colors.textMuted,
                    modifier = Modifier.size(ForgeSize.iconSm),
                )
            }
            Column(Modifier.weight(1f)) {
                if (label.fallback) {
                    // mono: the agent id, standing in for a callsign the daemon
                    // never assigned — an identifier, and marked as one.
                    Text(
                        stringResource(R.string.fleet_id_fallback, label.text),
                        style = type.numeric,
                        color = colors.text,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                } else {
                    Text(
                        label.text,
                        style = type.sessionTitle,
                        color = colors.text,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                }
                Text(
                    if (parent != null) {
                        stringResource(R.string.fleet_under_parent, parent)
                    } else {
                        stringResource(R.string.fleet_parent_unknown)
                    },
                    style = type.sessionMeta,
                    color = colors.textMuted,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
            node?.let {
                Row(
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(ForgeSpace.sm),
                ) {
                    StateDot(FleetTone.color(it.state))
                    Text(
                        FleetTone.label(it.state),
                        style = type.chip,
                        color = colors.textMuted,
                    )
                }
            }
        }

        // Lineage, exactly as the fleet published it. An absent parent says
        // unknown rather than borrowing the session this screen was opened from.
        // mono: session and agent ids — identifiers a person copies verbatim.
        Text(
            stringResource(R.string.fleet_lineage, state.sessionId),
            style = type.numeric,
            color = colors.textMuted,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
            modifier = Modifier.padding(horizontal = ForgeSpace.xl),
        )
        Text(
            stringResource(R.string.fleet_child_readonly),
            style = type.sessionMeta,
            color = colors.textMuted,
            modifier = Modifier.padding(
                horizontal = ForgeSpace.xl,
                vertical = ForgeSpace.sm,
            ),
        )

        state.notice?.let { notice ->
            Text(
                notice,
                style = type.sessionMeta,
                color = colors.amber,
                modifier = Modifier.padding(
                    horizontal = ForgeSpace.xl,
                    vertical = ForgeSpace.md,
                ),
            )
        }

        // The child's own card, answered against the child's own coordinates.
        // Without all of them there is nothing to compare and set, so the card
        // renders with no answer affordance rather than guessing one.
        row?.needsInput?.let { needsInput ->
            key(needsInput.menuId, needsInput.requestSeq, needsInput.workerGeneration) {
                InputRequiredCard(
                    needsInput = needsInput,
                    nowMs = nowMs,
                    answeredElsewhere = needsInput.menuId in answeredElsewhere,
                    coordinates = MenuCoordinates.of(
                        sessionId = state.sessionId,
                        needsInput = needsInput,
                        commandId = "child-rendered",
                    ),
                    onAnswer = onAnswer,
                    onAnswerSecret = onAnswerSecret,
                    modifier = Modifier.padding(
                        horizontal = ForgeSpace.xl,
                        vertical = ForgeSpace.lg,
                    ),
                )
            }
        }

        Box(Modifier.fillMaxWidth().weight(1f)) {
            if (state.loading && state.messages.isEmpty()) {
                Text(
                    stringResource(R.string.fleet_child_loading),
                    style = type.sessionMeta,
                    color = colors.textMuted,
                    modifier = Modifier.padding(ForgeSpace.xl),
                )
            } else if (state.messages.isEmpty()) {
                Text(
                    stringResource(R.string.fleet_child_empty),
                    style = type.sessionMeta,
                    color = colors.textMuted,
                    modifier = Modifier.padding(ForgeSpace.xl),
                )
            } else {
                // Read-only: `onRetry` would resend the parent's last turn, and
                // this screen has no turn of its own to resend.
                Transcript(messages = state.messages, modifier = Modifier.fillMaxSize())
            }
        }
    }
}
