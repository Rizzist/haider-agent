package ai.diffforge.haider.ui.components

import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeSize
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.ripple
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.semantics

/**
 * The one icon button: a 48 dp touch box around a 44 dp visual circle
 * (UI-SPEC 4.1). Replaces the 970 `HeaderButton`, which used three different
 * geometries on one bar.
 */
@Composable
fun ForgeIconButton(
    onClick: () -> Unit,
    contentDescription: String,
    modifier: Modifier = Modifier,
    enabled: Boolean = true,
    background: Color = Color.Transparent,
    content: @Composable () -> Unit,
) {
    val interaction = remember { MutableInteractionSource() }
    Box(
        modifier = modifier
            .size(ForgeSize.touch)
            .clip(CircleShape)
            .semantics {
                this.contentDescription = contentDescription
                this.role = Role.Button
            }
            .clickable(
                enabled = enabled,
                interactionSource = interaction,
                indication = ripple(bounded = false, radius = ForgeSize.control / 2),
                onClick = onClick,
            ),
        contentAlignment = Alignment.Center,
    ) {
        Box(
            // The dimming layer comes FIRST so it encloses the fill it is
            // dimming. Placed after `.background()` the layer covers only the
            // content, which is how the Save button ended up with a fill that
            // ignored its own state (round 5, P2).
            modifier = Modifier
                .alpha(if (enabled) 1f else DISABLED_ALPHA)
                .size(ForgeSize.control)
                .clip(CircleShape)
                .background(background),
            contentAlignment = Alignment.Center,
            content = { content() },
        )
    }
}

/** A 9 dp attention dot with a bg-coloured ring, drawn at a control's top-end corner. */
@Composable
fun AttentionBadge(color: Color, modifier: Modifier = Modifier) {
    val colors = Forge.colors
    Box(
        modifier = modifier
            .size(ForgeSize.badgeDot)
            .clip(CircleShape)
            .background(colors.bg),
        contentAlignment = Alignment.Center,
    ) {
        Box(
            Modifier
                .size(ForgeSize.stateDot)
                .clip(CircleShape)
                .background(color),
        )
    }
}

const val DISABLED_ALPHA = 0.55f
