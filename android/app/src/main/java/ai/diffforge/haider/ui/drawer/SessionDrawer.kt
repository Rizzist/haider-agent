package ai.diffforge.haider.ui.drawer

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.components.ForgeIconButton
import ai.diffforge.haider.ui.state.AppUiState
import ai.diffforge.haider.ui.state.ModelNames
import ai.diffforge.haider.ui.state.SessionFilter
import ai.diffforge.haider.ui.state.SessionGroupKind
import ai.diffforge.haider.ui.state.SessionListState
import ai.diffforge.haider.ui.state.SessionTree
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeMotion
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import ai.diffforge.haider.ui.theme.ThemeMode
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.background
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.requiredSize
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
import androidx.compose.material.icons.rounded.Hub
import androidx.compose.material.icons.rounded.Search
import androidx.compose.material.icons.rounded.Tune
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
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.CustomAccessibilityAction
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.customActions
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextOverflow
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.distinctUntilChanged
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.testTag

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
/** The merged daemon + New chat + collapse row. */
const val DRAWER_HEAD_TAG = "drawer_head"

@Composable
fun SessionDrawer(
    state: AppUiState,
    themeMode: ThemeMode,
    appVersion: String,
    /** False while the drawer is shut: its rows must not animate (O5). */
    open: Boolean = true,
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
    /** Folds or unfolds the sessions one session delegated (lane 971-UI-fleet). */
    onToggleFamily: (String) -> Unit = {},
    onOpenFleet: () -> Unit = {},
    nowMsProvider: () -> Long = System::currentTimeMillis,
    elapsedRealtimeProvider: () -> Long = android.os.SystemClock::elapsedRealtime,
    modifier: Modifier = Modifier,
) {
    val colors = Forge.colors
    val type = Forge.type
    var nowMs by remember { mutableLongStateOf(nowMsProvider()) }
    var elapsedMs by remember { mutableLongStateOf(elapsedRealtimeProvider()) }
    LaunchedEffect(Unit) {
        // Relative times are wall-clock; uptime is monotonic. They are two
        // different clocks and mixing them is a fifty-seven-year uptime.
        while (true) {
            delay(ForgeMotion.RESOURCE_TICK_MS)
            nowMs = nowMsProvider()
            elapsedMs = elapsedRealtimeProvider()
        }
    }

    val counts = remember(state.sessions) { SessionListState.counts(state.sessions) }
    // The snapshot is owned by the view model and captured on drawer open, so
    // the frozen order survives recomposition and a filter change re-captures
    // deliberately rather than by accident of a `remember` key.
    val snapshot = state.orderSnapshot
    val searchIds = state.searchOutcome?.hits?.map { it.sessionId }?.toSet().orEmpty()
    // Sections, not a flat list: a delegated session renders under the session
    // that spawned it (`parent_session_id`, frame.rs:1949). With no delegation
    // in the roster this is exactly SessionListState.groups — pinned in
    // SessionTreeTest, which is what keeps the existing goldens honest.
    val sections = remember(
        state.sessions,
        state.filter,
        state.query,
        state.activeSessionId,
        snapshot,
        searchIds,
    ) {
        SessionTree.sections(
            rows = state.sessions,
            activeId = state.activeSessionId,
            filter = state.filter,
            query = state.query,
            snapshot = snapshot,
            // Transcript-content hits the metadata filter would not find.
            extraIds = searchIds,
        )
    }
    val delegatedCount = remember(state.sessions) {
        state.sessions.count { row -> row.parentSessionId != null }
    }

    val listState = rememberLazyListState()
    LaunchedEffect(listState, state.paging.hasMore) {
        snapshotFlow { listState.layoutInfo.visibleItemsInfo.lastOrNull()?.index ?: 0 }
            .distinctUntilChanged()
            .collect { lastVisible ->
                val layout = listState.layoutInfo
                val total = layout.totalItemsCount
                // A closed drawer has no visible items, and "0 >= 0 - 8" would
                // otherwise page the whole roster in while nobody is looking.
                if (layout.visibleItemsInfo.isEmpty() || total == 0) return@collect
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
        // One row for both: the daemon's state on the left, New chat and the
        // collapse chevron on the right. Round 9 spent three full rows here —
        // a chevron alone, a status line, and a New chat row — which is the
        // "not making good use of space" the owner meant (round 10, R3).
        Row(
            modifier = Modifier
                .fillMaxWidth()
                // R3's 44 dp, taken literally this time. The row is 44 dp of
                // LAYOUT; each control keeps 48 dp of hit area by overflowing
                // the row symmetrically, which `Modifier.requiredHeight`
                // permits — the child measures 48 and the parent still
                // occupies 44 (verify-11 O7). Nothing is clipped, because the
                // controls' visuals are 32 dp inside that 48.
                .height(ForgeSize.drawerRow)
                .testTag(DRAWER_HEAD_TAG),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            DaemonStatusRow(
                status = state.daemon,
                onStart = onStartDaemon,
                onStop = onStopDaemon,
                onOpenDetails = onOpenDaemonDetails,
                modifier = Modifier.weight(1f),
            )
            NewSessionButton(onClick = onNewSession, onLongClick = onNewSessionWith)
            ForgeIconButton(
                onClick = onClose,
                contentDescription = stringResource(R.string.cd_close_sessions),
                visual = ForgeSize.headerControl,
            ) {
                Icon(
                    Icons.Rounded.ChevronLeft,
                    contentDescription = null,
                    tint = colors.textMuted,
                    modifier = Modifier.size(ForgeSize.iconSm),
                )
            }
        }

        // Search replaces the filter chips: needs-input rows already float to
        // the top, so a filter for them was a second way to say the same thing
        // (addition F, D4).
        if (state.sessions.isNotEmpty()) {
            SearchField(query = state.query, onQuery = onQuery)
            // Completeness is known only after coverage through each recorded
            // head, so say how far the index has got instead of implying it is
            // finished (contracts-v1, history and search).
            if (state.query.isNotBlank() && state.searching) {
                Text(
                    stringResource(R.string.drawer_search_running),
                    style = type.sessionMeta,
                    color = colors.textMuted,
                    modifier = Modifier.padding(start = ForgeSpace.xl, top = ForgeSpace.xs),
                )
            }
            if (state.query.isNotBlank() && !state.searching &&
                state.searchOutcome?.complete == false
            ) {
                Text(
                    stringResource(
                        R.string.drawer_search_partial,
                        state.searchOutcome?.index?.indexedSessions ?: 0,
                        state.searchOutcome?.index?.totalSessions ?: state.sessions.size,
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
        Box(Modifier.weight(1f)) {
            if (sections.isEmpty()) {
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
                    // Sections are contiguous runs, so a kind can legitimately
                    // appear more than once — an appended Active row behind a
                    // frozen Recent one does exactly that. The header key has
                    // to be unique per *section*, not per kind, or LazyColumn
                    // throws on the duplicate.
                    sections.forEach { section ->
                        val lines = SessionTree.lines(section.roots, state.familyToggles)
                        item(key = "section-${section.kind}-${lines.first().row.id}") {
                            Text(
                                stringResource(groupLabel(section.kind)),
                                style = type.drawerSection,
                                color = colors.textMuted,
                                modifier = Modifier.padding(
                                    start = ForgeSpace.xl,
                                    top = ForgeSpace.lg,
                                    bottom = ForgeSpace.sm,
                                ),
                            )
                        }
                        items(lines, key = { it.row.id }) { line ->
                            SessionTreeLineItem(
                                line = line,
                                selected = line.row.id == state.activeSessionId,
                                nowMs = nowMs,
                                onClick = { onSelect(line.row.id) },
                                onLongClick = {
                                    onRowAction(line.row.id, SessionRowAction.Rename)
                                },
                                onAction = { action -> onRowAction(line.row.id, action) },
                                onToggleFamily = { onToggleFamily(line.row.id) },
                                // Round 12: a shut drawer animates nothing.
                                visible = open,
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
            fleetCount = delegatedCount,
            onOpenFleet = onOpenFleet,
            onOpenSettings = onOpenSettings,
        )
    }
}

/**
 * A compose row, not a call to action: an icon square and "New chat", sitting
 * at the top of the list exactly like the desktop rail's (addition F, D3). The
 * long press still opens "New session with…", and the model and effort are a
 * tap away in the composer, so the gesture is never the only path.
 */
@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun NewSessionButton(onClick: () -> Unit, onLongClick: () -> Unit) {
    val colors = Forge.colors
    val label = stringResource(R.string.drawer_new_chat)
    val withLabel = stringResource(R.string.drawer_new_session_with)
    Box(
        modifier = Modifier
            .requiredSize(ForgeSize.touch)
            .combinedClickable(onClick = onClick, onLongClick = onLongClick)
            .semantics {
                contentDescription = label
                role = Role.Button
                customActions = listOf(CustomAccessibilityAction(withLabel) { onLongClick(); true })
            },
        contentAlignment = Alignment.Center,
    ) {
        Box(
            modifier = Modifier
                .size(ForgeSize.headerControl)
                .clip(ForgeShapes.cardTight)
                .background(colors.accentWash)
                .border(ForgeSize.hairline, colors.accentLine, ForgeShapes.cardTight),
            contentAlignment = Alignment.Center,
        ) {
            Icon(
                Icons.Rounded.Add,
                contentDescription = null,
                tint = colors.accent,
                modifier = Modifier.size(ForgeSize.iconSm),
            )
        }
    }
}

@Composable
private fun SearchField(query: String, onQuery: (String) -> Unit) {
    val colors = Forge.colors
    val type = Forge.type
    // The field is the target, so it keeps all 48 dp; the pill is *painted*
    // 40 dp behind it. Insetting with padding shrank the field itself to 40,
    // which is the same mistake in the other direction (verify-9 V4).
    val inset = with(LocalDensity.current) { ForgeSpace.xs.toPx() }
    val radius = with(LocalDensity.current) { ForgeSize.searchField.toPx() / 2f }
    val stroke = with(LocalDensity.current) { ForgeSize.hairline.toPx() }
    val fill = colors.surfaceControl
    val line = colors.border
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .height(ForgeSize.touch)
            .drawBehind {
                val corner = CornerRadius(radius, radius)
                drawRoundRect(
                    color = fill,
                    topLeft = Offset(0f, inset),
                    size = Size(size.width, size.height - inset * 2),
                    cornerRadius = corner,
                )
                drawRoundRect(
                    color = line,
                    topLeft = Offset(stroke / 2f, inset + stroke / 2f),
                    size = Size(size.width - stroke, size.height - inset * 2 - stroke),
                    cornerRadius = corner,
                    style = Stroke(width = stroke),
                )
            }
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
                .padding(horizontal = ForgeSpace.sm)
                // The field itself is the target and stays 48 dp; the pill
                // around it paints 40 (verify-9 V4).
                .heightIn(min = ForgeSize.touch),
            decorationBox = { inner ->
                Box(
                    modifier = Modifier.height(ForgeSize.searchField),
                    contentAlignment = Alignment.CenterStart,
                ) {
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
private fun DrawerFooter(
    fleetCount: Int,
    onOpenFleet: () -> Unit,
    onOpenSettings: () -> Unit,
) {
    val colors = Forge.colors
    Column {
        Box(
            Modifier
                .fillMaxWidth()
                .height(ForgeSize.hairline)
                .background(colors.border),
        )
        // Only when the roster actually carries delegated sessions: a door to
        // an empty room is worse than no door (addition F, D5).
        if (fleetCount > 0) {
            FooterRow(
                icon = Icons.Rounded.Hub,
                primary = stringResource(R.string.fleet_footer_agents, fleetCount),
                secondary = null,
                onClick = onOpenFleet,
            )
        }
        // One row. The model is a composer picker and the theme is on the top
        // bar, so neither needs a second home down here (addition F, D5).
        FooterRow(
            icon = Icons.Rounded.Tune,
            primary = stringResource(R.string.drawer_footer_settings),
            secondary = null,
            onClick = onOpenSettings,
        )
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
                    style = type.sessionMeta,
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

private const val PAGE_TRIGGER = 8
