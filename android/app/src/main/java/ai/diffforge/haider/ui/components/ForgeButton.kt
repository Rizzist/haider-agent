package ai.diffforge.haider.ui.components

import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.defaultMinSize
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Dp

enum class ForgeButtonKind { Filled, Ghost, Destructive }

/**
 * Filled / ghost / destructive button.
 *
 * A filled accent surface always takes `accentInk`: `Color.White` on the 971
 * ember accent is 2.64:1 in dark (UI-SPEC 2.1, trap 6.6.2).
 */
@Composable
fun ForgeButton(
    text: String,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
    kind: ForgeButtonKind = ForgeButtonKind.Filled,
    enabled: Boolean = true,
    minHeight: Dp = ForgeSize.touch,
    contentDescription: String? = null,
    leading: (@Composable () -> Unit)? = null,
) {
    val colors = Forge.colors
    val type = Forge.type
    val fill = when (kind) {
        ForgeButtonKind.Filled -> colors.accent
        ForgeButtonKind.Ghost -> colors.surfaceControl
        ForgeButtonKind.Destructive -> colors.red.copy(alpha = 0.16f)
    }
    val ink = when (kind) {
        ForgeButtonKind.Filled -> colors.accentInk
        ForgeButtonKind.Ghost -> colors.textSoft
        ForgeButtonKind.Destructive -> colors.red
    }
    val outline = when (kind) {
        ForgeButtonKind.Filled -> colors.accent
        ForgeButtonKind.Ghost -> colors.borderStrong
        ForgeButtonKind.Destructive -> colors.red.copy(alpha = 0.55f)
    }
    Row(
        modifier = modifier
            .heightIn(min = minHeight)
            .defaultMinSize(minWidth = ForgeSize.rowMin)
            .clip(ForgeShapes.pill)
            .background(fill)
            .border(ForgeSize.hairline, outline, ForgeShapes.pill)
            .clickable(enabled = enabled, onClick = onClick)
            .alpha(if (enabled) 1f else DISABLED_ALPHA)
            .padding(horizontal = ForgeSpace.xl)
            .semantics {
                this.role = Role.Button
                if (contentDescription != null) {
                    this.contentDescription = contentDescription
                }
            },
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(ForgeSpace.sm, Alignment.CenterHorizontally),
    ) {
        leading?.invoke()
        Text(text, style = type.button, color = ink, maxLines = 1, overflow = TextOverflow.Ellipsis)
    }
}
