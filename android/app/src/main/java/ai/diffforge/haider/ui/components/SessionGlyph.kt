package ai.diffforge.haider.ui.components

import ai.diffforge.haider.ui.daemon.SessionVisualState
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeSize
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.semantics.clearAndSetSemantics

/**
 * The brand mark for one session's model, with the desktop rail's activity
 * badge (`modelBrand.jsx` `ModelBrandIcon`).
 *
 * Round 6 shipped text glyphs — "✳", "D", "AI" — which is what the owner asked
 * about; these are the real vendor marks, resolved by **model** id first and
 * provider only as a fallback (addition H5). The badge is the state channel: a
 * 5 dp dot at the bottom-right, green while a turn runs, amber while it waits
 * for a human, red on error, absent when idle. The word itself still lives in
 * the row's merged `contentDescription`, so state is never colour-only.
 */
@Composable
fun SessionGlyph(
    provider: String?,
    state: SessionVisualState,
    animate: Boolean,
    modifier: Modifier = Modifier,
    model: String? = null,
    /** The rail/ground colour the badge rings itself against. */
    ringAgainst: Color? = null,
) {
    val colors = Forge.colors
    val family = modelBrandFor(model, provider)
    val brand = family?.let { if (colors.isDark) it.color else it.colorLight } ?: colors.textDisabled
    val badge = when (state) {
        SessionVisualState.Running -> colors.stateRunning
        SessionVisualState.NeedsInput -> colors.stateNeedsInput
        SessionVisualState.WaitingForNetwork -> colors.amber
        SessionVisualState.Errored -> colors.stateErrored
        else -> null
    }
    // Errored never animates — nothing pulses for a corpse.
    val pulse = if (animate && state == SessionVisualState.Running) {
        val transition = rememberInfiniteTransition(label = "badge-pulse")
        transition.animateFloat(
            initialValue = 0.45f,
            targetValue = 1f,
            animationSpec = infiniteRepeatable(tween(800), RepeatMode.Reverse),
            label = "badge-pulse-alpha",
        ).value
    } else {
        1f
    }
    Box(
        modifier = modifier.clearAndSetSemantics { }.size(ForgeSize.avatar),
        contentAlignment = Alignment.Center,
    ) {
        val mark = markFor(family?.key)
        if (mark == null) {
            // Unknown family keeps the plain status dot the desktop falls back
            // to; a letter tile is only for brands the catalogue actually names.
            if (family?.letter != null) {
                Box(
                    Modifier
                        .size(ForgeSize.brandMark)
                        .clip(ForgeShapes.cardTight)
                        .background(brand),
                    contentAlignment = Alignment.Center,
                ) {
                    Text(family.letter, style = Forge.type.brandLetter, color = colors.surface)
                }
            } else {
                Box(
                    Modifier
                        .size(ForgeSize.stateDot)
                        .clip(CircleShape)
                        .background(badge ?: colors.textDisabled),
                )
            }
        } else {
            Icon(
                mark,
                contentDescription = null,
                tint = brand,
                modifier = Modifier.size(ForgeSize.brandMark),
            )
        }
        if (badge != null) {
            Box(
                Modifier
                    .align(Alignment.BottomEnd)
                    .size(ForgeSize.badgeDot)
                    .clip(CircleShape)
                    .background(ringAgainst ?: colors.bg),
                contentAlignment = Alignment.Center,
            ) {
                Box(
                    Modifier
                        .size(ForgeSize.activityDot)
                        .alpha(pulse)
                        .clip(CircleShape)
                        .background(badge),
                )
            }
        }
    }
}

/** The brand's own tile, without state: pickers and account rows. */
@Composable
fun BrandMarkOnly(model: String?, provider: String?, modifier: Modifier = Modifier) {
    val colors = Forge.colors
    val family = modelBrandFor(model, provider)
    val brand = family?.let { if (colors.isDark) it.color else it.colorLight } ?: colors.textMuted
    val mark = markFor(family?.key)
    Box(modifier = modifier.size(ForgeSize.avatar), contentAlignment = Alignment.Center) {
        when {
            mark != null -> Icon(
                mark,
                contentDescription = null,
                tint = brand,
                modifier = Modifier.size(ForgeSize.brandMark),
            )
            family?.letter != null -> Box(
                Modifier
                    .size(ForgeSize.brandMark)
                    .clip(ForgeShapes.cardTight)
                    .background(brand),
                contentAlignment = Alignment.Center,
            ) {
                Text(family.letter, style = Forge.type.brandLetter, color = colors.surface)
            }
            else -> Box(
                Modifier
                    .size(ForgeSize.stateDot)
                    .clip(CircleShape)
                    .background(colors.textDisabled),
            )
        }
    }
}

private fun markFor(key: String?): ImageVector? = when (key) {
    "openai" -> OpenAiMark
    "claude" -> ClaudeMark
    "gemini" -> GeminiMark
    "deepseek" -> DeepSeekMark
    "grok" -> GrokMark
    else -> null
}
