package ai.diffforge.haider.ui.start

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.components.ForgeButton
import ai.diffforge.haider.ui.components.ForgeButtonKind
import ai.diffforge.haider.ui.state.SetupStep
import ai.diffforge.haider.ui.state.SetupStepId
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.Check
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.res.stringResource

/**
 * One setup step: number, title and a one-line status, tappable to expand.
 *
 * F3 asked for expandable rows and round 6 shipped rows that only rendered the
 * current step's explanation — a pending step had no status line and no tap
 * handler at all, so tapping "Let Haider notify you" before Start did nothing
 * (verify-6 O5). The current step starts expanded; any step can be opened.
 */
@Composable
fun SetupStepRow(
    step: SetupStep,
    index: Int,
    doneDetail: String?,
    onAction: () -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    // Re-keyed on `current`, so advancing a step opens the new one without
    // slamming shut a row the user opened deliberately.
    var expanded by remember(step.current) { mutableStateOf(step.current) }
    val statusLine = when {
        step.done -> doneDetail ?: stringResource(R.string.setup_status_done)
        step.current -> stringResource(R.string.setup_status_now)
        else -> stringResource(R.string.setup_status_pending)
    }
    val label = stringResource(titleRes(step.id)) + ", " + statusLine
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .heightIn(min = ForgeSize.touch)
            .clip(ForgeShapes.row)
            .clickable(onClick = { expanded = !expanded })
            .semantics {
                contentDescription = label
                role = Role.Button
            }
            .padding(vertical = ForgeSpace.lg),
        verticalAlignment = Alignment.Top,
    ) {
        Marker(step = step, index = index)
        Column(
            Modifier
                .weight(1f)
                .padding(start = ForgeSpace.lg),
            verticalArrangement = Arrangement.spacedBy(ForgeSpace.sm),
        ) {
            Text(stringResource(titleRes(step.id)), style = type.sessionTitle, color = colors.text)
            // One line of status on every row, in regular type: this is not
            // machine output (addition F, G1/F3).
            Text(
                statusLine,
                style = type.sessionMeta,
                color = if (step.done) colors.green else colors.textMuted,
            )
            if (expanded && !step.done) {
                Text(stringResource(bodyRes(step.id)), style = type.sessionMeta, color = colors.textMuted)
                ForgeButton(
                    text = stringResource(actionRes(step.id)),
                    onClick = onAction,
                    kind = ForgeButtonKind.Filled,
                )
            }
        }
    }
}

@Composable
private fun Marker(step: SetupStep, index: Int) {
    val colors = Forge.colors
    val type = Forge.type
    Box(
        modifier = Modifier
            .size(ForgeSize.icon)
            .clip(CircleShape)
            .background(
                when {
                    step.done -> colors.green.copy(alpha = 0.16f)
                    step.current -> colors.accentWash
                    else -> androidx.compose.ui.graphics.Color.Transparent
                },
            )
            .border(
                ForgeSize.hairline,
                when {
                    step.done -> colors.green.copy(alpha = 0.5f)
                    step.current -> colors.accent
                    else -> colors.border
                },
                CircleShape,
            ),
        contentAlignment = Alignment.Center,
    ) {
        if (step.done) {
            Icon(
                Icons.Rounded.Check,
                contentDescription = null,
                tint = colors.green,
                modifier = Modifier.size(ForgeSize.iconSm),
            )
        } else {
            Text(
                "${index + 1}",
                style = type.label,
                color = if (step.current) colors.accent else colors.textMuted,
            )
        }
    }
}

fun titleRes(id: SetupStepId): Int = when (id) {
    SetupStepId.RunService -> R.string.step_service_title
    SetupStepId.Notifications -> R.string.step_notify_title
    SetupStepId.Battery -> R.string.step_battery_title
    SetupStepId.Model -> R.string.step_model_title
}

fun bodyRes(id: SetupStepId): Int = when (id) {
    SetupStepId.RunService -> R.string.step_service_body
    SetupStepId.Notifications -> R.string.step_notify_body
    SetupStepId.Battery -> R.string.step_battery_body
    SetupStepId.Model -> R.string.step_model_body
}

fun actionRes(id: SetupStepId): Int = when (id) {
    SetupStepId.RunService -> R.string.step_service_action
    SetupStepId.Notifications -> R.string.step_notify_action
    SetupStepId.Battery -> R.string.step_battery_action
    SetupStepId.Model -> R.string.step_model_action
}
