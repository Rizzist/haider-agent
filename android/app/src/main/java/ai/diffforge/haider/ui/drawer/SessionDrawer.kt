package ai.diffforge.haider.ui.drawer

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.components.ForgeChip
import ai.diffforge.haider.ui.components.ForgeIconButton
import ai.diffforge.haider.ui.state.AppUiState
import ai.diffforge.haider.ui.state.ModelNames
import ai.diffforge.haider.ui.state.SessionFilter
import ai.diffforge.haider.ui.state.SessionGroupKind
import ai.diffforge.haider.ui.state.SessionListState
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeMotion
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import ai.diffforge.haider.ui.theme.ThemeMode
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.Add
import androidx.compose.material.icons.rounded.ChevronLeft
import androidx.compose.material.icons.rounded.ChevronRight
import androidx.compose.material.icons.rounded.Close
import androidx.compose.material.icons.rounded.Palette
import androidx.compose.material.icons.rounded.Search
import androidx.compose.material.icons.rounded.Tune
import androidx.compose.material.icons.rounded.Bolt
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshotFlow
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextOverflow
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.distinctUntilChanged

/**
 * The drawer: identity, daemon card and New session fixed at the top, the
 * session list scrolling, the footer pinned — the desktop rail's
 * `auto / minmax(0,1fr) / max-content` grid (appStyles.js:2785-2790), on a phone.
 *
 * The list order is frozen while the drawer is open: a roster delta updates rows
 * in place, but re-sorting only happens on open, on a filter change and on a
 * search change. A list that reorders under a thumb steals taps
 * (UI-SPEC 3.3, trap 6.6.4).
 */
@Composable
fun SessionDrawer(
    state: AppUiState,
    themeMode: ThemeMode,
    appVersion: String,
    onClose: () -> Unit,
    onNewSession: () -> Unit,
    onNewSessionWith: () -> Unit,
    onSelect: (String) -> Unit,
    onRowAction: (String, SessionRowAction) -> Unit,
    onFilter: (SessionFilter) -> Unit,
    onQuery: (String) -> Unit,
    onStartDaemon: () -> Unit,
    onStopDaemon: () -> Unit,
    onOpenDaemonDetails: () -> Unit,
    onOpenModel: () -> Unit,
    onOpenSettings: () -> Unit,
    onThemeMode: (ThemeMode) -> Unit,
    onLoadMore: () -> Unit,
    nowMsProvider: () -> Long = System::currentTimeMillis,
    modifier: Modifier = Modifier,
) {
    val colors = Forge.colors
    val type = Forge.type
    var nowMs by remember { mutableLongStateOf(nowMsProvider()) }
    LaunchedEffect(Unit) {
        // Recompute relative time on a 30 s ticker while the drawer is open,
        // and the resource line every 5 s. Never while it is closed.
        while (true) {
            delay(ForgeMotion.RESOURCE_TICK_MS)
            nowMs = nowMsProvider()
        }
    }

    val counts = remember(state.sessions) { SessionListState.counts(state.sessions) }
    // Captured on open and whenever the user changes what they are looking at.
    val snapshot = remember(state.filter, state.query, state.sessions.size == 0) {
        SessionListState.OrderSnapshot.of(
            SessionListState.order(state.sessions, state.activeSessionId),
        )
    }
    val groups = remember(state.sessions, state.filter, state.query, state.activeSessionId, snapshot) {
        SessionListState.groups(
            rows = state.sessions,
            activeId = state.activeSessionId,
            filter = state.filter,
            query = state.query,
            snapshot = snapshot,
        )
    }

    val listState = rememberLazyListState()
    LaunchedEffect(listState, state.paging.hasMore) {
        snapshotFlow { listState.layoutInfo.visibleItemsInfo.lastOrNull()?.index ?: 0 }
            .distinctUntilChanged()
            .collect { lastVisible ->
                val total = listState.layoutInfo.totalItemsCount
                if (state.paging.hasMore && !state.paging.loading && lastVisible >= total - PAGE_TRIGGER) {
                    onLoadMore()
                }
            }
    }

    Column(
        modifier = modifier
            .fillMaxSize()
            .background(colors.surface)
            .padding(horizontal = ForgeSpace.lg),
    ) {
        IdentityBlock(appVersion = appVersion, onClose = onClose)

        DaemonStatusCard(
            status = state.daemon,
            activeTurns = state.sessions.count { it.runId != null },
            nowMs = nowMs,
            onStart = onStartDaemon,
            onStop = onStopDaemon,
            onOpenDetails = onOpenDaemonDetails,
            modifier = Modifier.padding(bottom = ForgeSpace.lg),
        )

        NewSessionRow(onClick = onNewSession, onLongClick = onNewSessionWith)

        if (state.sessions.size > SEARCH_THRESHOLD) {
            SearchField(query = state.query, onQuery = onQuery)
            // Completeness is known only after coverage through each recorded
            // head, so say how far the index has got instead of implying it is
            // finished (contracts-v1, history and search).
            if (state.query.isNotBlank() && !state.searchIndex.complete) {
                Text(
                    stringResource(
                        R.string.drawer_search_partial,
                        state.searchIndex.indexedSessions,
                        state.searchIndex.totalSessions,
                    ),
                    style = type.sessionMeta,
                    color = colors.amber,
                    modifier = Modifier.padding(
                        start = ForgeSpace.xl,
                        top = ForgeSpace.xs,
                    ),
                )
            }
        }
        if (state.sessions.size > FILTER_THRESHOLD) {
            FilterRow(filter = state.filter, counts = counts, onFilter = onFilter)
        }

        Box(Modifier.weight(1f)) {
            if (groups.isEmpty()) {
                Text(
                    when {
                        state.query.isNotBlank() -> stringResource(R.string.drawer_search_empty, state.query)
                        state.filter != SessionFilter.All -> stringResource(R.string.drawer_filter_empty)
                        else -> stringResource(R.string.drawer_empty)
                    },
                    style = type.sessionMeta,
                    color = colors.textMuted,
                    modifier = Modifier.padding(vertical = ForgeSpace.xxl),
                )
            } else {
                LazyColumn(state = listState, modifier = Modifier.fillMaxSize()) {
                    groups.forEach { group ->
                        item(key = "group-${group.kind}") {
                            Text(
                                stringResource(groupLabel(group.kind)),
                                style = type.drawerSection,
                                color = colors.textMuted,
                                modifier = Modifier.padding(
                                    start = ForgeSpace.xl,
                                    top = ForgeSpace.lg,
                                    bottom = ForgeSpace.sm,
                                ),
                            )
                        }
                        items(group.rows, key = { it.id }) { row ->
                            SessionRowItem(
                                row = row,
                                selected = row.id == state.activeSessionId,
                                nowMs = nowMs,
                                onClick = { onSelect(row.id) },
                                onLongClick = { onRowAction(row.id, SessionRowAction.Rename) },
                                onAction = { action -> onRowAction(row.id, action) },
                            )
                        }
                    }
                    if (state.paging.hasMore) {
                        item(key = "paging") {
                            Text(
                                stringResource(R.string.drawer_loading_more),
                                style = type.sessionMeta,
                                color = colors.textMuted,
                                modifier = Modifier.padding(ForgeSpace.xl),
                            )
                        }
                    }
                }
            }
        }

        DrawerFooter(
            state = state,
            themeMode = themeMode,
            onOpenModel = onOpenModel,
            onOpenSettings = onOpenSettings,
            onThemeMode = onThemeMode,
        )
    }
}

@Composable
private fun IdentityBlock(appVersion: String, onClose: () -> Unit) {
    val colors = Forge.colors
    val type = Forge.type
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(top = ForgeSpace.xl, bottom = ForgeSpace.lg),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(
            modifier = Modifier
                .size(ForgeSize.markLg)
                .clip(ForgeShapes.card)
                .background(colors.accentWash)
                .border(ForgeSize.hairline, colors.accent, ForgeShapes.card),
            contentAlignment = Alignment.Center,
        ) {
            Text("H", style = type.h4, color = colors.accent)
        }
        Column(
            Modifier
                .weight(1f)
                .padding(start = ForgeSpace.lg),
        ) {
            Text(stringResource(R.string.app_name), style = type.h4, color = colors.text)
            Text(
                stringResource(R.string.drawer_identity_line, appVersion),
                style = type.numeric,
                color = colors.textMuted,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
        }
        ForgeIconButton(
            onClick = onClose,
            contentDescription = stringResource(R.string.cd_close_sessions),
        ) {
            Icon(
                Icons.Rounded.ChevronLeft,
                contentDescription = null,
                tint = colors.textSoft,
                modifier = Modifier.size(ForgeSize.icon),
            )
        }
    }
}

@Composable
private fun NewSessionRow(onClick: () -> Unit, onLongClick: () -> Unit) {
    val colors = Forge.colors
    val type = Forge.type
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .heightIn(min = ForgeSize.touch)
            .clip(ForgeShapes.pill)
            .background(colors.accentWash)
            .border(ForgeSize.hairline, colors.accent, ForgeShapes.pill)
            .clickable(onClick = onClick)
            .padding(horizontal = ForgeSpace.xl),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(ForgeSpace.md),
    ) {
        Icon(
            Icons.Rounded.Add,
            contentDescription = null,
            tint = colors.accent,
            modifier = Modifier.size(ForgeSize.iconSm),
        )
        Text(stringResource(R.string.drawer_new_session), style = type.button, color = colors.accent)
        Box(Modifier.weight(1f))
        Text(
            stringResource(R.string.drawer_new_session_with),
            style = type.sessionMeta,
            color = colors.accent.copy(alpha = 0.75f),
            maxLines = 1,
            modifier = Modifier
                .clip(ForgeShapes.pill)
                .clickable(onClick = onLongClick)
                .padding(horizontal = ForgeSpace.sm, vertical = ForgeSpace.xs),
        )
    }
}

@Composable
private fun SearchField(query: String, onQuery: (String) -> Unit) {
    val colors = Forge.colors
    val type = Forge.type
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(top = ForgeSpace.lg)
            .heightIn(min = ForgeSize.touch)
            .clip(ForgeShapes.pill)
            .background(colors.surfaceControl)
            .border(ForgeSize.hairline, colors.border, ForgeShapes.pill)
            .padding(horizontal = ForgeSpace.lg),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Icon(
            Icons.Rounded.Search,
            contentDescription = null,
            tint = colors.textMuted,
            modifier = Modifier.size(ForgeSize.iconSm),
        )
        BasicTextField(
            value = query,
            onValueChange = onQuery,
            singleLine = true,
            textStyle = type.sessionMeta.copy(color = colors.text),
            cursorBrush = SolidColor(colors.accent),
            modifier = Modifier
                .weight(1f)
                .padding(horizontal = ForgeSpace.md),
            decorationBox = { inner ->
                Box {
                    if (query.isEmpty()) {
                        Text(
                            stringResource(R.string.drawer_search_hint),
                            style = type.sessionMeta,
                            color = colors.textMuted,
                        )
                    }
                    inner()
                }
            },
        )
        if (query.isNotEmpty()) {
            ForgeIconButton(
                onClick = { onQuery("") },
                contentDescription = stringResource(R.string.cd_clear_search),
            ) {
                Icon(
                    Icons.Rounded.Close,
                    contentDescription = null,
                    tint = colors.textMuted,
                    modifier = Modifier.size(ForgeSize.iconSm),
                )
            }
        }
    }
}

@Composable
private fun FilterRow(
    filter: SessionFilter,
    counts: ai.diffforge.haider.ui.state.SessionListCounts,
    onFilter: (SessionFilter) -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .heightIn(min = ForgeSize.touch),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(ForgeSpace.md),
    ) {
        listOf(
            Triple(SessionFilter.All, R.string.drawer_filter_all, counts.all),
            Triple(SessionFilter.Running, R.string.drawer_filter_running, counts.running),
            Triple(SessionFilter.NeedsInput, R.string.drawer_filter_needs_input, counts.needsInput),
        ).forEach { (value, labelRes, count) ->
            val label = stringResource(labelRes, count)
            ForgeChip(
                onClick = { onFilter(value) },
                height = ForgeSize.filterChip,
                selected = filter == value,
                contentDescription = label,
            ) {
                Text(
                    label,
                    style = type.sessionMeta,
                    color = if (filter == value) colors.accent else colors.textMuted,
                    maxLines = 1,
                )
            }
        }
    }
}

@Composable
private fun DrawerFooter(
    state: AppUiState,
    themeMode: ThemeMode,
    onOpenModel: () -> Unit,
    onOpenSettings: () -> Unit,
    onThemeMode: (ThemeMode) -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    Column {
        Box(
            Modifier
                .fillMaxWidth()
                .height(ForgeSize.hairline)
                .background(colors.border),
        )
        FooterRow(
            icon = Icons.Rounded.Bolt,
            primary = stringResource(R.string.drawer_footer_model),
            secondary = state.models?.let {
                ModelNames.full(it.current.provider, it.current.model, it.current.effort)
            },
            onClick = onOpenModel,
        )
        FooterRow(
            icon = Icons.Rounded.Tune,
            primary = stringResource(R.string.drawer_footer_settings),
            secondary = stringResource(R.string.drawer_footer_settings_secondary),
            onClick = onOpenSettings,
        )
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .heightIn(min = ForgeSize.footerRow),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Icon(
                Icons.Rounded.Palette,
                contentDescription = null,
                tint = colors.textMuted,
                modifier = Modifier.size(ForgeSize.iconSm),
            )
            Text(
                stringResource(R.string.drawer_footer_appearance),
                style = type.sessionTitle,
                color = colors.text,
                modifier = Modifier.padding(start = ForgeSpace.lg),
            )
            Box(Modifier.weight(1f))
            Row(horizontalArrangement = Arrangement.spacedBy(ForgeSpace.xs)) {
                listOf(
                    ThemeMode.System to R.string.appearance_system,
                    ThemeMode.Light to R.string.appearance_light,
                    ThemeMode.Dark to R.string.appearance_dark,
                ).forEach { (mode, labelRes) ->
                    val label = stringResource(labelRes)
                    ForgeChip(
                        onClick = { onThemeMode(mode) },
                        height = ForgeSize.filterChip,
                        selected = themeMode == mode,
                        contentDescription = label,
                    ) {
                        Text(
                            label,
                            style = type.sessionMeta,
                            color = if (themeMode == mode) colors.accent else colors.textMuted,
                        )
                    }
                }
            }
        }
    }
}

@Composable
private fun FooterRow(
    icon: androidx.compose.ui.graphics.vector.ImageVector,
    primary: String,
    secondary: String?,
    onClick: () -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .heightIn(min = ForgeSize.footerRow)
            .clickable(onClick = onClick),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Icon(
            icon,
            contentDescription = null,
            tint = colors.textMuted,
            modifier = Modifier.size(ForgeSize.iconSm),
        )
        Column(
            Modifier
                .weight(1f)
                .padding(start = ForgeSpace.lg),
        ) {
            Text(primary, style = type.sessionTitle, color = colors.text)
            secondary?.let {
                Text(
                    it,
                    style = type.numeric,
                    color = colors.textMuted,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
        }
        Icon(
            Icons.Rounded.ChevronRight,
            contentDescription = null,
            tint = colors.textMuted,
            modifier = Modifier.size(ForgeSize.iconSm),
        )
    }
}

private fun groupLabel(kind: SessionGroupKind): Int = when (kind) {
    SessionGroupKind.NeedsYou -> R.string.drawer_group_needs_you
    SessionGroupKind.Active -> R.string.drawer_group_active
    SessionGroupKind.Recent -> R.string.drawer_group_recent
}

private const val FILTER_THRESHOLD = 3
private const val SEARCH_THRESHOLD = 20
private const val PAGE_TRIGGER = 8
