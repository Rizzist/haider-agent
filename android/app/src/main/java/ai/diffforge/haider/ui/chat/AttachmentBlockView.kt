package ai.diffforge.haider.ui.chat

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.components.ForgeIconButton
import ai.diffforge.haider.ui.daemon.Attachment
import ai.diffforge.haider.ui.daemon.DaemonService
import ai.diffforge.haider.ui.daemon.TokenUsage
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.Close
import androidx.compose.material.icons.rounded.Description
import androidx.compose.material.icons.rounded.PictureAsPdf
import androidx.compose.material.icons.rounded.ShortText
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextOverflow

const val ATTACHMENT_STRIP_TAG = "attachment_strip"
const val USAGE_LINE_TAG = "usage_line"

/**
 * The media and file blocks a turn carried.
 *
 * An `AttachmentBlock` is a CAS reference, never bytes, so a thumbnail is a
 * fetch: [DaemonService.attachmentBytes] by artifact. A fetch that comes back
 * empty draws the tile with its mime and no picture rather than a broken
 * image — the reference is still true even when the bytes are gone.
 */
@OptIn(ExperimentalLayoutApi::class)
@Composable
fun AttachmentStrip(
    attachments: List<Attachment>,
    service: DaemonService?,
    modifier: Modifier = Modifier,
    /**
     * Non-null in the composer, null in the transcript.
     *
     * A staged block had no way off the strip at all — the callback existed and
     * was never wired — which also left a `too_many_attachments` refusal with
     * no way out but abandoning the message (verify-11 O9).
     */
    onRemove: ((String) -> Unit)? = null,
) {
    if (attachments.isEmpty()) return
    FlowRow(
        modifier = modifier.testTag(ATTACHMENT_STRIP_TAG),
        horizontalArrangement = Arrangement.spacedBy(ForgeSpace.sm),
        verticalArrangement = Arrangement.spacedBy(ForgeSpace.sm),
    ) {
        attachments.forEach { attachment ->
            AttachmentTile(attachment = attachment, onRemove = onRemove) {
            when (attachment) {
                is Attachment.Image -> ImageTile(attachment, service)
                is Attachment.TextFile -> FileTile(
                    icon = Icons.Rounded.Description,
                    title = attachment.name,
                    detail = "${attachment.lines} lines",
                )
                is Attachment.PastedText -> FileTile(
                    icon = Icons.Rounded.ShortText,
                    title = "Pasted text",
                    detail = "${attachment.lines} lines",
                )
                is Attachment.Pdf -> FileTile(
                    icon = Icons.Rounded.PictureAsPdf,
                    title = attachment.name,
                    detail = "${attachment.pages} pages",
                )
                is Attachment.Unsupported -> FileTile(
                    icon = Icons.Rounded.Description,
                    // Named, not hidden: the turn really did carry this.
                    title = attachment.kind,
                    detail = "not shown here",
                )
            }
            }
        }
    }
}

/**
 * One tile, with a removal control when the strip is editable.
 *
 * The control is its own 48 dp target beside the tile rather than a small cross
 * on top of it: an overlay cross either breaks the target rule or covers the
 * thumbnail it is meant to describe.
 */
@Composable
private fun AttachmentTile(
    attachment: Attachment,
    onRemove: ((String) -> Unit)?,
    content: @Composable () -> Unit,
) {
    if (onRemove == null) {
        content()
        return
    }
    val colors = Forge.colors
    Row(
        verticalAlignment = Alignment.CenterVertically,
        modifier = Modifier.testTag(attachmentTileTag(attachment.artifact)),
    ) {
        content()
        ForgeIconButton(
            onClick = { onRemove(attachment.artifact) },
            contentDescription = stringResource(R.string.cd_remove_attachment),
            visual = ForgeSize.composerCircle,
        ) {
            Icon(
                Icons.Rounded.Close,
                contentDescription = null,
                tint = colors.textMuted,
                modifier = Modifier.size(ForgeSize.iconXs),
            )
        }
    }
}

/** Per-attachment handle, so a pin can name the one it means. */
fun attachmentTileTag(artifact: String): String = "attachment_tile_$artifact"

@Composable
private fun ImageTile(image: Attachment.Image, service: DaemonService?) {
    val colors = Forge.colors
    val type = Forge.type
    var bitmap by remember(image.artifact) { mutableStateOf<ImageBitmap?>(null) }
    LaunchedEffect(image.artifact, service) {
        val bytes = service?.attachmentBytes(image.artifact) ?: return@LaunchedEffect
        bitmap = runCatching {
            android.graphics.BitmapFactory
                .decodeByteArray(bytes, 0, bytes.size)
                ?.asImageBitmap()
        }.getOrNull()
    }
    Box(
        modifier = Modifier
            .size(ForgeSize.thumbnail)
            .clip(ForgeShapes.cardTight)
            .background(colors.surfaceControl)
            .border(ForgeSize.hairline, colors.border, ForgeShapes.cardTight),
        contentAlignment = Alignment.Center,
    ) {
        val picture = bitmap
        if (picture != null) {
            Image(
                bitmap = picture,
                contentDescription = "Attached image",
                contentScale = ContentScale.Crop,
                modifier = Modifier.size(ForgeSize.thumbnail),
            )
        } else {
            Text(
                image.mime.substringAfter('/').uppercase().take(4),
                style = type.selectLabel,
                color = colors.textMuted,
            )
        }
    }
}

@Composable
private fun FileTile(
    icon: androidx.compose.ui.graphics.vector.ImageVector,
    title: String,
    detail: String,
) {
    val colors = Forge.colors
    val type = Forge.type
    Row(
        modifier = Modifier
            .height(ForgeSize.chip)
            .clip(ForgeShapes.pill)
            .background(colors.surfaceControl)
            .border(ForgeSize.hairline, colors.border, ForgeShapes.pill)
            .padding(horizontal = ForgeSpace.md),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(ForgeSpace.xs),
    ) {
        Icon(
            icon,
            contentDescription = null,
            tint = colors.textMuted,
            modifier = Modifier.size(ForgeSize.iconXs),
        )
        Text(
            title,
            style = type.chip,
            color = colors.text,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
            modifier = Modifier.width(ForgeSize.attachmentLabel),
        )
        Text(detail, style = type.selectLabel, color = colors.textMuted, maxLines = 1)
    }
}

/**
 * The per-turn token line.
 *
 * `est_cost_usd` is described by the protocol as "never a bill — an estimate",
 * so it is rendered with that word and never as a total owed.
 */
@Composable
fun UsageLine(usage: TokenUsage, modifier: Modifier = Modifier) {
    val colors = Forge.colors
    val type = Forge.type
    val segments = buildList {
        add("${format(usage.inputTokens)} in")
        add("${format(usage.outputTokens)} out")
        if (usage.reasoningTokens > 0) add("${format(usage.reasoningTokens)} thinking")
        if (usage.cachedTokens > 0) add("${format(usage.cachedTokens)} cached")
        usage.estCostUsd?.let { add("~$${"%.2f".format(it)} est.") }
    }
    Column(modifier = modifier.testTag(USAGE_LINE_TAG)) {
        Text(
            segments.joinToString(" · "),
            style = type.selectLabel,
            color = colors.textMuted,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
    }
}

private fun format(tokens: Long): String = when {
    tokens >= 1_000_000 -> "%.1fM".format(tokens / 1_000_000.0)
    tokens >= 1_000 -> "%.1fk".format(tokens / 1_000.0)
    else -> tokens.toString()
}
