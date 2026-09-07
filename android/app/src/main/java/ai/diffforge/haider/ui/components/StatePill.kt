package ai.diffforge.haider.ui.components

import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.semantics.clearAndSetSemantics

/**
 * The word half of a state signal. State is never colour-only: every coloured
 * rail or dot is accompanied by a word (UI-SPEC 2.1 rule 3,
 * SessionsRail.jsx:684-685).
 *
 * Decorative by default: the merged session row speaks the state itself.
 */
@Composable
fun StatePill(
    text: String,
    color: Color,
    modifier: Modifier = Modifier,
    decorative: Boolean = true,
) {
    val type = Forge.type
    val base = modifier
        .clip(ForgeShapes.pill)
        .background(color.copy(alpha = 0.12f))
        .border(ForgeSize.hairline, color.copy(alpha = 0.55f), ForgeShapes.pill)
        .padding(horizontal = ForgeSpace.sm, vertical = ForgeSpace.xxs)
    Box(if (decorative) base.clearAndSetSemantics { } else base) {
        Text(text, style = type.drawerSection, color = color, maxLines = 1)
    }
}

/** The forked provenance pill; the same geometry in a muted colour. */
@Composable
fun ForkedPill(modifier: Modifier = Modifier) {
    StatePill(text = "FORKED", color = Forge.colors.textMuted, modifier = modifier)
}
