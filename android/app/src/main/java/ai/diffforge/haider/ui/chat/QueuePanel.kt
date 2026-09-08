package ai.diffforge.haider.ui.chat

import ai.diffforge.haider.ui.components.ForgeButton
import ai.diffforge.haider.ui.components.ForgeButtonKind
import ai.diffforge.haider.ui.daemon.QueueSnapshot
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.Text
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.text.style.TextOverflow

const val QUEUE_PANEL_TAG = "queue_panel"
const val DELIVERY_CHOOSER_TAG = "delivery_chooser"

/**
 * What is held behind the running turn (`queue.list`, `queue.rs:13`).
 *
 * Every mutation carries the revision the rows were read at, so a tap on a
 * stale snapshot is refused by the daemon rather than applied to whatever
 * moved into that ordinal. [QueueSnapshot.supported] separates "nothing
 * queued" from "this daemon has no queue" — the frame documentation is
 * explicit that absence is never an empty snapshot.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun QueuePanel(
    snapshot: QueueSnapshot,
    notice: String?,
    onPromote: (String) -> Unit,
    onRemove: (String) -> Unit,
    onDismiss: () -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    ModalBottomSheet(
        onDismissRequest = onDismiss,
        sheetState = rememberModalBottomSheetState(),
        containerColor = colors.surfaceRaised,
        shape = ForgeShapes.sheet,
    ) {
        Column(
            Modifier
                .padding(start = ForgeSpace.xl, end = ForgeSpace.xl, bottom = ForgeSpace.xxxl)
                .testTag(QUEUE_PANEL_TAG),
            verticalArrangement = Arrangement.spacedBy(ForgeSpace.md),
        ) {
            Text("Waiting to send", style = type.h4, color = colors.text)
            notice?.let {
                Text(it, style = type.sessionMeta, color = colors.amber)
            }
            when {
                !snapshot.supported -> Text(
                    "This daemon does not hold a queue.",
                    style = type.sessionMeta,
                    color = colors.textMuted,
                )
                snapshot.rows.isEmpty() -> Text(
                    "Nothing is waiting.",
                    style = type.sessionMeta,
                    color = colors.textMuted,
                )
                else -> snapshot.rows.sortedBy { it.ordinal }.forEach { row ->
                    Row(
                        modifier = Modifier
                            .fillMaxWidth()
                            .heightIn(min = ForgeSize.touch)
                            .clip(ForgeShapes.row)
                            .background(colors.surface)
                            .border(ForgeSize.hairline, colors.border, ForgeShapes.row)
                            .padding(horizontal = ForgeSpace.lg),
                        verticalAlignment = Alignment.CenterVertically,
                        horizontalArrangement = Arrangement.spacedBy(ForgeSpace.sm),
                    ) {
                        Text(
                            "${row.ordinal}",
                            style = type.selectLabel,
                            color = colors.textMuted,
                        )
                        Text(
                            row.text,
                            style = type.sessionMeta,
                            color = colors.text,
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                            modifier = Modifier.weight(1f),
                        )
                        ForgeButton(
                            text = "Send now",
                            onClick = { onPromote(row.id) },
                            kind = ForgeButtonKind.Ghost,
                            minHeight = ForgeSize.chip,
                        )
                        ForgeButton(
                            text = "Drop",
                            onClick = { onRemove(row.id) },
                            kind = ForgeButtonKind.Destructive,
                            minHeight = ForgeSize.chip,
                        )
                    }
                }
            }
        }
    }
}

/**
 * Send-while-running: queue it, or steer the turn that is already going.
 *
 * `DeliveryMode`'s wire default is Steer, and this asks rather than assuming,
 * because the two do different things to a turn in flight. `Subturn` exists on
 * the wire and is not offered: it is a delegation shape, not a composer choice.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun DeliveryChooser(
    onSteer: () -> Unit,
    onQueue: () -> Unit,
    onDismiss: () -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    ModalBottomSheet(
        onDismissRequest = onDismiss,
        sheetState = rememberModalBottomSheetState(),
        containerColor = colors.surfaceRaised,
        shape = ForgeShapes.sheet,
    ) {
        Column(
            Modifier
                .padding(start = ForgeSpace.xl, end = ForgeSpace.xl, bottom = ForgeSpace.xxxl)
                .testTag(DELIVERY_CHOOSER_TAG),
            verticalArrangement = Arrangement.spacedBy(ForgeSpace.md),
        ) {
            Text("A turn is already running", style = type.h4, color = colors.text)
            Text(
                "Steer it now, or hold this until the turn finishes.",
                style = type.sessionMeta,
                color = colors.textMuted,
            )
            Row(horizontalArrangement = Arrangement.spacedBy(ForgeSpace.md)) {
                ForgeButton(text = "Steer now", onClick = onSteer)
                ForgeButton(text = "Wait in line", onClick = onQueue, kind = ForgeButtonKind.Ghost)
            }
        }
    }
}
