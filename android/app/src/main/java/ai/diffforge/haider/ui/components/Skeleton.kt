package ai.diffforge.haider.ui.components

import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeSpace
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.width
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.unit.Dp

/** A shimmering placeholder box. Static when motion is disabled. */
@Composable
fun Skeleton(
    width: Dp,
    modifier: Modifier = Modifier,
    height: Dp = ForgeSpace.md,
) {
    val colors = Forge.colors
    val alpha = if (motionEnabled()) {
        val transition = rememberInfiniteTransition(label = "skeleton")
        transition.animateFloat(
            initialValue = 0.25f,
            targetValue = 0.6f,
            animationSpec = infiniteRepeatable(tween(700), RepeatMode.Reverse),
            label = "skeleton-alpha",
        ).value
    } else {
        0.4f
    }
    Box(
        modifier
            .width(width)
            .height(height)
            .clip(ForgeShapes.pill)
            .background(colors.textMuted.copy(alpha = alpha)),
    )
}
