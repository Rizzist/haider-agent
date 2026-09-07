package ai.diffforge.haider.ui.chat

import ai.diffforge.haider.transport.SessionConfig
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.Check
import androidx.compose.material.icons.rounded.Close
import androidx.compose.material3.Icon
import androidx.compose.material3.minimumInteractiveComponentSize
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogProperties

@Composable
fun ModelPicker(
    config: SessionConfig?,
    error: String?,
    busy: Boolean,
    onSelectModel: (String, String) -> Unit,
    onSelectEffort: (String?) -> Unit,
    onRefresh: () -> Unit,
    onDismiss: () -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    var pending by remember { mutableStateOf<PendingSelection?>(null) }
    Dialog(
        onDismissRequest = onDismiss,
        properties = DialogProperties(usePlatformDefaultWidth = false),
    ) {
        Column(
            modifier = Modifier
                .padding(horizontal = ForgeSpace.xl)
                .widthIn(max = ForgeSize.sheetMax)
                .fillMaxWidth()
                .fillMaxHeight(0.86f)
                .clip(ForgeShapes.card)
                .background(colors.surfaceRaised)
                .border(ForgeSize.hairline, colors.borderStrong, ForgeShapes.card),
        ) {
            Row(
                modifier = Modifier.fillMaxWidth().padding(start = ForgeSpace.xl, end = ForgeSpace.md, top = ForgeSpace.lg, bottom = ForgeSpace.md),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Column(modifier = Modifier.weight(1f)) {
                    Text("Model & provider", style = type.h4, color = colors.text)
                    Text(
                        config?.let { "${it.current.provider} / ${it.current.model}" } ?: "Loading daemon catalog…",
                        style = type.toolRow,
                        color = colors.textMuted,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                }
                Box(
                    modifier = Modifier.size(ForgeSize.touch).clip(CircleShape).clickable(onClick = onDismiss),
                    contentAlignment = Alignment.Center,
                ) {
                    Icon(Icons.Rounded.Close, contentDescription = "Close", tint = colors.textSoft)
                }
            }
            Box(Modifier.fillMaxWidth().background(colors.border).size(width = ForgeSize.hairline, height = ForgeSize.hairline))

            val pendingSelection = pending
            when {
                pendingSelection != null -> CacheChangeConfirmation(
                    selection = pendingSelection,
                    onConfirm = {
                        when (pendingSelection) {
                            is PendingSelection.Model -> onSelectModel(
                                pendingSelection.provider,
                                pendingSelection.model,
                            )
                            is PendingSelection.Effort -> onSelectEffort(pendingSelection.effort)
                        }
                        pending = null
                    },
                    onCancel = { pending = null },
                )
                error != null -> CatalogNotice(error, "Retry", onRefresh)
                config == null -> CatalogNotice("Asking the daemon for its model catalog…", null, onRefresh)
                !config.catalogAvailable -> CatalogNotice(
                    config.unavailableReason ?: "The provider catalog is unavailable.",
                    "Retry",
                    onRefresh,
                )
                else -> CatalogList(
                    config = config,
                    busy = busy,
                    onSelectModel = { provider, model ->
                        pending = PendingSelection.Model(provider, model)
                    },
                    onSelectEffort = { effort -> pending = PendingSelection.Effort(effort) },
                )
            }
        }
    }
}

@Composable
private fun CacheChangeConfirmation(
    selection: PendingSelection,
    onConfirm: () -> Unit,
    onCancel: () -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    val target = when (selection) {
        is PendingSelection.Model -> "${selection.provider} / ${selection.model}"
        is PendingSelection.Effort -> selection.effort ?: "provider-default effort"
    }
    Column(modifier = Modifier.fillMaxWidth().padding(ForgeSpace.xxl)) {
        // The question is the title; the uppercase eyebrow above it said the
        // same thing in a shout (addition F, G2).
        Text("Switch to $target?", style = type.h4, color = colors.amber)
        Spacer(Modifier.size(ForgeSize.stateDot))
        Text(
            "This can invalidate stable prompt tokens and start a new context-cache epoch. " +
                "The daemon will apply the change to the next turn.",
            style = type.userBody,
            color = colors.textMuted,
        )
        Row(
            modifier = Modifier.fillMaxWidth().padding(top = ForgeSpace.xl),
            horizontalArrangement = Arrangement.End,
        ) {
            PickerAction("Cancel", colors.textSoft, onCancel)
            Spacer(Modifier.width(ForgeSpace.md))
            PickerAction("Confirm change", colors.amber, onConfirm)
        }
    }
}

@Composable
private fun PickerAction(label: String, color: Color, onClick: () -> Unit) {
    Text(
        label,
        style = Forge.type.chip,
        color = color,
        modifier = Modifier
            .clip(ForgeShapes.pill)
            .border(ForgeSize.hairline, color.copy(alpha = 0.45f), ForgeShapes.pill)
            .clickable(onClick = onClick)
            .minimumInteractiveComponentSize()
            .padding(horizontal = ForgeSpace.lg, vertical = ForgeSpace.md),
    )
}

@Composable
private fun CatalogList(
    config: SessionConfig,
    busy: Boolean,
    onSelectModel: (String, String) -> Unit,
    onSelectEffort: (String?) -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    val selectedModel = config.providers
        .firstOrNull { it.id == config.current.provider }
        ?.models
        ?.firstOrNull { it.id == config.current.model }

    LazyColumn(
        modifier = Modifier.fillMaxWidth(),
        contentPadding = androidx.compose.foundation.layout.PaddingValues(bottom = ForgeSpace.xl),
    ) {
        item {
            Text(
                "Effort",
                style = type.sessionMeta,
                color = colors.textMuted,
                modifier = Modifier.padding(start = ForgeSpace.xl, end = ForgeSpace.xl, top = ForgeSpace.xl, bottom = ForgeSpace.sm),
            )
        }
        item {
            Column(modifier = Modifier.padding(horizontal = ForgeSpace.lg), verticalArrangement = Arrangement.spacedBy(ForgeSpace.xs)) {
                SelectionRow(
                    title = "Provider default",
                    detail = selectedModel?.defaultEffort?.let { "Default: $it" },
                    selected = config.current.effort == null,
                    enabled = !busy && config.current.effort != null,
                    onClick = { onSelectEffort(null) },
                )
                selectedModel?.supportedEfforts.orEmpty().forEach { effort ->
                    SelectionRow(
                        title = effort,
                        detail = null,
                        selected = config.current.effort == effort,
                        enabled = !busy && config.current.effort != effort,
                        onClick = { onSelectEffort(effort) },
                    )
                }
                if (selectedModel?.supportedEfforts.isNullOrEmpty()) {
                    Text(
                        "This model does not advertise an effort ladder.",
                        style = type.toolRow,
                        color = colors.textMuted,
                        modifier = Modifier.padding(horizontal = ForgeSpace.md, vertical = ForgeSpace.xs),
                    )
                }
            }
        }
        config.providers.forEach { provider ->
            item(key = "provider-${provider.id}") {
                val available = provider.enabled && provider.availability == "available"
                Row(
                    modifier = Modifier.fillMaxWidth().padding(start = ForgeSpace.xl, end = ForgeSpace.xl, top = ForgeSpace.xl, bottom = ForgeSpace.sm),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    BrandMark(provider.id)
                    Spacer(Modifier.width(ForgeSpace.md))
                    // The id is already the brand as it is written everywhere else
                    // — "anthropic", not "ANTHROPIC" (addition F, G2).
                    Text(provider.id, style = type.sessionTitle, color = colors.textSoft)
                    Spacer(Modifier.weight(1f))
                    Box(
                        Modifier.size(ForgeSize.stateDot).clip(CircleShape).background(
                            if (available) colors.green else colors.textMuted,
                        ),
                    )
                    Spacer(Modifier.width(ForgeSpace.xs))
                    Text(
                        if (available) "available" else provider.availability,
                        style = type.sessionMeta,
                        color = if (available) colors.green else colors.textMuted,
                    )
                }
                provider.availabilityReason?.let { reason ->
                    Text(
                        reason,
                        style = type.toolRow,
                        color = colors.textMuted,
                        modifier = Modifier.padding(horizontal = ForgeSpace.xl, vertical = ForgeSpace.xxs),
                    )
                }
            }
            items(provider.models, key = { "${provider.id}/${it.id}" }) { model ->
                val available = provider.enabled && provider.availability == "available"
                val context = model.contextWindow?.let { "${formatTokens(it)} context" }
                SelectionRow(
                    title = model.id,
                    detail = context,
                    selected = config.current.provider == provider.id && config.current.model == model.id,
                    enabled = available && !busy &&
                        !(config.current.provider == provider.id && config.current.model == model.id),
                    onClick = { onSelectModel(provider.id, model.id) },
                    modifier = Modifier.padding(horizontal = ForgeSpace.lg),
                )
            }
        }
        item {
            Text(
                "Provider/model availability is daemon-owned. Unavailable rows are read-only. " +
                    "Changing model or effort may start a new context-cache epoch.",
                style = type.toolRow,
                color = colors.textMuted,
                modifier = Modifier.padding(horizontal = ForgeSpace.xl, vertical = ForgeSpace.xl),
            )
        }
    }
}

@Composable
private fun SelectionRow(
    title: String,
    detail: String?,
    selected: Boolean,
    enabled: Boolean,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = Forge.colors
    val type = Forge.type
    Row(
        modifier = modifier
            .fillMaxWidth()
            .clip(ForgeShapes.cardTight)
            .background(if (selected) colors.surfaceSelected else Color.Transparent)
            .clickable(enabled = enabled, onClick = onClick)
            .minimumInteractiveComponentSize()
            .padding(horizontal = ForgeSpace.md, vertical = ForgeSpace.md),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(modifier = Modifier.weight(1f)) {
            Text(
                title,
                style = type.userBody,
                color = if (selected || enabled) colors.chatText else colors.textDisabled,
            )
            detail?.let { Text(it, style = type.toolRow, color = colors.textMuted) }
        }
        if (selected) {
            Icon(Icons.Rounded.Check, contentDescription = "Selected", tint = colors.accentSoft, modifier = Modifier.size(ForgeSize.iconSm))
        }
    }
}

@Composable
private fun CatalogNotice(message: String, action: String?, onAction: () -> Unit) {
    val colors = Forge.colors
    val type = Forge.type
    Column(
        modifier = Modifier.fillMaxWidth().padding(ForgeSpace.xxxl),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Text(message, style = type.chatBody, color = colors.textMuted)
        if (action != null) {
            Text(
                action,
                style = type.chip,
                color = colors.accentSoft,
                modifier = Modifier
                    .padding(top = ForgeSpace.lg)
                    .clip(ForgeShapes.pill)
                    .border(ForgeSize.hairline, colors.accentSoft.copy(alpha = 0.4f), ForgeShapes.pill)
                    .clickable(onClick = onAction)
                    .minimumInteractiveComponentSize()
                    .padding(horizontal = ForgeSpace.lg, vertical = ForgeSpace.md),
            )
        }
    }
}

private fun formatTokens(tokens: Long): String = when {
    tokens >= 1_000_000L -> "${tokens / 1_000_000L}m"
    tokens >= 1_000L -> "${tokens / 1_000L}k"
    else -> tokens.toString()
}

private sealed interface PendingSelection {
    data class Model(val provider: String, val model: String) : PendingSelection
    data class Effort(val effort: String?) : PendingSelection
}
