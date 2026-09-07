package ai.diffforge.haider.ui.components

import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.defaultMinSize
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
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
    /**
     * The *visual* height. The target is always at least [ForgeSize.touch]:
     * a banner's 34 dp button keeps its 34 dp look inside a 48 dp target,
     * rather than being an exception to the rule.
     */
    minHeight: Dp = ForgeSize.touch,
    contentDescription: String? = null,
    leading: (@Composable () -> Unit)? = null,
) {
    val colors = Forge.colors
    val type = Forge.type
    // Disabled state is expressed in the COLOURS, not with Modifier.alpha.
    // The alpha modifier introduces a graphics layer, and on both emulators the
    // Save button lost its fill entirely across an enabled/keyboard transition
    // — a filled accent button that paints as nothing is worse than a dim one.
    val dim = if (enabled) 1f else DISABLED_ALPHA
    val fill = when (kind) {
        ForgeButtonKind.Filled -> colors.accent.copy(alpha = dim)
        ForgeButtonKind.Ghost -> colors.surfaceControl.copy(alpha = dim)
        ForgeButtonKind.Destructive -> colors.red.copy(alpha = 0.16f * dim)
    }
    val ink = when (kind) {
        ForgeButtonKind.Filled -> colors.accentInk
        ForgeButtonKind.Ghost -> colors.textSoft
        ForgeButtonKind.Destructive -> colors.red
    }.copy(alpha = if (enabled) 1f else 0.7f)
    val outline = when (kind) {
        ForgeButtonKind.Filled -> colors.accent
        ForgeButtonKind.Ghost -> colors.borderStrong
        ForgeButtonKind.Destructive -> colors.red.copy(alpha = 0.55f)
    }.copy(alpha = dim)
    Box(
        modifier = modifier
            .defaultMinSize(minWidth = ForgeSize.touch, minHeight = ForgeSize.touch)
            .clip(ForgeShapes.pill)
            .clickable(enabled = enabled, onClick = onClick)
            .semantics {
                this.role = Role.Button
                if (contentDescription != null) {
                    this.contentDescription = contentDescription
                }
            },
        contentAlignment = Alignment.Center,
    ) {
        Row(
            modifier = Modifier
                .heightIn(min = minHeight)
                .defaultMinSize(minWidth = ForgeSize.rowMin)
                .clip(ForgeShapes.pill)
                .background(fill)
                .border(ForgeSize.hairline, outline, ForgeShapes.pill)
                .padding(horizontal = ForgeSpace.xl),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(ForgeSpace.sm, Alignment.CenterHorizontally),
        ) {
            leading?.invoke()
            Text(text, style = type.button, color = ink, maxLines = 1, overflow = TextOverflow.Ellipsis)
        }
    }
}
