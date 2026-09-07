package ai.diffforge.haider.ui.components

import ai.diffforge.haider.ui.daemon.SessionVisualState
import ai.diffforge.haider.ui.theme.Forge
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
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.semantics.clearAndSetSemantics

/**
 * The brand-dot for one session, ported from the desktop rail's icon strip
 * (`SessionsRail.jsx:41`, `modelBrand.jsx`): the mark of the provider the
 * session is on, falling back to a plain dot when the family is unknown.
 *
 * In the minimalist row this glyph is the *only* visual state channel, so it
 * carries the accent: needs-input takes the accent colour, errored takes red,
 * and a running session breathes. The word itself lives in the row's merged
 * `contentDescription`, which is what keeps state from being colour-only.
 */
@Composable
fun SessionGlyph(
    provider: String?,
    state: SessionVisualState,
    animate: Boolean,
    modifier: Modifier = Modifier,
) {
    val colors = Forge.colors
    val (mark, brand) = providerMark(provider)
    val tint = when (state) {
        SessionVisualState.NeedsInput -> colors.stateNeedsInput
        SessionVisualState.Errored -> colors.stateErrored
        SessionVisualState.WaitingForNetwork -> colors.amber
        else -> brand
    }
    // Errored never animates — nothing pulses for a corpse.
    val alpha = if (animate && state == SessionVisualState.Running) {
        val transition = rememberInfiniteTransition(label = "glyph-breath")
        transition.animateFloat(
            initialValue = 0.45f,
            targetValue = 1f,
            animationSpec = infiniteRepeatable(tween(900), RepeatMode.Reverse),
            label = "glyph-breath-alpha",
        ).value
    } else {
        1f
    }
    Box(
        modifier = modifier
            .clearAndSetSemantics { }
            .size(ForgeSize.avatar)
            .alpha(alpha)
            .clip(CircleShape)
            .background(tint.copy(alpha = 0.14f))
            .border(ForgeSize.hairline, tint.copy(alpha = 0.5f), CircleShape),
        contentAlignment = Alignment.Center,
    ) {
        if (mark == null) {
            // Unknown family: the plain status dot the desktop falls back to.
            Box(
                Modifier
                    .size(ForgeSize.stateDot)
                    .clip(CircleShape)
                    .background(tint),
            )
        } else {
            Text(mark, style = Forge.type.label, color = tint)
        }
    }
}

/** Substring detection over the provider id, as `modelBrand.jsx:89` does. */
@Composable
private fun providerMark(provider: String?): Pair<String?, Color> {
    val colors = Forge.colors
    val normalized = provider.orEmpty().lowercase()
    return when {
        "anthropic" in normalized || "claude" in normalized -> "✳" to Color(0xFFD97757)
        "gemini" in normalized || "google" in normalized -> "✦" to Color(0xFF4E86F5)
        "deepseek" in normalized -> "D" to Color(0xFF4D6BFE)
        "qwen" in normalized -> "Q" to Color(0xFF615CED)
        "kimi" in normalized || "moonshot" in normalized -> "K" to Color(0xFF16A8F0)
        "mistral" in normalized -> "M" to Color(0xFFFF7000)
        "llama" in normalized || "meta" in normalized -> "L" to Color(0xFF0668E1)
        "glm" in normalized || "zhipu" in normalized -> "G" to Color(0xFF3859FF)
        "grok" in normalized || "xai" in normalized -> "X" to colors.text
        "openai" in normalized || "codex" in normalized -> "AI" to colors.text
        normalized.isBlank() -> null to colors.textMuted
        else -> null to colors.textMuted
    }
}
