package ai.diffforge.haider.ui.chat

import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.update.UpdateUiState
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.ArrowForward
import androidx.compose.material.icons.rounded.SystemUpdate
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.unit.dp

@Composable
fun UpdateBanner(
    state: UpdateUiState,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val content = when (state) {
        UpdateUiState.Hidden -> return
        is UpdateUiState.Available -> BannerContent(
            title = "Update available → ${state.release.tag}",
            detail = "Download, verify SHA-256, then open Android's installer",
            clickable = true,
        )
        is UpdateUiState.Downloading -> BannerContent(
            title = "Downloading ${state.release.tag}…",
            detail = "The APK will be verified before installation",
            clickable = false,
        )
        is UpdateUiState.PermissionRequired -> BannerContent(
            title = "Allow installs to continue → Settings",
            detail = "Enable “Allow from this source”, then return to Haider",
            clickable = true,
        )
        is UpdateUiState.AwaitingConfirmation -> BannerContent(
            title = if (state.confirmationReady) {
                "Open Android's installer → ${state.tag}"
            } else {
                "Confirm ${state.tag} in Android's installer"
            },
            detail = "Installation never proceeds silently",
            clickable = state.confirmationReady,
        )
        is UpdateUiState.Error -> BannerContent(
            title = "Update refused",
            detail = state.message,
            clickable = state.release != null,
        )
    }
    val colors = Forge.colors
    val type = Forge.type
    Row(
        modifier = modifier
            .fillMaxWidth()
            .padding(horizontal = 12.dp, vertical = 7.dp)
            .clip(ForgeShapes.cardTight)
            .background(colors.surfaceRaised)
            .border(1.dp, if (state is UpdateUiState.Error) colors.red else colors.accentSoft, ForgeShapes.cardTight)
            .clickable(enabled = content.clickable, onClick = onClick)
            .padding(horizontal = 12.dp, vertical = 10.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Icon(
            Icons.Rounded.SystemUpdate,
            contentDescription = null,
            tint = if (state is UpdateUiState.Error) colors.red else colors.accentSoft,
        )
        Spacer(Modifier.width(10.dp))
        Column(Modifier.weight(1f)) {
            Text(content.title, style = type.toolStrong, color = colors.text)
            Text(content.detail, style = type.toolRow, color = colors.textMuted)
        }
        if (content.clickable) {
            Spacer(Modifier.width(8.dp))
            Icon(Icons.Rounded.ArrowForward, contentDescription = "Continue update", tint = colors.accentSoft)
        }
    }
}

private data class BannerContent(
    val title: String,
    val detail: String,
    val clickable: Boolean,
)
