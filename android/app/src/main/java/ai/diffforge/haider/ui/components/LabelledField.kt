package ai.diffforge.haider.ui.components

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
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.platform.LocalFocusManager
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.input.VisualTransformation

/**
 * A labelled single-line field: the app's one text input.
 *
 * It lives here rather than beside the Accounts form because three surfaces now
 * need the same one, and the thing they need identical is not the look — it is
 * that the FIELD itself keeps 48 dp, not just the box drawn around it. A field
 * that is 48 dp only because its container is leaves a sub-48 target the touch
 * sweep can see and a thumb can miss.
 */
@Composable
@Composable
internal fun LabelledField(
    label: String,
    value: String,
    onValueChange: (String) -> Unit,
    masked: Boolean = false,
    tag: String? = null,
    trailing: (@Composable () -> Unit)? = null,
) {
    val colors = Forge.colors
    val type = Forge.type
    val focus = LocalFocusManager.current
    Column(verticalArrangement = Arrangement.spacedBy(ForgeSpace.xs)) {
        // Sentence case: uppercase tracked labels are the drawer's alone
        // (addition F, G2).
        Text(label, style = type.sessionMeta, color = colors.textMuted)
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .heightIn(min = ForgeSize.touch)
                .clip(ForgeShapes.cardTight)
                .background(colors.surfaceControl)
                .border(ForgeSize.hairline, colors.border, ForgeShapes.cardTight)
                .padding(start = ForgeSpace.lg),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            BasicTextField(
                value = value,
                onValueChange = onValueChange,
                singleLine = true,
                textStyle = type.chatBody.copy(color = colors.text),
                cursorBrush = SolidColor(colors.accent),
                visualTransformation = if (masked) {
                    PasswordVisualTransformation()
                } else {
                    VisualTransformation.None
                },
                // A single-line field with no Done action left the keyboard up
                // with nothing to dismiss it but the back gesture.
                keyboardOptions = KeyboardOptions(
                    keyboardType = if (masked) KeyboardType.Password else KeyboardType.Text,
                    imeAction = ImeAction.Done,
                ),
                keyboardActions = KeyboardActions(onDone = { focus.clearFocus() }),
                modifier = Modifier
                    .weight(1f)
                    .padding(vertical = ForgeSpace.md)
                    .heightIn(min = ForgeSize.touch)
                    .let { if (tag != null) it.testTag(tag) else it },
            )
            trailing?.let {
                Box(Modifier.padding(end = ForgeSpace.xs)) { it() }
            }
        }
    }
}
