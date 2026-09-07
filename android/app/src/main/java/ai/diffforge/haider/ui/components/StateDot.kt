package ai.diffforge.haider.ui.components

import ai.diffforge.haider.ui.theme.ForgeMotion
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.semantics.clearAndSetSemantics
import androidx.compose.ui.unit.Dp

/** A 6 dp state dot. Always decorative: the parent already speaks the state. */
@Composable
fun StateDot(color: Color, modifier: Modifier = Modifier, size: Dp = ForgeSize.stateDot) {
    Box(
        modifier
            .clearAndSetSemantics { }
            .size(size)
            .clip(CircleShape)
            .background(color),
    )
}

/**
 * The running-turn marquee: three dots at 100/60/30 % alpha on a 900 ms cycle.
 * With motion off the dots render static at full alpha and the `RUNNING` word
 * carries the signal. Never used for an errored row.
 */
@Composable
fun RunningDots(color: Color, animate: Boolean, modifier: Modifier = Modifier) {
    val phase = if (animate) {
        val transition = rememberInfiniteTransition(label = "row-marquee")
        transition.animateFloat(
            initialValue = 0f,
            targetValue = 3f,
            animationSpec = infiniteRepeatable(
                animation = tween(ForgeMotion.MARQUEE_MS, easing = ForgeMotion.easing),
                repeatMode = RepeatMode.Restart,
            ),
            label = "row-marquee-phase",
        ).value
    } else {
        null
    }
    Row(
        modifier = modifier.clearAndSetSemantics { },
        horizontalArrangement = Arrangement.spacedBy(ForgeSpace.xxs),
    ) {
        repeat(3) { index ->
            val alpha = when {
                phase == null -> 1f
                else -> DOT_ALPHAS[((index - phase.toInt()) + 3) % 3]
            }
            Box(
                Modifier
                    .size(ForgeSpace.xs)
                    .clip(CircleShape)
                    .background(color.copy(alpha = alpha)),
            )
        }
    }
}

private val DOT_ALPHAS = floatArrayOf(1f, 0.6f, 0.3f)
