package ai.diffforge.haider.ui.chat

import ai.diffforge.haider.ui.state.BannerAction
import ai.diffforge.haider.ui.state.BannerModel
import ai.diffforge.haider.ui.state.BannerSeverity
import ai.diffforge.haider.ui.state.BannerText
import ai.diffforge.haider.update.UpdateUiState

/**
 * The update surface no longer draws its own row: it emits a [BannerModel] at
 * rank 7 and the one [ai.diffforge.haider.ui.scaffold.StatusBanner] renders it,
 * so update messaging inherits the app's single banner style.
 *
 * The `UpdateUiState` copy below is the 970 mapping preserved verbatim
 * (the old `UpdateBanner.kt:32-63`).
 */
object UpdateBannerModel {
    const val RANK = 7

    fun from(state: UpdateUiState): BannerModel? {
        val copy = when (state) {
            UpdateUiState.Hidden -> return null
            is UpdateUiState.Available -> Copy(
                "Update available → ${state.release.tag}",
                "Download, verify SHA-256, then open Android's installer",
                clickable = true,
                error = false,
            )
            is UpdateUiState.Downloading -> Copy(
                "Downloading ${state.release.tag}…",
                "The APK will be verified before installation",
                clickable = false,
                error = false,
            )
            is UpdateUiState.PermissionRequired -> Copy(
                "Allow installs to continue → Settings",
                "Enable “Allow from this source”, then return to Haider",
                clickable = true,
                error = false,
            )
            is UpdateUiState.AwaitingConfirmation -> Copy(
                if (state.confirmationReady) {
                    "Open Android's installer → ${state.tag}"
                } else {
                    "Confirm ${state.tag} in Android's installer"
                },
                "Installation never proceeds silently",
                clickable = state.confirmationReady,
                error = false,
            )
            is UpdateUiState.Error -> Copy(
                "Update refused",
                state.message,
                clickable = state.release != null,
                error = true,
            )
        }
        return BannerModel(
            rank = RANK,
            severity = if (copy.error) BannerSeverity.Error else BannerSeverity.Info,
            title = BannerText.literal(copy.title),
            detail = BannerText.literal(copy.detail),
            actionLabel = if (copy.clickable) BannerText.literal("Continue") else null,
            action = if (copy.clickable) BannerAction.ContinueUpdate else null,
            dismissible = true,
        )
    }

    private data class Copy(
        val title: String,
        val detail: String,
        val clickable: Boolean,
        val error: Boolean,
    )
}
