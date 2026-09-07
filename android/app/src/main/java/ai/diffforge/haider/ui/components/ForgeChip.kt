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
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.minimumInteractiveComponentSize
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.Dp

/**
 * A 32 dp chip whose *touch* box is grown by [minimumInteractiveComponentSize],
 * not its pixels.
 *
 * `heightIn(min = 48.dp)` on a chip looks like accessibility and is not: it
 * inflates the visual box and is what made the 970 composer eat 100 dp of the
 * short axis (UI-SPEC 4.1, D5, trap 6.6.1).
 */
@Composable
fun ForgeChip(
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
    height: Dp = ForgeSize.chip,
    selected: Boolean = false,
    borderColor: Color? = null,
    backgroundColor: Color? = null,
    enabled: Boolean = true,
    contentDescription: String? = null,
    content: @Composable () -> Unit,
) {
    val colors = Forge.colors
    val fill = backgroundColor ?: if (selected) colors.accentWash else colors.surfaceControl
    val outline = borderColor ?: if (selected) colors.accent else colors.border
    Row(
        modifier = modifier
            .minimumInteractiveComponentSize()
            .height(height)
            .clip(ForgeShapes.pill)
            .background(fill)
            .border(ForgeSize.hairline, outline, ForgeShapes.pill)
            .clickable(enabled = enabled, onClick = onClick)
            .padding(horizontal = ForgeSpace.lg)
            .semantics {
                this.role = Role.Button
                if (contentDescription != null) this.contentDescription = contentDescription
            },
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(ForgeSpace.sm),
        content = { content() },
    )
}
