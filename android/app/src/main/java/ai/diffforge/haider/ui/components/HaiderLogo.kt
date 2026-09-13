package ai.diffforge.haider.ui.components

import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.LogoStyle
import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.width
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.PathFillType
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.graphics.vector.PathParser
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.unit.Dp

/**
 * The Haider Code wordmark: the Arabic-calligraphy pixel grid from
 * haidercode-web `site/logo.svg` (64x23, `crispEdges`), converted rect-for-rect
 * — every `<rect>` is one `M x,y h w v 1 h -w z` subpath, so the geometry is
 * the site's exactly.
 *
 * The site ships one colourway, drawn for dark grounds; that is the DARK
 * variant here. The LIGHT variant keeps the geometry and darkens both oranges
 * so the mark clears 3:1 on the light theme's white surface (pinned in
 * LogoStyleTest) while staying the brand's hue.
 */
object HaiderLogoPalette {
    /** The site's palette (logo.svg), for dark grounds. */
    val darkPrimary = Color(0xFFE0642F)
    val darkSecondary = Color(0xFFEF9A6D)

    /** Contrast-adjusted for light grounds: 5.4:1 and 3.6:1 on white. */
    val lightPrimary = Color(0xFFB34A1E)
    val lightSecondary = Color(0xFFC9713F)
}

/** The wordmark's 64:23 pixel grid, as width over height. */
private const val LOGO_ASPECT = 64f / 23f

/** The main glyph (site fill #e0642f), 51 rects. */
private const val GLYPH =
    "M24,0h1v1h-1zM23,1h3v1h-3zM37,1h1v1h-1zM23,2h3v1h-3zM36,2h2v1h-2z" +
        "M47,2h6v1h-6zM22,3h5v1h-5zM35,3h4v1h-4zM46,3h9v1h-9zM10,4h3v1h-3z" +
        "M22,4h5v1h-5zM35,4h5v1h-5zM45,4h11v1h-11zM10,5h3v1h-3zM23,5h5v1h-5z" +
        "M34,5h6v1h-6zM45,5h12v1h-12zM9,6h5v1h-5zM23,6h5v1h-5zM35,6h5v1h-5z" +
        "M44,6h3v1h-3zM50,6h9v1h-9zM9,7h6v1h-6zM24,7h4v1h-4zM35,7h5v1h-5z" +
        "M52,7h8v1h-8zM9,8h6v1h-6zM17,8h1v1h-1zM24,8h4v1h-4zM36,8h5v1h-5z" +
        "M53,8h8v1h-8zM10,9h5v1h-5zM16,9h3v1h-3zM24,9h39v1h-39zM11,10h4v1h-4z" +
        "M16,10h3v1h-3zM22,10h42v1h-42zM11,11h4v1h-4zM16,11h48v1h-48zM12,12h3v1h-3z" +
        "M16,12h48v1h-48zM12,13h3v1h-3zM16,13h48v1h-48zM11,14h4v1h-4zM16,14h47v1h-47z" +
        "M10,15h5v1h-5zM17,15h8v1h-8zM0,16h2v1h-2zM9,16h6v1h-6zM31,16h2v1h-2z" +
        "M36,16h1v1h-1z"

/** The flourish under it (site fill #ef9a6d), 14 rects. */
private const val FLOURISH =
    "M0,17h4v1h-4zM7,17h7v1h-7zM31,17h3v1h-3zM35,17h3v1h-3zM1,18h13v1h-13z" +
        "M30,18h9v1h-9zM1,19h12v1h-12zM30,19h9v1h-9zM2,20h10v1h-10zM31,20h8v1h-8z" +
        "M3,21h8v1h-8zM32,21h1v1h-1zM36,21h2v1h-2zM4,22h6v1h-6z"

private fun logoVector(name: String, primary: Color, secondary: Color): ImageVector =
    ImageVector.Builder(
        name = name,
        defaultWidth = ForgeSize.logoViewportWidth,
        defaultHeight = ForgeSize.logoViewportHeight,
        viewportWidth = 64f,
        viewportHeight = 23f,
    ).apply {
        addPath(
            pathData = PathParser().parsePathString(GLYPH).toNodes(),
            fill = SolidColor(primary),
            pathFillType = PathFillType.NonZero,
        )
        addPath(
            pathData = PathParser().parsePathString(FLOURISH).toNodes(),
            fill = SolidColor(secondary),
            pathFillType = PathFillType.NonZero,
        )
    }.build()

val HaiderLogoDark: ImageVector = logoVector(
    "HaiderLogoDark",
    HaiderLogoPalette.darkPrimary,
    HaiderLogoPalette.darkSecondary,
)

val HaiderLogoLight: ImageVector = logoVector(
    "HaiderLogoLight",
    HaiderLogoPalette.lightPrimary,
    HaiderLogoPalette.lightSecondary,
)

/** Which variant actually rendered, for tests: the mark itself is silent. */
const val HAIDER_LOGO_DARK_TAG = "haider_logo_dark"
const val HAIDER_LOGO_LIGHT_TAG = "haider_logo_light"

/**
 * The wordmark at a given [width]; height follows the 64:23 grid. Decorative,
 * so it is silenced like every brand tile (UI-SPEC 4.2).
 */
@Composable
fun HaiderLogo(style: LogoStyle, width: Dp, modifier: Modifier = Modifier) {
    val darkVariant = style.resolvesToDark(Forge.colors.isDark)
    Image(
        imageVector = if (darkVariant) HaiderLogoDark else HaiderLogoLight,
        contentDescription = null,
        modifier = modifier
            .width(width)
            .aspectRatio(LOGO_ASPECT)
            .testTag(if (darkVariant) HAIDER_LOGO_DARK_TAG else HAIDER_LOGO_LIGHT_TAG),
    )
}
