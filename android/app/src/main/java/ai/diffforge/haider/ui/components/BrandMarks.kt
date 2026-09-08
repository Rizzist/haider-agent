package ai.diffforge.haider.ui.components

import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.PathFillType
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.graphics.vector.PathParser
import androidx.compose.ui.graphics.vector.path
import ai.diffforge.haider.ui.theme.ForgeSize

/**
 * The vendor marks, as ImageVectors on a 24x24 viewport.
 *
 * Ported from the desktop client's `modelBrand.jsx`, which is explicit about
 * the provenance rule and it is kept here: a real vendor path ships only where
 * one is actually available. OpenAI is the simple-icons path, DeepSeek is that
 * project's official whale, and Claude/Gemini/Grok use the vendors' own
 * published geometry (Anthropic's sunburst, Gemini's four-point spark, xAI's
 * slash). Everything else gets a neutral coloured letter tile — a made-up logo
 * for a brand we have no mark for would be worse than a letter.
 */
private fun brandVector(name: String, build: ImageVector.Builder.() -> Unit): ImageVector =
    ImageVector.Builder(
        name = name,
        defaultWidth = ForgeSize.markViewport,
        defaultHeight = ForgeSize.markViewport,
        viewportWidth = 24f,
        viewportHeight = 24f,
    ).apply(build).build()

private fun ImageVector.Builder.solid(pathData: String) {
    addPath(
        pathData = PathParser().parsePathString(pathData).toNodes(),
        fill = SolidColor(Color.White),
        pathFillType = PathFillType.NonZero,
    )
}

/** simple-icons OpenAI. */
val OpenAiMark: ImageVector = brandVector("OpenAi") {
    solid("M22.282 9.821a5.985 5.985 0 0 0-.516-4.91 6.046 6.046 0 0 0-6.51-2.9A6.065 6.065 0 0 0 4.981 4.18a5.985 5.985 0 0 0-3.998 2.9 6.046 6.046 0 0 0 .743 7.097 5.98 5.98 0 0 0 .51 4.911 6.051 6.051 0 0 0 6.515 2.9A5.985 5.985 0 0 0 13.26 24a6.056 6.056 0 0 0 5.772-4.206 5.99 5.99 0 0 0 3.997-2.9 6.056 6.056 0 0 0-.747-7.073zM13.26 22.43a4.476 4.476 0 0 1-2.876-1.04l.141-.081 4.779-2.758a.795.795 0 0 0 .392-.681v-6.737l2.02 1.168a.071.071 0 0 1 .038.052v5.583a4.504 4.504 0 0 1-4.494 4.494zM3.6 18.304a4.47 4.47 0 0 1-.535-3.014l.142.085 4.783 2.759a.771.771 0 0 0 .78 0l5.843-3.369v2.332a.08.08 0 0 1-.033.062L9.74 19.95a4.5 4.5 0 0 1-6.14-1.646zM2.34 7.896a4.485 4.485 0 0 1 2.366-1.973V11.6a.766.766 0 0 0 .388.676l5.815 3.355-2.02 1.168a.076.076 0 0 1-.071 0l-4.83-2.786A4.504 4.504 0 0 1 2.34 7.872zm16.597 3.855-5.833-3.387L15.119 7.2a.076.076 0 0 1 .071 0l4.83 2.791a4.494 4.494 0 0 1-.676 8.105v-5.678a.79.79 0 0 0-.407-.667zm2.01-3.023-.141-.085-4.774-2.782a.776.776 0 0 0-.785 0L9.409 9.23V6.897a.066.066 0 0 1 .028-.061l4.83-2.787a4.5 4.5 0 0 1 6.68 4.66zm-12.64 4.135-2.02-1.164a.08.08 0 0 1-.038-.057V6.075a4.5 4.5 0 0 1 7.375-3.453l-.142.08-4.778 2.758a.795.795 0 0 0-.393.681zm1.097-2.365 2.602-1.5 2.607 1.5v2.999l-2.597 1.5-2.607-1.5Z")
}

/** simple-icons DeepSeek whale. */
val DeepSeekMark: ImageVector = brandVector("DeepSeek") {
    solid("M23.748 4.651c-.254-.124-.364.113-.512.233-.051.04-.094.09-.137.137-.372.397-.806.657-1.373.626-.829-.046-1.537.214-2.163.848-.133-.782-.575-1.248-1.247-1.548-.352-.155-.708-.311-.955-.65-.172-.24-.219-.509-.305-.774-.055-.16-.11-.323-.293-.35-.2-.031-.278.136-.356.276-.313.572-.434 1.202-.422 1.84.027 1.436.633 2.58 1.838 3.393.137.094.172.187.129.323-.082.28-.18.553-.266.833-.055.179-.137.218-.328.14a5.5 5.5 0 0 1-1.737-1.179c-.857-.828-1.631-1.743-2.597-2.46a12 12 0 0 0-.689-.47c-.985-.957.13-1.743.387-1.836.27-.098.094-.433-.778-.428-.872.003-1.67.295-2.687.685a3 3 0 0 1-.465.136 9.6 9.6 0 0 0-2.883-.101c-1.885.21-3.39 1.1-4.497 2.622C.082 8.776-.231 10.854.152 13.02c.403 2.284 1.568 4.175 3.36 5.653 1.857 1.533 3.997 2.284 6.438 2.14 1.482-.085 3.132-.284 4.994-1.86.47.234.962.328 1.78.398.629.058 1.235-.031 1.705-.129.735-.155.684-.836.418-.961-2.155-1.004-1.682-.595-2.112-.926 1.095-1.295 2.768-3.598 3.284-6.733.05-.346.115-.834.108-1.114-.004-.171.035-.238.23-.257a4.2 4.2 0 0 0 1.545-.475c1.397-.763 1.96-2.016 2.093-3.517.02-.23-.004-.467-.247-.588M11.58 18.168c-2.088-1.642-3.101-2.183-3.52-2.16-.39.024-.32.472-.234.763.09.288.207.487.371.74.114.167.192.416-.113.603-.673.416-1.842-.14-1.897-.168-1.361-.801-2.5-1.86-3.301-3.306-.775-1.393-1.225-2.888-1.299-4.482-.02-.385.094-.522.477-.592a4.7 4.7 0 0 1 1.53-.038c2.131.311 3.946 1.264 5.467 2.774.868.86 1.525 1.887 2.202 2.89.72 1.066 1.494 2.082 2.48 2.915.348.291.626.513.892.677-.802.09-2.14.109-3.055-.615zm1.001-6.44a.306.306 0 0 1 .415-.287.3.3 0 0 1 .113.074.3.3 0 0 1 .086.214c0 .17-.136.307-.308.307a.303.303 0 0 1-.306-.307m3.11 1.596c-.2.081-.4.151-.591.16a1.25 1.25 0 0 1-.798-.254c-.274-.23-.47-.358-.551-.758a1.7 1.7 0 0 1 .015-.588c.07-.327-.007-.537-.238-.727-.188-.156-.426-.199-.689-.199a.6.6 0 0 1-.254-.078.253.253 0 0 1-.114-.358 1 1 0 0 1 .192-.21c.356-.202.767-.136 1.146.016.352.144.618.408 1.001.782.392.451.462.576.685.915.176.264.336.536.446.848.066.194-.02.353-.25.45")
}

/**
 * Anthropic's radiating spark: twelve tapered rays around a core.
 *
 * The JS renders one `<path>` twelve times under `rotate(i*30 12 12)`;
 * ImageVector has no transform on a bare path, so the twelve rays are baked
 * out here at the same 30-degree steps about (12,12).
 */
val ClaudeMark: ImageVector = brandVector("Claude") {
    val ray = listOf(12f to 2.4f, 13.35f to 8.9f, 12f to 11f, 10.65f to 8.9f)
    repeat(12) { index ->
        val radians = Math.toRadians(index * 30.0)
        val cos = kotlin.math.cos(radians).toFloat()
        val sin = kotlin.math.sin(radians).toFloat()
        val points = ray.map { (x, y) ->
            val dx = x - 12f
            val dy = y - 12f
            (12f + dx * cos - dy * sin) to (12f + dx * sin + dy * cos)
        }
        path(fill = SolidColor(Color.White)) {
            moveTo(points[0].first, points[0].second)
            points.drop(1).forEach { lineTo(it.first, it.second) }
            close()
        }
    }
    path(fill = SolidColor(Color.White)) {
        // A circle of r=2.6 at (12,12), as four quadrant arcs.
        moveTo(14.6f, 12f)
        arcToRelative(2.6f, 2.6f, 0f, isMoreThanHalf = true, isPositiveArc = true, -5.2f, 0f)
        arcToRelative(2.6f, 2.6f, 0f, isMoreThanHalf = true, isPositiveArc = true, 5.2f, 0f)
        close()
    }
}

/** Gemini's four-point curved sparkle. */
val GeminiMark: ImageVector = brandVector("Gemini") {
    solid(
        "M12 0C12 6.627 6.627 12 0 12c6.627 0 12 5.373 12 12 " +
            "0-6.627 5.373-12 12-12-6.627 0-12-5.373-12-12z",
    )
}

/** xAI's diagonal strike. */
val GrokMark: ImageVector = brandVector("Grok") {
    solid("M4.2 4.2h3.4L20 19.8h-3.4z")
    solid("M20 4.2 12.9 12l-1.7-2.1 5.4-5.7z")
    solid("M4 19.8l5.2-5.5 1.7 2.1-3.5 3.4z")
}
