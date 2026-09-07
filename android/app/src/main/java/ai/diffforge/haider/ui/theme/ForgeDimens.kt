package ai.diffforge.haider.ui.theme

import androidx.compose.animation.core.CubicBezierEasing
import androidx.compose.animation.core.Easing
import androidx.compose.ui.unit.dp

/**
 * Spacing, size, motion and elevation tokens (UI-SPEC 2.3-2.5).
 *
 * Law: no `.dp` literal may appear in a layout modifier outside [ForgeSpace],
 * [ForgeSize] and [ForgeShapes]. This file is the single place those numbers live.
 */
object ForgeSpace {
    /** 4 dp base grid, with two half-steps reserved for optical work. */
    val xxs = 2.dp
    val xs = 4.dp
    val sm = 6.dp
    val md = 8.dp
    val lg = 12.dp
    val xl = 16.dp
    val xxl = 20.dp
    val xxxl = 24.dp
    val huge = 32.dp
}

object ForgeSize {
    /** Minimum interactive box, everywhere, no exceptions. */
    val touch = 48.dp

    /** Visible circle of an icon button inside a [touch] box. */
    val control = 44.dp
    val icon = 22.dp
    val iconMd = 20.dp
    val iconSm = 18.dp
    val avatar = 22.dp
    val markLg = 34.dp

    /** Session-row state rail. */
    val rail = 3.dp
    val hairline = 1.dp
    val header = 56.dp
    val drawerWidth = 322.dp
    val drawerInset = 56.dp
    val rowMin = 64.dp
    val rowMinThreeLine = 82.dp
    val composerMin = 56.dp
    val contextRow = 32.dp
    val stateDot = 6.dp
    val badgeDot = 9.dp
    val chip = 32.dp
    val filterChip = 30.dp
    val bannerAction = 34.dp
    val footerRow = 52.dp
    val newSessionRow = 46.dp
    val actionButton = 44.dp
    val stopChip = 28.dp

    /** Tablet affordance retained from the existing transcript. */
    val readableMax = 776.dp
    val sheetMax = 640.dp
    val proseMax = 600.dp
    val startFirstChildInset = 22.dp
    val toolResultMax = 220.dp
    val progressLine = 2.dp
}

object ForgeMotion {
    const val FAST_MS = 120
    const val BASE_MS = 200
    const val SLOW_MS = 320

    /** FastOutSlowIn, spelled out so this file has no Compose-animation import cycle. */
    val easing: Easing = CubicBezierEasing(0.4f, 0.0f, 0.2f, 1.0f)

    /** Session-row marquee cycle. */
    const val MARQUEE_MS = 900

    /** Drawer relative-time ticker. */
    const val TIME_TICK_MS = 30_000L

    /** Daemon resource line refresh while the drawer is open. */
    const val RESOURCE_TICK_MS = 5_000L
}

object ForgeElevation {
    /**
     * Elevation is a surface step, not a shadow, except for the drawer which
     * overlaps same-coloured content (UI-SPEC 2.5).
     */
    val drawer = 14.dp
    const val DRAWER_SHADOW_DARK = 0.45f
    const val DRAWER_SHADOW_LIGHT = 0.16f
}
