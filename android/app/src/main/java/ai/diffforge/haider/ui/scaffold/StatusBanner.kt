package ai.diffforge.haider.ui.scaffold

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.components.ForgeButton
import ai.diffforge.haider.ui.components.ForgeButtonKind
import ai.diffforge.haider.ui.state.BannerAction
import ai.diffforge.haider.ui.state.BannerModel
import ai.diffforge.haider.ui.state.BannerSeverity
import ai.diffforge.haider.ui.state.BannerText
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.material.icons.Icons
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.heightIn
import androidx.compose.material.icons.rounded.Bolt
import androidx.compose.material.icons.rounded.ChevronRight
import androidx.compose.material.icons.rounded.Close
import androidx.compose.material.icons.rounded.CloudOff
import androidx.compose.material.icons.rounded.ErrorOutline
import androidx.compose.material.icons.rounded.NotificationsOff
import androidx.compose.material.icons.rounded.PlayArrow
import androidx.compose.material.icons.rounded.QuestionAnswer
import androidx.compose.material.icons.rounded.SystemUpdate
import androidx.compose.material3.Icon
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.LiveRegionMode
import androidx.compose.ui.semantics.liveRegion
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.semantics.traversalIndex
import ai.diffforge.haider.ui.components.ForgeIconButton

/**
 * One banner under the header, at most one instance visible, chosen by the
 * severity ladder in [ai.diffforge.haider.ui.state.BannerResolver].
 *
 * Update messaging comes through here too: `UpdateBanner` now emits a
 * [BannerModel] instead of drawing its own row, so there is one banner style in
 * the app rather than two.
 */
@Composable
fun StatusBanner(
    model: BannerModel?,
    onAction: (BannerAction) -> Unit,
    onDismiss: (Int) -> Unit,
    modifier: Modifier = Modifier,
) {
    if (model == null) return
    val colors = Forge.colors
    val type = Forge.type
    val tint = when (model.severity) {
        BannerSeverity.Error -> colors.red
        BannerSeverity.Warning -> colors.amber
        BannerSeverity.Accent -> colors.accent
        BannerSeverity.Info -> colors.textMuted
    }
    Column(
        modifier
            .fillMaxWidth()
            .padding(horizontal = ForgeSpace.xl, vertical = ForgeSpace.sm)
            // Announced before the transcript when it appears (UI-SPEC 4.2).
            .semantics {
                liveRegion = LiveRegionMode.Polite
                traversalIndex = -1f
            },
    ) {
        // One line, 48 dp: glyph, what it is, and a chevron if tapping does
        // something. The three-line body and the wide Open button said the same
        // thing at four times the height (addition F, S2).
        val actionable = model.action != null
        val spoken = resolveLabel(model)
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .heightIn(min = ForgeSize.touch)
                .clip(ForgeShapes.row)
                .background(colors.surfaceRaised)
                .border(ForgeSize.hairline, tint.copy(alpha = 0.45f), ForgeShapes.row)
                .then(
                    if (actionable) {
                        Modifier
                            .clickable { onAction(model.action!!) }
                            .semantics {
                                contentDescription = spoken
                                role = Role.Button
                            }
                    } else {
                        Modifier
                    },
                )
                .padding(horizontal = ForgeSpace.lg),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(ForgeSpace.md),
        ) {
            Icon(
                icon(model),
                contentDescription = null,
                tint = tint,
                modifier = Modifier.size(ForgeSize.iconSm),
            )
            Text(
                resolve(model.title),
                style = type.sessionTitle,
                color = colors.text,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.weight(1f, fill = false),
            )
            model.suffix?.let {
                Text(
                    resolve(it),
                    style = type.sessionMeta,
                    color = colors.textSoft,
                    maxLines = 1,
                )
            }
            Box(Modifier.weight(1f))
            if (actionable) {
                Icon(
                    Icons.Rounded.ChevronRight,
                    contentDescription = null,
                    tint = tint,
                    modifier = Modifier.size(ForgeSize.iconSm),
                )
            }
            if (model.dismissible) {
                ForgeIconButton(
                    onClick = { onDismiss(model.rank) },
                    contentDescription = stringResource(R.string.cd_dismiss_banner),
                ) {
                    Icon(
                        Icons.Rounded.Close,
                        contentDescription = null,
                        tint = colors.textMuted,
                        modifier = Modifier.size(ForgeSize.iconSm),
                    )
                }
            }
        }
        if (model.progress) {
            LinearProgressIndicator(
                modifier = Modifier
                    .fillMaxWidth()
                    .height(ForgeSize.progressLine),
                color = tint,
                trackColor = Color.Transparent,
            )
        }
    }
}

/** What the strip announces: its title, plus its detail for the screen reader. */
@Composable
private fun resolveLabel(model: BannerModel): String = listOfNotNull(
    resolve(model.title),
    model.suffix?.let { resolve(it) },
    model.detail?.let { resolve(it) },
    model.actionLabel?.let { resolve(it) },
).joinToString(". ")

@Composable
private fun resolve(text: BannerText): String = when {
    text.literal != null -> text.literal
    text.resId != null && text.args.isEmpty() -> stringResource(text.resId)
    text.resId != null -> stringResource(text.resId, *text.args.toTypedArray())
    else -> ""
}

private fun icon(model: BannerModel) = when (model.rank) {
    1 -> Icons.Rounded.PlayArrow
    2 -> Icons.Rounded.PlayArrow
    3 -> Icons.Rounded.QuestionAnswer
    4 -> Icons.Rounded.NotificationsOff
    5 -> Icons.Rounded.Bolt
    6 -> Icons.Rounded.CloudOff
    7 -> Icons.Rounded.SystemUpdate
    else -> Icons.Rounded.ErrorOutline
}

/** The dismissal store: seven days, keyed by rank, reset by a state change. */
class SharedPreferencesBannerDismissals(
    private val preferences: android.content.SharedPreferences,
) : ai.diffforge.haider.ui.state.BannerDismissals {
    override fun snapshot(): Map<Int, Long> = preferences.all
        .mapNotNull { (key, value) ->
            val rank = key.removePrefix(PREFIX).toIntOrNull() ?: return@mapNotNull null
            val at = value as? Long ?: return@mapNotNull null
            rank to at
        }
        .toMap()

    override fun dismiss(rank: Int, atMs: Long) {
        preferences.edit().putLong("$PREFIX$rank", atMs).apply()
    }

    override fun clear(ranks: Set<Int>) {
        if (ranks.isEmpty()) return
        val editor = preferences.edit()
        ranks.forEach { editor.remove("$PREFIX$it") }
        editor.apply()
    }

    private companion object {
        const val PREFIX = "banner_dismissed_"
    }
}

/** In-memory dismissals, for previews and tests. */
class InMemoryBannerDismissals : ai.diffforge.haider.ui.state.BannerDismissals {
    private val entries = mutableMapOf<Int, Long>()
    override fun snapshot(): Map<Int, Long> = entries.toMap()
    override fun dismiss(rank: Int, atMs: Long) {
        entries[rank] = atMs
    }

    override fun clear(ranks: Set<Int>) {
        ranks.forEach(entries::remove)
    }
}

/** Preferences file for banner dismissals. */
const val BANNER_PREFERENCES = "haider_banners"
