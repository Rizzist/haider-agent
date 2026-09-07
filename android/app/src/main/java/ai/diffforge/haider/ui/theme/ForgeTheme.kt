package ai.diffforge.haider.ui.theme

import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.Immutable
import androidx.compose.runtime.ProvidableCompositionLocal
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

/**
 * Session Deck design tokens, ported from rust-diffforge @ haider-rewrite
 * (`src/app/appStyles.js` `--forge-*`). Dark is primary. Android sizes are the
 * desktop-dense scale bumped ~1.15x for touch legibility, ratios preserved.
 *
 * 971 divergence (UI-SPEC 2.1): on Android the accent family is the ember
 * family. A phone has room for exactly one accent; blue survives only as
 * [ForgeColors.link] for markdown links.
 */
@Immutable
data class ForgeColors(
    val bg: Color,
    val bgDeep: Color,
    val surface: Color,
    val surfaceRaised: Color,
    val surfaceControl: Color,
    val surfaceHover: Color,
    val surfaceSelected: Color,
    val border: Color,
    val borderStrong: Color,
    val text: Color,
    val chatText: Color,
    val textSoft: Color,
    val textMuted: Color,
    val textDisabled: Color,
    val accent: Color,
    val accentSoft: Color,
    /** Ink on a FILLED accent surface. Never Color.White in dark. */
    val accentInk: Color,
    /** Tinted fills, chips, step markers. */
    val accentWash: Color,
    /** 2 dp outline on focus / keyboard navigation. */
    val focusRing: Color,
    /** Drawer scrim. */
    val scrim: Color,
    /** Markdown links only. */
    val link: Color,
    val amber: Color,
    val ember: Color,
    val green: Color,
    val red: Color,
    val trajectoryModel: Color,
    val stateRunning: Color,
    val stateNeedsInput: Color,
    val stateIdle: Color,
    val stateErrored: Color,
    /** Contradictory coordinates stay neutral (sessionActivity.js:91-94). */
    val stateUnknown: Color,
    val isDark: Boolean,
)

val ForgeDark = ForgeColors(
    bg = Color(0xFF07090D),
    bgDeep = Color(0xFF020304),
    surface = Color(0xFF0D1117),
    surfaceRaised = Color(0xFF11161D),
    surfaceControl = Color(0xFF151B23),
    surfaceHover = Color(0x0EE6ECF5), // rgba(230,236,245,0.055)
    surfaceSelected = Color(0x1FE8873A), // rgba(232,135,58,0.12)
    border = Color(0x1AE6ECF5), // rgba(230,236,245,0.10)
    borderStrong = Color(0x29E6ECF5), // rgba(230,236,245,0.16)
    text = Color(0xFFF4F7FA),
    chatText = Color(0xFFD6DEE8),
    textSoft = Color(0xFFB6C0CC),
    textMuted = Color(0xFF7A8493),
    textDisabled = Color(0xFF505966),
    accent = Color(0xFFE8873A),
    accentSoft = Color(0xFFFFA55E),
    accentInk = Color(0xFF0B0B0C),
    accentWash = Color(0x24E8873A),
    focusRing = Color(0x8CE8873A),
    scrim = Color(0xA8020304),
    link = Color(0xFF7DB0FF),
    amber = Color(0xFFDFA55A),
    ember = Color(0xFFE8873A),
    green = Color(0xFF3CCB7F),
    red = Color(0xFFEF6B6B),
    trajectoryModel = Color(0xFF8B7CF6),
    stateRunning = Color(0xFFDFA55A),
    stateNeedsInput = Color(0xFFE8873A),
    stateIdle = Color(0xFF7A8493),
    stateErrored = Color(0xFFEF6B6B),
    stateUnknown = Color(0xFF7A8493),
    isDark = true,
)

val ForgeLight = ForgeColors(
    bg = Color(0xFFF5F5F7),
    bgDeep = Color(0xFFECECEF),
    surface = Color(0xFFFFFFFF),
    surfaceRaised = Color(0xFFFFFFFF),
    surfaceControl = Color(0xFFFAFAFC),
    surfaceHover = Color(0x0A000000),
    surfaceSelected = Color(0x1A9A4B08),
    border = Color(0x14000000), // rgba(0,0,0,0.08)
    borderStrong = Color(0x24000000), // rgba(0,0,0,0.14)
    text = Color(0xFF1D1D1F),
    chatText = Color(0xFF2B2B2F),
    textSoft = Color(0xFF333333),
    textMuted = Color(0xFF6B6B6B),
    textDisabled = Color(0xFFA1A1A6),
    // UI-SPEC 2.1 proposed accent #B45309 / accentSoft #C2621A. Its contrast
    // table measured the accent only against the plain ground (5.05:1) and
    // never against accentWash, where the same pair is 4.02:1, nor accentSoft
    // at all, which is 3.81:1. Both are darkened here so every documented pair
    // clears 4.5:1 — see ContrastTest. In a light theme "soft" means darker.
    accent = Color(0xFF9A4B08),
    accentSoft = Color(0xFF7C2D12),
    accentInk = Color(0xFFFFFFFF),
    accentWash = Color(0x1A9A4B08),
    focusRing = Color(0x8C9A4B08),
    scrim = Color(0x6B141416),
    link = Color(0xFF0066CC),
    amber = Color(0xFF8B5A00),
    ember = Color(0xFF9A4B08),
    green = Color(0xFF0A7F45),
    red = Color(0xFFB42318),
    trajectoryModel = Color(0xFF6D5AE0),
    stateRunning = Color(0xFF8B5A00),
    stateNeedsInput = Color(0xFF9A4B08),
    stateIdle = Color(0xFF6B6B6B),
    stateErrored = Color(0xFFB42318),
    stateUnknown = Color(0xFF6B6B6B),
    isDark = false,
)

/** Typography roles. Inter/Roboto Flex substitute = system default for now;
 *  JetBrains/Roboto Mono substitute = FontFamily.Monospace. Real variable fonts
 *  are a follow-up (needs bundled assets). */
@Immutable
data class ForgeType(
    val chatBody: TextStyle,
    val userBody: TextStyle,
    val thinking: TextStyle,
    val toolRow: TextStyle,
    val toolStrong: TextStyle,
    val chip: TextStyle,
    val label: TextStyle,
    val h1: TextStyle,
    val h4: TextStyle,
    val statusPill: TextStyle,
    val sessionTitle: TextStyle,
    val sessionMeta: TextStyle,
    val drawerSection: TextStyle,
    val button: TextStyle,
    val banner: TextStyle,
    val numeric: TextStyle,
)

val ForgeTypography = ForgeType(
    chatBody = TextStyle(fontFamily = FontFamily.Default, fontSize = 15.5.sp, lineHeight = 26.sp, fontWeight = FontWeight.Normal),
    userBody = TextStyle(fontFamily = FontFamily.Default, fontSize = 15.sp, lineHeight = 24.sp, fontWeight = FontWeight.Medium),
    thinking = TextStyle(fontFamily = FontFamily.Default, fontSize = 14.sp, lineHeight = 22.sp, fontWeight = FontWeight.Normal),
    toolRow = TextStyle(fontFamily = FontFamily.Monospace, fontSize = 12.sp, lineHeight = 18.sp),
    toolStrong = TextStyle(fontFamily = FontFamily.Monospace, fontSize = 12.sp, lineHeight = 18.sp, fontWeight = FontWeight.SemiBold),
    chip = TextStyle(fontFamily = FontFamily.Default, fontSize = 11.sp, lineHeight = 14.sp, fontWeight = FontWeight.Medium),
    label = TextStyle(fontFamily = FontFamily.Default, fontSize = 10.sp, lineHeight = 13.sp, fontWeight = FontWeight.SemiBold, letterSpacing = 0.6.sp),
    h1 = TextStyle(fontFamily = FontFamily.Default, fontSize = 19.sp, lineHeight = 24.sp, fontWeight = FontWeight.SemiBold),
    h4 = TextStyle(fontFamily = FontFamily.Default, fontSize = 16.sp, lineHeight = 21.sp, fontWeight = FontWeight.SemiBold),
    statusPill = TextStyle(fontFamily = FontFamily.Default, fontSize = 11.sp, lineHeight = 13.sp, fontWeight = FontWeight.Bold),
    sessionTitle = TextStyle(fontFamily = FontFamily.Default, fontSize = 15.sp, lineHeight = 20.sp, fontWeight = FontWeight.Medium),
    sessionMeta = TextStyle(fontFamily = FontFamily.Default, fontSize = 12.sp, lineHeight = 16.sp, fontWeight = FontWeight.Normal),
    drawerSection = TextStyle(
        fontFamily = FontFamily.Default,
        fontSize = 10.5.sp,
        lineHeight = 14.sp,
        fontWeight = FontWeight.Bold,
        letterSpacing = 0.9.sp,
    ),
    button = TextStyle(fontFamily = FontFamily.Default, fontSize = 14.5.sp, lineHeight = 18.sp, fontWeight = FontWeight.SemiBold),
    banner = TextStyle(fontFamily = FontFamily.Default, fontSize = 13.sp, lineHeight = 18.sp, fontWeight = FontWeight.SemiBold),
    numeric = TextStyle(
        fontFamily = FontFamily.Monospace,
        fontSize = 12.sp,
        lineHeight = 16.sp,
        fontWeight = FontWeight.Medium,
    ),
)

/** Shape tokens: pills at 999, cards 8-14, sheets 20. */
object ForgeShapes {
    val pill = RoundedCornerShape(999.dp)
    val card = RoundedCornerShape(12.dp)
    val cardTight = RoundedCornerShape(8.dp)
    val cardWide = RoundedCornerShape(14.dp)
    val composer = RoundedCornerShape(26.dp)
    val row = RoundedCornerShape(10.dp)
    val sheet = RoundedCornerShape(topStart = 20.dp, topEnd = 20.dp)
    val quote = RoundedCornerShape(6.dp)

    /** The asymmetric user bubble; kept verbatim from the 970 transcript. */
    val userBubble = RoundedCornerShape(
        topStart = 14.dp,
        topEnd = 14.dp,
        bottomStart = 14.dp,
        bottomEnd = 5.dp,
    )
}

val LocalForgeColors: ProvidableCompositionLocal<ForgeColors> =
    staticCompositionLocalOf { ForgeDark }
val LocalForgeType: ProvidableCompositionLocal<ForgeType> =
    staticCompositionLocalOf { ForgeTypography }

object Forge {
    val colors: ForgeColors
        @Composable get() = LocalForgeColors.current
    val type: ForgeType
        @Composable get() = LocalForgeType.current
}

@Composable
fun ForgeTheme(
    dark: Boolean = isSystemInDarkTheme(),
    content: @Composable () -> Unit,
) {
    val colors = if (dark) ForgeDark else ForgeLight
    CompositionLocalProvider(
        LocalForgeColors provides colors,
        LocalForgeType provides ForgeTypography,
        content = content,
    )
}
