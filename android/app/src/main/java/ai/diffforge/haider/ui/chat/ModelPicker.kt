package ai.diffforge.haider.ui.chat

import ai.diffforge.haider.transport.SessionConfig
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
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
                .padding(horizontal = 16.dp)
                .widthIn(max = 640.dp)
                .fillMaxWidth()
                .fillMaxHeight(0.86f)
                .clip(ForgeShapes.card)
                .background(colors.surfaceRaised)
                .border(1.dp, colors.borderStrong, ForgeShapes.card),
        ) {
            Row(
                modifier = Modifier.fillMaxWidth().padding(start = 18.dp, end = 10.dp, top = 12.dp, bottom = 10.dp),
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
                    modifier = Modifier.size(48.dp).clip(CircleShape).clickable(onClick = onDismiss),
                    contentAlignment = Alignment.Center,
                ) {
                    Icon(Icons.Rounded.Close, contentDescription = "Close", tint = colors.textSoft)
                }
            }
            Box(Modifier.fillMaxWidth().background(colors.border).size(width = 1.dp, height = 1.dp))

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
    Column(modifier = Modifier.fillMaxWidth().padding(22.dp)) {
        Text("CONFIRM CACHE EPOCH", style = type.label, color = colors.amber)
        Spacer(Modifier.size(8.dp))
        Text("Switch to $target?", style = type.h4, color = colors.text)
        Spacer(Modifier.size(7.dp))
        Text(
            "This can invalidate stable prompt tokens and start a new context-cache epoch. " +
                "The daemon will apply the change to the next turn.",
            style = type.userBody,
            color = colors.textMuted,
        )
        Row(
            modifier = Modifier.fillMaxWidth().padding(top = 18.dp),
            horizontalArrangement = Arrangement.End,
        ) {
            PickerAction("Cancel", colors.textSoft, onCancel)
            Spacer(Modifier.width(8.dp))
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
            .border(1.dp, color.copy(alpha = 0.45f), ForgeShapes.pill)
            .clickable(onClick = onClick)
            .heightIn(min = 48.dp)
            .padding(horizontal = 13.dp, vertical = 8.dp),
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
        contentPadding = androidx.compose.foundation.layout.PaddingValues(bottom = 18.dp),
    ) {
        item {
            Text(
                "EFFORT",
                style = type.label,
                color = colors.textMuted,
                modifier = Modifier.padding(start = 18.dp, end = 18.dp, top = 15.dp, bottom = 7.dp),
            )
        }
        item {
            Column(modifier = Modifier.padding(horizontal = 12.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
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
                        modifier = Modifier.padding(horizontal = 8.dp, vertical = 5.dp),
                    )
                }
            }
        }
        config.providers.forEach { provider ->
            item(key = "provider-${provider.id}") {
                val available = provider.enabled && provider.availability == "available"
                Row(
                    modifier = Modifier.fillMaxWidth().padding(start = 18.dp, end = 18.dp, top = 19.dp, bottom = 7.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    BrandMark(provider.id)
                    Spacer(Modifier.width(8.dp))
                    Text(provider.id.uppercase(), style = type.label, color = colors.textSoft)
                    Spacer(Modifier.weight(1f))
                    Box(
                        Modifier.size(6.dp).clip(CircleShape).background(
                            if (available) colors.green else colors.textMuted,
                        ),
                    )
                    Spacer(Modifier.width(5.dp))
                    Text(
                        if (available) "AVAILABLE" else provider.availability.uppercase(),
                        style = type.label,
                        color = if (available) colors.green else colors.textMuted,
                    )
                }
                provider.availabilityReason?.let { reason ->
                    Text(
                        reason,
                        style = type.toolRow,
                        color = colors.textMuted,
                        modifier = Modifier.padding(horizontal = 18.dp, vertical = 2.dp),
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
                    modifier = Modifier.padding(horizontal = 12.dp),
                )
            }
        }
        item {
            Text(
                "Provider/model availability is daemon-owned. Unavailable rows are read-only. " +
                    "Changing model or effort may start a new context-cache epoch.",
                style = type.toolRow,
                color = colors.textMuted,
                modifier = Modifier.padding(horizontal = 18.dp, vertical = 16.dp),
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
            .heightIn(min = 48.dp)
            .padding(horizontal = 10.dp, vertical = 10.dp),
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
            Icon(Icons.Rounded.Check, contentDescription = "Selected", tint = colors.accentSoft, modifier = Modifier.size(18.dp))
        }
    }
}

@Composable
private fun CatalogNotice(message: String, action: String?, onAction: () -> Unit) {
    val colors = Forge.colors
    val type = Forge.type
    Column(
        modifier = Modifier.fillMaxWidth().padding(24.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Text(message, style = type.chatBody, color = colors.textMuted)
        if (action != null) {
            Text(
                action,
                style = type.chip,
                color = colors.accentSoft,
                modifier = Modifier
                    .padding(top = 14.dp)
                    .clip(ForgeShapes.pill)
                    .border(1.dp, colors.accentSoft.copy(alpha = 0.4f), ForgeShapes.pill)
                    .clickable(onClick = onAction)
                    .heightIn(min = 48.dp)
                    .padding(horizontal = 14.dp, vertical = 8.dp),
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
