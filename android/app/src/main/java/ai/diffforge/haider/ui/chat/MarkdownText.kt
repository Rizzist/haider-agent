package ai.diffforge.haider.ui.chat

import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.ClickableText
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalUriHandler
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.dp

/** Lightweight, dependency-free Markdown renderer for daemon prose. */
@Composable
internal fun MarkdownText(
    text: String,
    showCaret: Boolean = false,
    caretAlpha: Float = 1f,
) {
    val colors = Forge.colors
    val type = Forge.type
    val blocks = remember(text) { parseMarkdownBlocks(text) }
    val rendered = if (blocks.isEmpty()) listOf(MarkdownBlock.Paragraph("")) else blocks
    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        rendered.forEachIndexed { index, block ->
            val last = index == rendered.lastIndex
            when (block) {
                is MarkdownBlock.Code -> Text(
                    text = literalCode(
                        block.text,
                        colors.ember,
                        showCaret && last,
                        caretAlpha,
                    ),
                    style = type.toolRow.copy(fontFamily = FontFamily.Monospace),
                    color = colors.chatText,
                    modifier = Modifier
                        .fillMaxWidth()
                        .horizontalScroll(rememberScrollState())
                        .clip(RoundedCornerShape(7.dp))
                        .background(colors.bgDeep)
                        .border(1.dp, colors.border, RoundedCornerShape(7.dp))
                        .padding(10.dp),
                )
                is MarkdownBlock.Quote -> InlineMarkdownLine(
                    text = block.text,
                    style = type.thinking,
                    color = colors.textMuted,
                    showCaret = showCaret && last,
                    caretAlpha = caretAlpha,
                    modifier = Modifier
                        .fillMaxWidth()
                        .drawBehind {
                            val stroke = 2.dp.toPx()
                            drawLine(
                                color = colors.ember.copy(alpha = 0.45f),
                                start = Offset(stroke / 2f, 0f),
                                end = Offset(stroke / 2f, size.height),
                                strokeWidth = stroke,
                            )
                        }
                        .padding(start = 12.dp, top = 3.dp, bottom = 3.dp),
                )
                is MarkdownBlock.Heading -> InlineMarkdownLine(
                    text = block.text,
                    style = if (block.level <= 2) type.h1 else type.h4,
                    color = colors.text,
                    showCaret = showCaret && last,
                    caretAlpha = caretAlpha,
                )
                is MarkdownBlock.ListItem -> InlineMarkdownLine(
                    text = "${block.marker} ${block.text}",
                    style = type.chatBody,
                    color = colors.chatText,
                    showCaret = showCaret && last,
                    caretAlpha = caretAlpha,
                    modifier = Modifier.padding(start = 8.dp),
                )
                is MarkdownBlock.Table -> Text(
                    text = inlineMarkdown(
                        block.rows.joinToString("\n") { row -> row.joinToString("  │  ") },
                        colors.chatText,
                        colors.accentSoft,
                        colors.ember,
                        showCaret && last,
                        caretAlpha,
                    ),
                    style = type.toolRow,
                    color = colors.chatText,
                    modifier = Modifier
                        .fillMaxWidth()
                        .horizontalScroll(rememberScrollState())
                        .clip(ForgeShapes.cardTight)
                        .background(colors.surface)
                        .border(1.dp, colors.border, ForgeShapes.cardTight)
                        .padding(9.dp),
                )
                is MarkdownBlock.Paragraph -> InlineMarkdownLine(
                    text = block.text,
                    style = type.chatBody,
                    color = colors.chatText,
                    showCaret = showCaret && last,
                    caretAlpha = caretAlpha,
                )
            }
        }
    }
}

private fun literalCode(
    text: String,
    caret: Color,
    showCaret: Boolean,
    caretAlpha: Float,
): AnnotatedString = buildAnnotatedString {
    append(text)
    if (showCaret) {
        withStyle(SpanStyle(background = caret.copy(alpha = caretAlpha.coerceIn(0f, 1f)))) {
            append(" ")
        }
    }
}

@Composable
private fun InlineMarkdownLine(
    text: String,
    style: TextStyle,
    color: Color,
    showCaret: Boolean,
    caretAlpha: Float,
    modifier: Modifier = Modifier,
) {
    val colors = Forge.colors
    val uriHandler = LocalUriHandler.current
    val annotated = inlineMarkdown(
        text,
        color,
        colors.accentSoft,
        colors.ember,
        showCaret,
        caretAlpha,
    )
    ClickableText(
        text = annotated,
        style = style.copy(color = color),
        modifier = modifier,
        onClick = { offset ->
            annotated.getStringAnnotations(URL_TAG, offset, offset)
                .firstOrNull()
                ?.item
                ?.let { url -> runCatching { uriHandler.openUri(url) } }
        },
    )
}

private fun inlineMarkdown(
    text: String,
    color: Color,
    link: Color,
    caret: Color,
    showCaret: Boolean,
    caretAlpha: Float,
): AnnotatedString = buildAnnotatedString {
    var cursor = 0
    while (cursor < text.length) {
        when {
            text.startsWith("**", cursor) -> {
                val end = text.indexOf("**", cursor + 2)
                if (end >= 0) {
                    withStyle(SpanStyle(fontWeight = FontWeight.SemiBold, color = color)) {
                        append(text.substring(cursor + 2, end))
                    }
                    cursor = end + 2
                } else {
                    append(text[cursor++])
                }
            }
            text[cursor] == '`' -> {
                val end = text.indexOf('`', cursor + 1)
                if (end >= 0) {
                    withStyle(SpanStyle(fontFamily = FontFamily.Monospace, background = color.copy(alpha = 0.09f))) {
                        append(text.substring(cursor + 1, end))
                    }
                    cursor = end + 1
                } else {
                    append(text[cursor++])
                }
            }
            text[cursor] == '[' -> {
                val labelEnd = text.indexOf(']', cursor + 1)
                val urlStart = if (labelEnd >= 0 && labelEnd + 1 < text.length && text[labelEnd + 1] == '(') {
                    labelEnd + 2
                } else {
                    -1
                }
                val urlEnd = if (urlStart >= 0) text.indexOf(')', urlStart) else -1
                if (urlEnd >= 0) {
                    pushStringAnnotation(URL_TAG, text.substring(urlStart, urlEnd))
                    withStyle(SpanStyle(color = link, textDecoration = TextDecoration.Underline)) {
                        append(text.substring(cursor + 1, labelEnd))
                    }
                    pop()
                    cursor = urlEnd + 1
                } else {
                    append(text[cursor++])
                }
            }
            text[cursor] == '*' || text[cursor] == '_' -> {
                val marker = text[cursor]
                val end = text.indexOf(marker, cursor + 1)
                if (end > cursor + 1) {
                    withStyle(SpanStyle(fontStyle = androidx.compose.ui.text.font.FontStyle.Italic)) {
                        append(text.substring(cursor + 1, end))
                    }
                    cursor = end + 1
                } else {
                    append(text[cursor++])
                }
            }
            else -> append(text[cursor++])
        }
    }
    if (showCaret) {
        withStyle(SpanStyle(background = caret.copy(alpha = caretAlpha.coerceIn(0f, 1f)))) { append(" ") }
    }
}

private sealed interface MarkdownBlock {
    data class Paragraph(val text: String) : MarkdownBlock
    data class Heading(val level: Int, val text: String) : MarkdownBlock
    data class Quote(val text: String) : MarkdownBlock
    data class ListItem(val marker: String, val text: String) : MarkdownBlock
    data class Code(val text: String) : MarkdownBlock
    data class Table(val rows: List<List<String>>) : MarkdownBlock
}

private fun parseMarkdownBlocks(source: String): List<MarkdownBlock> {
    val lines = source.replace("\r\n", "\n").split('\n')
    val blocks = mutableListOf<MarkdownBlock>()
    var index = 0
    while (index < lines.size) {
        val line = lines[index]
        when {
            line.isBlank() -> index++
            line.trimStart().startsWith("```") -> {
                index++
                val code = mutableListOf<String>()
                while (index < lines.size && !lines[index].trimStart().startsWith("```")) {
                    code += lines[index++]
                }
                if (index < lines.size) index++
                blocks += MarkdownBlock.Code(code.joinToString("\n"))
            }
            line.startsWith("#") && line.indexOfFirst { it != '#' } in 1..6 -> {
                val level = line.takeWhile { it == '#' }.length
                blocks += MarkdownBlock.Heading(level, line.drop(level).trimStart())
                index++
            }
            line.trimStart().startsWith(">") -> {
                val quote = mutableListOf<String>()
                while (index < lines.size && lines[index].trimStart().startsWith(">")) {
                    quote += lines[index++].trimStart().removePrefix(">").trimStart()
                }
                blocks += MarkdownBlock.Quote(quote.joinToString("\n"))
            }
            isTableStart(lines, index) -> {
                val rows = mutableListOf<List<String>>()
                rows += tableCells(lines[index])
                index += 2 // separator row is presentation syntax, not content
                while (index < lines.size && lines[index].contains('|') && lines[index].isNotBlank()) {
                    rows += tableCells(lines[index++])
                }
                blocks += MarkdownBlock.Table(rows)
            }
            listMarker(line) != null -> {
                val (marker, content) = listMarker(line) ?: ("•" to line)
                blocks += MarkdownBlock.ListItem(marker, content)
                index++
            }
            else -> {
                val paragraph = mutableListOf<String>()
                while (index < lines.size && lines[index].isNotBlank() && !startsBlock(lines, index)) {
                    paragraph += lines[index++]
                }
                if (paragraph.isEmpty()) paragraph += lines[index++]
                blocks += MarkdownBlock.Paragraph(paragraph.joinToString("\n"))
            }
        }
    }
    return blocks
}

private fun startsBlock(lines: List<String>, index: Int): Boolean {
    val line = lines[index]
    return line.trimStart().startsWith("```") ||
        line.trimStart().startsWith(">") ||
        (line.startsWith("#") && line.indexOfFirst { it != '#' } in 1..6) ||
        listMarker(line) != null ||
        isTableStart(lines, index)
}

private fun listMarker(line: String): Pair<String, String>? {
    val trimmed = line.trimStart()
    if (trimmed.startsWith("- ") || trimmed.startsWith("* ") || trimmed.startsWith("+ ")) {
        return "•" to trimmed.drop(2)
    }
    val dot = trimmed.indexOf('.')
    if (dot in 1..4 && trimmed.substring(0, dot).all { it.isDigit() } && trimmed.getOrNull(dot + 1) == ' ') {
        return trimmed.substring(0, dot + 1) to trimmed.drop(dot + 2)
    }
    return null
}

private fun isTableStart(lines: List<String>, index: Int): Boolean {
    if (index + 1 >= lines.size || !lines[index].contains('|')) return false
    val separator = lines[index + 1].trim().trim('|').split('|')
    return separator.isNotEmpty() && separator.all { cell ->
        cell.trim().trim(':').let { value -> value.length >= 3 && value.all { it == '-' } }
    }
}

private fun tableCells(line: String): List<String> =
    line.trim().trim('|').split('|').map { it.trim() }

private const val URL_TAG = "url"
