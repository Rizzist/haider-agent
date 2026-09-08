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
    /** Hairline on tinted accent surfaces (reference `accentLine`). */
    val accentLine: Color,
    /** 2 dp outline on focus / keyboard navigation. */
    val focusRing: Color,
    /** Drawer scrim. */
    val scrim: Color,
    /** Markdown links only. */
    val link: Color,
    val amber: Color,
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

/**
 * next-diffforge forge dark (owner addition H4).
 *
 * Surfaces and text tiers come from `LiveAppDemo.js:41-72`; the accent family
 * is the dashboard's blue selection set (`selBg` / `selBorder` / `selRing`),
 * replacing 971's ember. State tones follow `modelBrand.jsx`: green while a
 * turn runs, amber while it waits for a human, red on error.
 */
val ForgeDark = ForgeColors(
    bg = Color(0xFF07090D),
    bgDeep = Color(0xFF020304),
    surface = Color(0xFF0D1117),
    surfaceRaised = Color(0xFF11161D),
    surfaceControl = Color(0xFF151B23),
    surfaceHover = Color(0x0EE6ECF5), // rgba(230,236,245,0.055)
    surfaceSelected = Color(0x143B82F6), // selBg rgba(59,130,246,0.08)
    border = Color(0x1AE6ECF5), // rgba(230,236,245,0.10)
    borderStrong = Color(0x29E6ECF5), // rgba(230,236,245,0.16)
    text = Color(0xFFF4F7FA),
    chatText = Color(0xFFE8EEF8),
    textSoft = Color(0xFFB6C0CC),
    textMuted = Color(0xFF7A8493),
    textDisabled = Color(0xFF505966),
    accent = Color(0xFF3B82F6),
    accentSoft = Color(0xFF7DB0FF),
    // Near-black on a solid #3B82F6 fill measures 4.9:1; white measures 3.7:1
    // and would fail the same table that caught the ember pair in round 1.
    accentInk = Color(0xFF060B12),
    accentWash = Color(0x143B82F6), // selBg
    accentLine = Color(0x807DB0FF), // selBorder rgba(125,176,255,0.5)
    focusRing = Color(0x947DB0FF), // rgba(125,176,255,0.58)
    scrim = Color(0xA8020304),
    link = Color(0xFF7DB0FF),
    amber = Color(0xFFDFA55A),
    green = Color(0xFF3CCB7F),
    red = Color(0xFFEF6B6B),
    trajectoryModel = Color(0xFFB795F6),
    stateRunning = Color(0xFF3CCB7F),
    stateNeedsInput = Color(0xFFDFA55A),
    stateIdle = Color(0xFF7A8493),
    stateErrored = Color(0xFFEF6B6B),
    stateUnknown = Color(0xFF7A8493),
    isDark = true,
)

/**
 * next-diffforge forge light (`tokens.js` THEME_VARS.light).
 *
 * As in round 1, "soft" means *darker* in a light theme: the soft accent is
 * the reference's `--df-accent-text`, so it clears 4.5:1 on the wash as well
 * as on the ground.
 */
val ForgeLight = ForgeColors(
    bg = Color(0xFFF4F6FB),
    bgDeep = Color(0xFFEEF1F7),
    surface = Color(0xFFFFFFFF),
    surfaceRaised = Color(0xFFFFFFFF),
    surfaceControl = Color(0xFFE9EDF5),
    surfaceHover = Color(0x0A0F172A),
    surfaceSelected = Color(0x1A1A56C4),
    border = Color(0x1F0F172A), // rgba(15,23,42,0.12)
    borderStrong = Color(0x380F172A), // rgba(15,23,42,0.22)
    text = Color(0xFF0B1420),
    chatText = Color(0xFF16202E),
    textSoft = Color(0xFF46556B),
    textMuted = Color(0xFF5F6B80),
    textDisabled = Color(0xFF94A3B8),
    accent = Color(0xFF1A56C4),
    accentSoft = Color(0xFF123F92),
    accentInk = Color(0xFFFFFFFF),
    accentWash = Color(0x1A1A56C4),
    accentLine = Color(0x521A56C4),
    focusRing = Color(0x731A56C4),
    scrim = Color(0x6B0F172A),
    link = Color(0xFF123F92),
    amber = Color(0xFF9A5B00),
    green = Color(0xFF15803D),
    red = Color(0xFFC02626),
    trajectoryModel = Color(0xFF6D5AE0),
    stateRunning = Color(0xFF15803D),
    stateNeedsInput = Color(0xFF9A5B00),
    stateIdle = Color(0xFF5F6B80),
    stateErrored = Color(0xFFC02626),
    stateUnknown = Color(0xFF5F6B80),
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
    val sessionTitle: TextStyle,
    val sessionMeta: TextStyle,
    val drawerSection: TextStyle,
    val button: TextStyle,
    val banner: TextStyle,
    val numeric: TextStyle,
    /** The letter inside a brand tile. */
    val brandLetter: TextStyle,
    /** The tiny tracked label inside a composer select (dashboard.js:39694). */
    val selectLabel: TextStyle,
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
    brandLetter = TextStyle(
        fontFamily = FontFamily.Default,
        fontSize = 9.sp,
        lineHeight = 10.sp,
        fontWeight = FontWeight.Bold,
    ),
    selectLabel = TextStyle(
        fontFamily = FontFamily.Default,
        fontSize = 9.sp,
        lineHeight = 11.sp,
        fontWeight = FontWeight.Black,
        letterSpacing = 0.9.sp,
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
