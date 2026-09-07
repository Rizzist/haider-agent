package ai.diffforge.haider.ui.start

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.ChevronRight
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.semantics

/**
 * Three capability-truthful prompts — SMS, screen and accessibility are exactly
 * the three capabilities the APK actually brokers
 * (`mobile_transport.rs:63-66`).
 *
 * Tapping one *fills the composer and does not send*: the user stays the author.
 */
@Composable
fun SuggestionList(onSuggestion: (String) -> Unit, modifier: Modifier = Modifier) {
    val colors = Forge.colors
    val type = Forge.type
    Column(modifier.fillMaxWidth()) {
        Text(
            stringResource(R.string.suggestions_header),
            style = type.drawerSection,
            color = colors.textMuted,
            modifier = Modifier.padding(top = ForgeSpace.xxl, bottom = ForgeSpace.md),
        )
        Column(
            Modifier
                .fillMaxWidth()
                .clip(ForgeShapes.card)
                .background(colors.surface)
                .border(ForgeSize.hairline, colors.border, ForgeShapes.card),
        ) {
            listOf(
                "✉" to R.string.suggestion_sms,
                "◉" to R.string.suggestion_screen,
                ">_" to R.string.suggestion_settings,
            ).forEach { (glyph, textRes) ->
                val text = stringResource(textRes)
                Row(
                    modifier = Modifier
                        .fillMaxWidth()
                        .heightIn(min = ForgeSize.touch)
                        .clickable { onSuggestion(text) }
                        .padding(horizontal = ForgeSpace.lg)
                        .semantics {
                            contentDescription = text
                            role = Role.Button
                        },
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Text(glyph, style = type.toolStrong, color = colors.textMuted)
                    Text(
                        text,
                        style = type.chatBody,
                        color = colors.chatText,
                        modifier = Modifier
                            .weight(1f)
                            .padding(horizontal = ForgeSpace.lg),
                    )
                    Icon(
                        Icons.Rounded.ChevronRight,
                        contentDescription = null,
                        tint = colors.textMuted,
                        modifier = Modifier.size(ForgeSize.iconSm),
                    )
                }
            }
        }
    }
}
