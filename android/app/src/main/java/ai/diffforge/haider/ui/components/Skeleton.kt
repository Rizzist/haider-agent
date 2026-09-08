package ai.diffforge.haider.ui.components

import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeSpace
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
    // A placeholder is by definition on screen, so it subscribes; it still
    // shares the one ticker (verify-10 O5).
    val alpha = rememberPulse(active = true, low = 0.25f, high = 0.6f)
    Box(
        modifier
            .width(width)
            .height(height)
            .clip(ForgeShapes.pill)
            .background(colors.textMuted.copy(alpha = alpha)),
    )
}
