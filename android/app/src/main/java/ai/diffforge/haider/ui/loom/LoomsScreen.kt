package ai.diffforge.haider.ui.loom

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.components.ForgeButton
import ai.diffforge.haider.ui.components.ForgeButtonKind
import ai.diffforge.haider.ui.components.ForgeChip
import ai.diffforge.haider.ui.components.ForgeIconButton
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawing
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.ArrowBack
import androidx.compose.material.icons.rounded.Refresh
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextOverflow

const val LOOMS_SCREEN_TAG = "looms_screen"

/**
 * The Loom registry: the specialists that exist, and the workflows built from
 * them.
 *
 * Two things this screen refuses to guess. A declared CLI whose name is absent
 * from `cli_present` reads **not probed**, never "missing" — the map is only
 * ever a positive statement about the names it contains. And an archive is a
 * compare-and-set under the revision the list published: a row without one
 * offers no archive button, because pressing it would be a fence this client
 * invented.
 */
@Composable
fun LoomsScreen(
    state: LoomScreenState,
    onBack: () -> Unit,
    onRefresh: () -> Unit,
    onIncludeArchived: (Boolean) -> Unit,
    onSetArchived: (LoomEntryKind, String, Boolean, LoomFence) -> Unit,
    onAuthor: (LoomAuthorKind) -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = Forge.colors
    val type = Forge.type
    Column(
        modifier
            .fillMaxSize()
            .background(colors.bg)
            // Without this the header row — and the only back control on the
            // screen — sat under the status bar and could not be tapped
            // (971-V F8, `api35-looms-unavailable.png`). Every other full
            // screen already insets itself.
            .windowInsetsPadding(WindowInsets.safeDrawing)
            .testTag(LOOMS_SCREEN_TAG),
    ) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .heightIn(min = ForgeSize.header)
                .padding(horizontal = ForgeSpace.xs),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            ForgeIconButton(
                onClick = onBack,
                contentDescription = stringResource(R.string.action_back),
            ) {
                Icon(
                    Icons.Rounded.ArrowBack,
                    contentDescription = null,
                    tint = colors.textSoft,
                    modifier = Modifier.size(ForgeSize.icon),
                )
            }
            Text(
                stringResource(R.string.looms_title),
                style = type.sessionTitle,
                color = colors.text,
                modifier = Modifier.weight(1f).padding(start = ForgeSpace.md),
            )
            ForgeIconButton(
                onClick = onRefresh,
                contentDescription = stringResource(R.string.cd_refresh_session),
            ) {
                Icon(
                    Icons.Rounded.Refresh,
                    contentDescription = null,
                    tint = colors.textSoft,
                    modifier = Modifier.size(ForgeSize.iconMd),
                )
            }
        }

        Column(
            Modifier
                .weight(1f)
                .verticalScroll(rememberScrollState())
                .padding(horizontal = ForgeSpace.lg, vertical = ForgeSpace.md),
            verticalArrangement = Arrangement.spacedBy(ForgeSpace.md),
        ) {
            val unavailable = state.unavailable
            if (unavailable != null) {
                // No list at all: an empty one would read as "you have none".
                Text(
                    stringResource(R.string.looms_unavailable, unavailable),
                    style = type.emptyBody,
                    color = colors.textMuted,
                )
                return@Column
            }
            val registry = state.registry
            if (registry == null) {
                Text(
                    stringResource(R.string.looms_unread),
                    style = type.emptyBody,
                    color = colors.textMuted,
                )
                return@Column
            }

            Row(horizontalArrangement = Arrangement.spacedBy(ForgeSpace.md)) {
                ForgeChip(
                    onClick = { onIncludeArchived(!state.includeArchived) },
                    selected = state.includeArchived,
                    contentDescription = stringResource(R.string.looms_show_archived),
                ) {
                    Text(
                        stringResource(R.string.looms_show_archived),
                        style = type.chip,
                        color = if (state.includeArchived) colors.text else colors.textMuted,
                    )
                }
            }
            state.conflictId?.let {
                Text(
                    stringResource(R.string.looms_conflict, it),
                    style = type.sessionMeta,
                    color = colors.amber,
                )
            }

            SectionLabel(stringResource(R.string.looms_section_agent_types))
            if (registry.agentTypes.isEmpty()) {
                Text(
                    stringResource(R.string.looms_empty),
                    style = type.emptyBody,
                    color = colors.textMuted,
                )
            }
            registry.agentTypes.forEach { entry ->
                AgentTypeRow(
                    entry = entry,
                    registry = registry,
                    job = state.installJobs.firstOrNull { it.agentTypeId == entry.id },
                    busy = state.busyId == entry.id,
                    onSetArchived = { archived ->
                        onSetArchived(
                            LoomEntryKind.AgentType,
                            entry.id,
                            archived,
                            LoomFence.of(entry),
                        )
                    },
                )
            }
            ForgeButton(
                text = stringResource(R.string.looms_new_agent_type),
                onClick = { onAuthor(LoomAuthorKind.AgentType) },
                kind = ForgeButtonKind.Ghost,
            )

            SectionLabel(stringResource(R.string.looms_section_workflows))
            if (registry.workflows.isEmpty()) {
                Text(
                    stringResource(R.string.looms_empty),
                    style = type.emptyBody,
                    color = colors.textMuted,
                )
            }
            registry.workflows.forEach { entry ->
                WorkflowRow(
                    entry = entry,
                    busy = state.busyId == entry.id,
                    onSetArchived = { archived ->
                        onSetArchived(
                            LoomEntryKind.Workflow,
                            entry.id,
                            archived,
                            LoomFence.of(entry),
                        )
                    },
                )
            }
            ForgeButton(
                text = stringResource(R.string.looms_new_workflow),
                onClick = { onAuthor(LoomAuthorKind.Workflow) },
                kind = ForgeButtonKind.Ghost,
            )

            // Only the jobs no row above could show. Each agent type already
            // carries its own install state, and listing it twice makes the
            // screen look like it has twice as much to say.
            val orphanJobs = state.installJobs.filter { job ->
                registry.agentTypes.none { it.id == job.agentTypeId }
            }
            if (orphanJobs.isNotEmpty()) {
                SectionLabel(stringResource(R.string.looms_section_installs))
                orphanJobs.forEach { job -> InstallRow(job) }
            }
        }
    }
}

@Composable
private fun SectionLabel(text: String) {
    Text(
        text,
        style = Forge.type.sessionTitle,
        color = Forge.colors.textSoft,
        modifier = Modifier.padding(top = ForgeSpace.md),
    )
}

@Composable
private fun AgentTypeRow(
    entry: LoomAgentTypeEntry,
    registry: LoomRegistry,
    job: LoomInstallJob?,
    busy: Boolean,
    onSetArchived: (Boolean) -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    Card {
        Row(verticalAlignment = Alignment.CenterVertically) {
            GlyphTile(color = entry.color, glyph = entry.glyph)
            Column(Modifier.weight(1f).padding(start = ForgeSpace.lg)) {
                Text(
                    entry.name.ifBlank { entry.id },
                    style = type.sessionTitle,
                    color = colors.text,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                Text(
                    stringResource(R.string.looms_types, entry.inType, entry.outType),
                    style = type.sessionMeta,
                    color = colors.textMuted,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
            entry.rev?.let {
                Text(
                    stringResource(R.string.looms_rev, it),
                    style = type.sessionMeta,
                    color = colors.textMuted,
                )
            }
        }
        if (entry.job.isNotBlank()) {
            Text(entry.job, style = type.emptyBody, color = colors.textSoft, maxLines = 2)
        }
        entry.clis.forEach { cli -> CliRow(cli, registry.cliPresence(cli)) }
        if (entry.denials.isNotEmpty()) {
            Text(
                stringResource(R.string.looms_denials, entry.denials.joinToString(", ")),
                style = type.sessionMeta,
                color = colors.textMuted,
            )
        }
        job?.let { InstallRow(it) }
        ArchiveButton(archived = entry.archived, busy = busy, fenced = entry.rev != null, onSetArchived)
    }
}

@Composable
private fun WorkflowRow(
    entry: LoomWorkflowEntry,
    busy: Boolean,
    onSetArchived: (Boolean) -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    Card {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Column(Modifier.weight(1f)) {
                Text(
                    entry.templateName.ifBlank { entry.id },
                    style = type.sessionTitle,
                    color = colors.text,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                Text(
                    stringResource(
                        R.string.looms_workflow_meta,
                        entry.inType,
                        entry.outType,
                        entry.nodeCount,
                    ),
                    style = type.sessionMeta,
                    color = colors.textMuted,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
            entry.rev?.let {
                Text(
                    stringResource(R.string.looms_rev, it),
                    style = type.sessionMeta,
                    color = colors.textMuted,
                )
            }
        }
        if (entry.source.isNotBlank()) {
            // mono: pipe source — the workflow's structure of record, read as code.
            Text(
                entry.source,
                style = type.toolRow,
                color = colors.textSoft,
                maxLines = 6,
                overflow = TextOverflow.Ellipsis,
            )
        }
        ArchiveButton(archived = entry.archived, busy = busy, fenced = entry.rev != null, onSetArchived)
    }
}

@Composable
private fun ArchiveButton(
    archived: Boolean,
    busy: Boolean,
    fenced: Boolean,
    onSetArchived: (Boolean) -> Unit,
) {
    // No revision, no button: `expected_rev` is required on this door, and a
    // fence the client made up is not a compare-and-set.
    if (!fenced) {
        Text(
            stringResource(R.string.looms_no_fence),
            style = Forge.type.sessionMeta,
            color = Forge.colors.textMuted,
        )
        return
    }
    ForgeButton(
        text = stringResource(
            when {
                busy -> R.string.looms_working
                archived -> R.string.looms_unarchive
                else -> R.string.looms_archive
            },
        ),
        onClick = { onSetArchived(!archived) },
        enabled = !busy,
        // Ghost, not destructive. Archiving is a selection state with its own
        // undo one tap away; painting it red borrows the weight of a delete for
        // something that deletes nothing.
        kind = ForgeButtonKind.Ghost,
        minHeight = ForgeSize.bannerAction,
    )
}

@Composable
private fun CliRow(name: String, presence: LoomCliPresence) {
    val colors = Forge.colors
    val word = stringResource(
        when (presence) {
            LoomCliPresence.Present -> R.string.looms_cli_present
            LoomCliPresence.Missing -> R.string.looms_cli_missing
            // The map said nothing about this name. That is not the same claim
            // as "it is not on this device".
            LoomCliPresence.NotProbed -> R.string.looms_cli_unprobed
        },
    )
    Row(
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(ForgeSpace.sm),
    ) {
        Box(
            Modifier
                .size(ForgeSize.stateDot)
                .clip(ForgeShapes.pill)
                .background(
                    when (presence) {
                        LoomCliPresence.Present -> colors.green
                        LoomCliPresence.Missing -> colors.amber
                        LoomCliPresence.NotProbed -> colors.stateUnknown
                    },
                ),
        )
        // mono: a program name, as typed on a command line.
        Text(name, style = Forge.type.toolRow, color = colors.textSoft)
        Text(word, style = Forge.type.sessionMeta, color = colors.textMuted)
    }
}

@Composable
private fun InstallRow(job: LoomInstallJob) {
    val colors = Forge.colors
    val type = Forge.type
    Column {
        Text(
            stringResource(R.string.looms_install_state, job.stateRaw.orEmpty()),
            style = type.sessionMeta,
            color = if (job.retryable) colors.amber else colors.textMuted,
        )
        job.reason?.let {
            Text(it, style = type.sessionMeta, color = colors.textMuted, maxLines = 2)
        }
    }
}

/**
 * The registry's own accent and glyph.
 *
 * Both are display fields that participate in the content digest, so an entry
 * that carries neither gets a neutral tile — not a colour hashed from its id,
 * which would look exactly like a daemon fact and be none.
 */
@Composable
private fun GlyphTile(color: String, glyph: String) {
    val colors = Forge.colors
    val accent = parseHexColor(color)
    Box(
        Modifier
            .size(ForgeSize.loomGlyphTile)
            .clip(ForgeShapes.cardTight)
            .background(accent?.copy(alpha = ACCENT_WASH) ?: colors.surfaceControl)
            .border(
                ForgeSize.hairline,
                accent ?: colors.border,
                ForgeShapes.cardTight,
            ),
        contentAlignment = Alignment.Center,
    ) {
        if (glyph.isNotBlank()) {
            // mono: the registry's own glyph, authored against this ramp and
            // rendered on it rather than re-cast as prose.
            Text(
                glyph.take(2),
                style = Forge.type.toolStrong,
                color = accent ?: colors.textSoft,
            )
        }
    }
}

private const val ACCENT_WASH = 0.16f

/** A hex accent the registry published, or null. Never a colour this app chose. */
internal fun parseHexColor(value: String): Color? {
    val hex = value.trim().removePrefix("#")
    if (hex.length != 6 && hex.length != 8) return null
    val parsed = hex.toLongOrNull(radix = 16) ?: return null
    return if (hex.length == 6) Color(parsed or 0xFF000000L) else Color(parsed)
}

@Composable
private fun Card(content: @Composable () -> Unit) {
    val colors = Forge.colors
    Column(
        Modifier
            .fillMaxWidth()
            .clip(ForgeShapes.card)
            .background(colors.surface)
            .border(ForgeSize.hairline, colors.border, ForgeShapes.card)
            .padding(ForgeSpace.lg),
        verticalArrangement = Arrangement.spacedBy(ForgeSpace.sm),
    ) { content() }
}
