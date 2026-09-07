package ai.diffforge.haider.ui.start

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.components.ForgeButton
import ai.diffforge.haider.ui.components.ForgeButtonKind
import ai.diffforge.haider.ui.state.SetupStep
import ai.diffforge.haider.ui.state.SetupStepId
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.Check
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.res.stringResource

/**
 * One setup step. A completed step collapses to a single line, so the card
 * shrinks as the user advances and the suggestion block rises into the
 * reclaimed space — the screen is never emptier than when it started
 * (UI-SPEC 3.4).
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
    Row(
        modifier = Modifier
            .fillMaxWidth()
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
            if (step.done) {
                // One line of status, in regular type: this is not machine
                // output (addition F, G1/F3).
                doneDetail?.let {
                    Text(it, style = type.sessionMeta, color = colors.green)
                }
            } else if (step.current) {
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
