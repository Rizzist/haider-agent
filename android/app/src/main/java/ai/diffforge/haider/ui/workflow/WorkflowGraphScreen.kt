package ai.diffforge.haider.ui.workflow

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.components.ForgeButton
import ai.diffforge.haider.ui.components.ForgeButtonKind
import ai.diffforge.haider.ui.components.ForgeChip
import ai.diffforge.haider.ui.components.ForgeIconButton
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.gestures.detectTransformGestures
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.selection.selectable
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.AccountTree
import androidx.compose.material.icons.rounded.Adjust
import androidx.compose.material.icons.rounded.ArrowBack
import androidx.compose.material.icons.rounded.CallMerge
import androidx.compose.material.icons.rounded.CenterFocusStrong
import androidx.compose.material.icons.rounded.Loop
import androidx.compose.material.icons.rounded.Refresh
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.clipToBounds
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.PathEffect
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.drawscope.DrawScope
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.clearAndSetSemantics
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Dp
import kotlin.math.max
import kotlin.math.min

const val WORKFLOW_GRAPH_TAG = "workflow_graph_canvas"
const val WORKFLOW_AST_TAG = "workflow_ast_tree"
const val WORKFLOW_NODE_TAG_PREFIX = "workflow_node_"

private const val GRAPH_MIN_ZOOM = 0.5f
private const val GRAPH_MAX_ZOOM = 3f

/**
 * The live workflow graph.
 *
 * The topology is the frozen activation ast; the colours are the projection's
 * per-node phases. Neither is computed here, and the two honesty rules that
 * cost the most to keep are visible in the code:
 *
 *  - an ast that published no edge list draws **no** edges and says so, rather
 *    than presenting a zero-edge topology as the daemon's answer;
 *  - tapping a node opens the child session the daemon recorded for that exact
 *    `parent_attempt`, or nothing at all. A node whose attempt has no attached
 *    child offers its evidence instead of a plausible neighbour.
 */
@Composable
fun WorkflowGraphScreen(
    state: WorkflowScreenState,
    onBack: () -> Unit,
    onToggleAst: () -> Unit,
    onSelectNode: (String) -> Unit,
    onOpenChild: (ChildGraphLink) -> Unit,
    onRefresh: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = Forge.colors
    val type = Forge.type
    Column(modifier.fillMaxSize().background(colors.bg)) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .heightIn(min = ForgeSize.header)
                .padding(horizontal = ForgeSpace.xs),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(ForgeSpace.xxs),
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
            Column(Modifier.weight(1f).padding(start = ForgeSpace.xs)) {
                Text(
                    stringResource(R.string.workflow_title),
                    style = type.sessionTitle,
                    color = colors.text,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                Text(
                    state.sessionTitle.ifBlank { state.sessionId },
                    style = type.sessionMeta,
                    color = colors.textMuted,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
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

        val snapshot = state.snapshot
        if (snapshot != null) {
            ViewToggle(showAst = state.showAst, onToggleAst = onToggleAst)
        }

        Column(
            Modifier
                .weight(1f)
                .fillMaxWidth(),
        ) {
            when (val read = state.read) {
                // "Not asked" and "the daemon said there is none" are different
                // claims, and the screen makes exactly the one it can support.
                WorkflowGraphRead.Unread -> Hint(stringResource(R.string.workflow_unread))
                WorkflowGraphRead.NoGraph -> Hint(stringResource(R.string.workflow_none))
                is WorkflowGraphRead.Unavailable ->
                    Hint(stringResource(R.string.workflow_unavailable, read.reason))
                is WorkflowGraphRead.Graph -> GraphBody(
                    snapshot = read.snapshot,
                    state = state,
                    onSelectNode = onSelectNode,
                    onOpenChild = onOpenChild,
                )
            }
            state.error?.let { Hint(stringResource(R.string.workflow_read_failed, it)) }
        }
    }
}

@Composable
private fun ViewToggle(showAst: Boolean, onToggleAst: () -> Unit) {
    val colors = Forge.colors
    Row(
        Modifier
            .fillMaxWidth()
            .padding(horizontal = ForgeSpace.lg),
        horizontalArrangement = Arrangement.spacedBy(ForgeSpace.md),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        ForgeChip(
            onClick = { if (showAst) onToggleAst() },
            selected = !showAst,
            contentDescription = stringResource(R.string.workflow_tab_graph),
        ) {
            Icon(
                Icons.Rounded.AccountTree,
                contentDescription = null,
                tint = if (showAst) colors.textMuted else colors.accent,
                modifier = Modifier.size(ForgeSize.iconSm),
            )
            Text(
                stringResource(R.string.workflow_tab_graph),
                style = Forge.type.chip,
                color = if (showAst) colors.textMuted else colors.text,
            )
        }
        ForgeChip(
            onClick = { if (!showAst) onToggleAst() },
            selected = showAst,
            contentDescription = stringResource(R.string.workflow_tab_ast),
        ) {
            Icon(
                Icons.Rounded.CallMerge,
                contentDescription = null,
                tint = if (showAst) colors.accent else colors.textMuted,
                modifier = Modifier.size(ForgeSize.iconSm),
            )
            Text(
                stringResource(R.string.workflow_tab_ast),
                style = Forge.type.chip,
                color = if (showAst) colors.text else colors.textMuted,
            )
        }
    }
}

@Composable
private fun GraphBody(
    snapshot: WorkflowGraphSnapshot,
    state: WorkflowScreenState,
    onSelectNode: (String) -> Unit,
    onOpenChild: (ChildGraphLink) -> Unit,
) {
    val layout = remember(snapshot.ast) { WorkflowLayoutEngine.layout(snapshot.ast) }
    Column(Modifier.fillMaxSize()) {
        GraphFacts(snapshot = snapshot, layout = layout, watchCursor = state.watchCursor)
        Box(Modifier.weight(1f).fillMaxWidth()) {
            if (state.showAst) {
                AstTree(snapshot.ast)
            } else if (!snapshot.ast.edgesPublished) {
                // The ast published no edge list. Nodes are still real, so they
                // are listed; the topology is not drawn, and that is said.
                Column(Modifier.fillMaxSize().verticalScroll(rememberScrollState())) {
                    Hint(stringResource(R.string.workflow_edges_unpublished))
                    snapshot.nodes.forEach { node ->
                        NodeCard(
                            name = node.node,
                            node = node,
                            spec = snapshot.ast.nodes.firstOrNull { it.node == node.node },
                            hasChild = ChildGraphIndex
                                .latestLink(state.links, snapshot.graphId, node.node) != null,
                            selected = state.selectedNode == node.node,
                            onClick = { onSelectNode(node.node) },
                            modifier = Modifier.padding(
                                horizontal = ForgeSpace.lg,
                                vertical = ForgeSpace.xs,
                            ),
                        )
                    }
                }
            } else {
                DagCanvas(
                    snapshot = snapshot,
                    layout = layout,
                    links = state.links,
                    selectedNode = state.selectedNode,
                    onSelectNode = onSelectNode,
                )
            }
        }
        state.selectedNode?.let { name ->
            NodePanel(
                name = name,
                node = snapshot.nodeState(name),
                spec = snapshot.ast.nodes.firstOrNull { it.node == name },
                link = ChildGraphIndex.latestLink(state.links, snapshot.graphId, name),
                onOpenChild = onOpenChild,
            )
        }
        if (state.recentEvents.isNotEmpty()) RecentActivity(state.recentEvents)
    }
}

@Composable
private fun GraphFacts(
    snapshot: WorkflowGraphSnapshot,
    layout: GraphLayout,
    watchCursor: String?,
) {
    val colors = Forge.colors
    val type = Forge.type
    val notPublished = stringResource(R.string.workflow_fact_not_published)
    Column(
        Modifier
            .fillMaxWidth()
            .padding(horizontal = ForgeSpace.lg, vertical = ForgeSpace.md),
        verticalArrangement = Arrangement.spacedBy(ForgeSpace.xs),
    ) {
        Text(
            stringResource(
                R.string.workflow_fact_phase_line,
                snapshot.phaseRaw ?: notPublished,
                snapshot.activeNodes.size,
            ),
            style = type.sessionTitle,
            color = colors.text,
        )
        // The topology fence, verbatim from the daemon. The graph drawn below is
        // the graph as of this digest; it is never recomputed here.
        val fence = stringResource(
            R.string.workflow_fact_fence,
            snapshot.astDigest ?: notPublished,
        )
        Text(
            fence,
            // mono: a daemon-issued digest a person compares character by character.
            style = type.numeric,
            color = colors.textMuted,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
        Text(
            stringResource(
                R.string.workflow_fact_cursors,
                snapshot.throughCursor ?: notPublished,
                watchCursor ?: stringResource(R.string.workflow_watch_cursor_none),
            ),
            style = type.sessionMeta,
            color = colors.textMuted,
        )
        Text(
            stringResource(
                R.string.workflow_fact_edges,
                if (snapshot.ast.edgesPublished) snapshot.ast.edges.size else 0,
                layout.backwardRoutes,
                snapshot.backEdgeActivations ?: 0,
            ),
            style = type.sessionMeta,
            color = colors.textMuted,
        )
    }
}

// ---------- the canvas ----------

@Composable
private fun DagCanvas(
    snapshot: WorkflowGraphSnapshot,
    layout: GraphLayout,
    links: List<ChildGraphLink>,
    selectedNode: String?,
    onSelectNode: (String) -> Unit,
) {
    val colors = Forge.colors
    val nodeW = ForgeSize.graphNodeWidth
    val nodeH = ForgeSize.graphNodeHeight
    val colGap = ForgeSize.graphColumnGap
    val layerGap = ForgeSize.graphLayerGap
    val pad = ForgeSize.graphCanvasPad
    val cols = layout.widestLayer.coerceAtLeast(1)
    val slotW = nodeW + colGap
    val slotH = nodeH + layerGap
    val contentW = pad * 2 + nodeW * cols + colGap * (cols - 1)
    val contentH = pad * 2 + nodeH * layout.layers + layerGap * (layout.layers - 1)

    fun originOf(placement: GraphPlacement): Pair<Dp, Dp> {
        val indent = slotW * ((cols - placement.columnsInLayer) / 2f)
        return (pad + indent + slotW * placement.column) to (pad + slotH * placement.layer)
    }

    var scale by remember { mutableFloatStateOf(1f) }
    var offset by remember { mutableStateOf(Offset.Zero) }
    val density = LocalDensity.current

    BoxWithConstraints(
        Modifier
            .fillMaxSize()
            .clipToBounds()
            .testTag(WORKFLOW_GRAPH_TAG),
    ) {
        val viewW = with(density) { maxWidth.toPx() }
        val viewH = with(density) { maxHeight.toPx() }
        val fullW = with(density) { contentW.toPx() }
        val fullH = with(density) { contentH.toPx() }
        Box(
            Modifier
                .fillMaxSize()
                .pointerInput(fullW, fullH, viewW, viewH) {
                    detectTransformGestures { _, pan, zoom, _ ->
                        scale = (scale * zoom).coerceIn(GRAPH_MIN_ZOOM, GRAPH_MAX_ZOOM)
                        // The graph cannot be panned off the screen: a gesture
                        // that loses the content is a gesture the user has to
                        // undo, and there is no scrollbar here to find it with.
                        val slackX = max(0f, fullW * scale - viewW)
                        val slackY = max(0f, fullH * scale - viewH)
                        offset = Offset(
                            x = min(0f, max(-slackX, offset.x + pan.x)),
                            y = min(0f, max(-slackY, offset.y + pan.y)),
                        )
                    }
                },
        ) {
            Box(
                Modifier
                    .width(contentW)
                    .height(contentH)
                    .graphicsLayer(
                        scaleX = scale,
                        scaleY = scale,
                        translationX = offset.x,
                        translationY = offset.y,
                        transformOrigin = androidx.compose.ui.graphics.TransformOrigin(0f, 0f),
                    ),
            ) {
                val forwardInk = colors.borderStrong
                val backInk = colors.amber
                val inputInk = colors.accentLine
                val unknownInk = colors.red
                Canvas(
                    // The lines are decoration for the cards, which carry the
                    // topology in their own descriptions; a screen reader is not
                    // served by a canvas it cannot enumerate.
                    Modifier.fillMaxSize().clearAndSetSemantics { },
                ) {
                    val nodeWpx = nodeW.toPx()
                    val nodeHpx = nodeH.toPx()
                    val padPx = pad.toPx()
                    val stroke = ForgeSize.graphEdge.toPx()
                    val arrow = ForgeSize.graphArrow.toPx()
                    val dashes = PathEffect.dashPathEffect(
                        floatArrayOf(ForgeSize.graphDash.toPx(), ForgeSize.graphDashGap.toPx()),
                    )
                    layout.routes.forEach { route ->
                        val to = route.to ?: return@forEach
                        val (toXdp, toYdp) = originOf(to)
                        val toX = toXdp.toPx()
                        val toY = toYdp.toPx()
                        val ink = when {
                            route.edge.kind == WorkflowEdgeKind.Unknown -> unknownInk
                            route.backward -> backInk
                            route.edge.kind == WorkflowEdgeKind.GraphInput -> inputInk
                            else -> forwardInk
                        }
                        val from = route.from
                        if (from == null) {
                            // A graph-input edge has no source node: a stub
                            // from the canvas edge, never a fabricated node.
                            val x = toX + nodeWpx / 2f
                            drawLine(
                                color = ink,
                                start = Offset(x, toY - padPx * 0.7f),
                                end = Offset(x, toY),
                                strokeWidth = stroke,
                                cap = StrokeCap.Round,
                                pathEffect = dashes,
                            )
                            drawDownArrow(Offset(x, toY), arrow, ink)
                            return@forEach
                        }
                        val (fromXdp, fromYdp) = originOf(from)
                        val fromX = fromXdp.toPx()
                        val fromY = fromYdp.toPx()
                        if (route.backward) {
                            val gutter = size.width - padPx * 0.35f
                            val startPoint = Offset(fromX + nodeWpx, fromY + nodeHpx / 2f)
                            val endPoint = Offset(toX + nodeWpx, toY + nodeHpx / 2f)
                            // Out to the gutter, up the side, back in: the
                            // return path is drawn where a forward edge never
                            // is, so a retry cannot be mistaken for progress.
                            val path = Path().apply {
                                moveTo(startPoint.x, startPoint.y)
                                cubicTo(
                                    gutter, startPoint.y,
                                    gutter, endPoint.y,
                                    endPoint.x, endPoint.y,
                                )
                            }
                            drawPath(
                                path = path,
                                color = ink,
                                style = Stroke(width = stroke, cap = StrokeCap.Round, pathEffect = dashes),
                            )
                            drawLeftArrow(endPoint, arrow, ink)
                        } else {
                            val startPoint = Offset(fromX + nodeWpx / 2f, fromY + nodeHpx)
                            val endPoint = Offset(toX + nodeWpx / 2f, toY)
                            val mid = (startPoint.y + endPoint.y) / 2f
                            val path = Path().apply {
                                moveTo(startPoint.x, startPoint.y)
                                cubicTo(
                                    startPoint.x, mid,
                                    endPoint.x, mid,
                                    endPoint.x, endPoint.y,
                                )
                            }
                            drawPath(
                                path = path,
                                color = ink,
                                style = Stroke(width = stroke, cap = StrokeCap.Round),
                            )
                            drawDownArrow(endPoint, arrow, ink)
                        }
                    }
                }
                layout.placements.forEach { placement ->
                    val (x, y) = originOf(placement)
                    NodeCard(
                        name = placement.node,
                        node = snapshot.nodeState(placement.node),
                        spec = snapshot.ast.nodes.firstOrNull { it.node == placement.node },
                        hasChild = ChildGraphIndex
                            .latestLink(links, snapshot.graphId, placement.node) != null,
                        selected = selectedNode == placement.node,
                        onClick = { onSelectNode(placement.node) },
                        modifier = Modifier.offset(x = x, y = y).width(nodeW),
                    )
                }
            }
        }
        if (scale != 1f || offset != Offset.Zero) {
            ForgeIconButton(
                onClick = { scale = 1f; offset = Offset.Zero },
                contentDescription = stringResource(R.string.cd_reset_zoom),
                modifier = Modifier.align(Alignment.BottomEnd).padding(ForgeSpace.md),
            ) {
                Icon(
                    Icons.Rounded.CenterFocusStrong,
                    contentDescription = null,
                    tint = colors.textSoft,
                    modifier = Modifier.size(ForgeSize.iconMd),
                )
            }
        }
    }
}

private fun DrawScope.drawDownArrow(tip: Offset, size: Float, color: Color) {
    val path = Path().apply {
        moveTo(tip.x, tip.y)
        lineTo(tip.x - size / 2f, tip.y - size)
        lineTo(tip.x + size / 2f, tip.y - size)
        close()
    }
    drawPath(path, color)
}

private fun DrawScope.drawLeftArrow(tip: Offset, size: Float, color: Color) {
    val path = Path().apply {
        moveTo(tip.x, tip.y)
        lineTo(tip.x + size, tip.y - size / 2f)
        lineTo(tip.x + size, tip.y + size / 2f)
        close()
    }
    drawPath(path, color)
}

// ---------- one node ----------

/**
 * A node card is its own 48 dp+ target: [ForgeSize.graphNodeHeight] is 62 dp, so
 * there is no separate touch box to keep in step with the paint.
 */
@Composable
private fun NodeCard(
    name: String,
    node: WorkflowNodeState?,
    spec: WorkflowAstNode?,
    hasChild: Boolean,
    selected: Boolean,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = Forge.colors
    val type = Forge.type
    val phase = node?.phase ?: WorkflowNodePhase.Unpublished
    val tone = phaseColor(phase)
    val word = phaseWord(phase, node?.phaseRaw)
    val spoken = buildList {
        add(name)
        add(word)
        node?.takeIf { it.iteration > 0 }?.let {
            add(stringResource(R.string.workflow_node_iteration, it.iteration))
        }
        if (spec?.convergenceGate == true) add(stringResource(R.string.workflow_node_convergence))
        if (spec?.join?.isJoin == true) {
            add(stringResource(R.string.workflow_node_join, spec.join.initialAll.size))
        }
        if (spec?.join?.reactivates == true) add(stringResource(R.string.workflow_node_reactivates))
        if (hasChild) add(stringResource(R.string.workflow_node_has_child))
    }.joinToString(", ")

    Row(
        modifier = modifier
            .height(ForgeSize.graphNodeHeight)
            .clip(ForgeShapes.card)
            .background(if (selected) colors.accentWash else colors.surfaceRaised)
            .border(
                ForgeSize.hairline,
                if (selected) colors.accent else colors.border,
                ForgeShapes.card,
            )
            .selectable(selected = selected, onClick = onClick)
            .semantics(mergeDescendants = true) { contentDescription = spoken }
            .testTag("$WORKFLOW_NODE_TAG_PREFIX$name"),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(
            Modifier
                .width(ForgeSize.graphNodeRail)
                .fillMaxHeight()
                .background(tone),
        )
        Column(
            Modifier
                .weight(1f)
                .padding(horizontal = ForgeSpace.md, vertical = ForgeSpace.sm),
            verticalArrangement = Arrangement.spacedBy(ForgeSpace.xxs),
        ) {
            // mono: a graph node name is the identifier the wire and the
            // journal use; it is matched by eye against a log, not read as prose.
            Text(
                name,
                style = type.toolStrong,
                color = colors.text,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(ForgeSpace.xs),
            ) {
                Box(
                    Modifier
                        .size(ForgeSize.graphPhaseDot)
                        .clip(ForgeShapes.pill)
                        .background(tone),
                )
                Text(
                    word,
                    style = type.sessionMeta,
                    color = colors.textSoft,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
        }
        Column(
            Modifier.padding(end = ForgeSpace.md),
            verticalArrangement = Arrangement.spacedBy(ForgeSpace.xxs),
            horizontalAlignment = Alignment.End,
        ) {
            if (spec?.join?.isJoin == true) {
                Icon(
                    Icons.Rounded.CallMerge,
                    contentDescription = null,
                    tint = colors.textMuted,
                    modifier = Modifier.size(ForgeSize.iconXs),
                )
            }
            if (spec?.join?.reactivates == true) {
                Icon(
                    Icons.Rounded.Loop,
                    contentDescription = null,
                    tint = colors.amber,
                    modifier = Modifier.size(ForgeSize.iconXs),
                )
            }
            if (spec?.convergenceGate == true) {
                Icon(
                    Icons.Rounded.Adjust,
                    contentDescription = null,
                    tint = colors.accentSoft,
                    modifier = Modifier.size(ForgeSize.iconXs),
                )
            }
        }
    }
}

// ---------- panels ----------

@Composable
private fun NodePanel(
    name: String,
    node: WorkflowNodeState?,
    spec: WorkflowAstNode?,
    link: ChildGraphLink?,
    onOpenChild: (ChildGraphLink) -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    Column(
        Modifier
            .fillMaxWidth()
            .background(colors.surface)
            .padding(horizontal = ForgeSpace.lg, vertical = ForgeSpace.md),
        verticalArrangement = Arrangement.spacedBy(ForgeSpace.xs),
    ) {
        // mono: the node identifier again, matched against the journal.
        Text(name, style = type.toolStrong, color = colors.text)
        Text(
            phaseWord(node?.phase ?: WorkflowNodePhase.Unpublished, node?.phaseRaw),
            style = type.sessionMeta,
            color = colors.textSoft,
        )
        spec?.let {
            Text(
                stringResource(R.string.workflow_node_types, it.inputType, it.outputType),
                style = type.sessionMeta,
                color = colors.textMuted,
            )
        }
        node?.rejection?.let { rejection ->
            Text(
                stringResource(
                    R.string.workflow_node_rejection,
                    rejection.code,
                    rejection.message,
                ),
                style = type.sessionMeta,
                color = colors.red,
            )
        }
        node?.takeIf { it.outputCount > 0 }?.let {
            Text(
                stringResource(R.string.workflow_node_evidence, it.outputCount),
                style = type.sessionMeta,
                color = colors.textMuted,
            )
        }
        node?.convergenceDigest?.let {
            // mono: a decision digest, compared rather than read.
            Text(
                stringResource(R.string.workflow_node_convergence_digest, it),
                style = type.numeric,
                color = colors.textMuted,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
        }
        if (link != null) {
            Text(
                stringResource(
                    R.string.workflow_node_child_attempt,
                    link.parentAttempt.attempt,
                    link.workflow ?: "",
                ),
                style = type.sessionMeta,
                color = colors.textMuted,
            )
            ForgeButton(
                text = stringResource(R.string.workflow_node_open_child),
                onClick = { onOpenChild(link) },
                kind = ForgeButtonKind.Ghost,
                minHeight = ForgeSize.bannerAction,
            )
        } else {
            Text(
                stringResource(R.string.workflow_node_child_none),
                style = type.sessionMeta,
                color = colors.textMuted,
            )
        }
    }
}

@Composable
private fun AstTree(ast: WorkflowAst) {
    val colors = Forge.colors
    val type = Forge.type
    val lines = remember(ast) { WorkflowAstTree.lines(ast) }
    Column(
        Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(horizontal = ForgeSpace.lg, vertical = ForgeSpace.md)
            .testTag(WORKFLOW_AST_TAG),
        verticalArrangement = Arrangement.spacedBy(ForgeSpace.xs),
    ) {
        Text(
            stringResource(R.string.workflow_ast_header, ast.workflowId, ast.inputType, ast.outputType),
            style = type.sessionMeta,
            color = colors.textMuted,
        )
        lines.forEach { line ->
            Row(
                Modifier
                    .fillMaxWidth()
                    .padding(start = ForgeSize.astIndent * line.depth),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(ForgeSpace.xs),
            ) {
                Box(
                    Modifier
                        .size(ForgeSize.graphPhaseDot)
                        .clip(ForgeShapes.pill)
                        .background(if (line.repeat) colors.border else colors.accentLine),
                )
                Column(Modifier.weight(1f)) {
                    // mono: the node identifier, aligned down the tree.
                    Text(
                        line.node,
                        style = type.toolRow,
                        color = if (line.repeat) colors.textMuted else colors.text,
                    )
                    Text(
                        if (line.repeat) {
                            stringResource(R.string.workflow_ast_repeat)
                        } else {
                            stringResource(
                                R.string.workflow_ast_line,
                                line.inputType,
                                line.outputType,
                            )
                        },
                        style = type.sessionMeta,
                        color = colors.textMuted,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                }
                if (line.join.isJoin) {
                    Icon(
                        Icons.Rounded.CallMerge,
                        contentDescription = stringResource(
                            R.string.workflow_node_join,
                            line.join.initialAll.size,
                        ),
                        tint = colors.textMuted,
                        modifier = Modifier.size(ForgeSize.iconXs),
                    )
                }
                if (line.join.reactivates) {
                    Icon(
                        Icons.Rounded.Loop,
                        contentDescription = stringResource(R.string.workflow_node_reactivates),
                        tint = colors.amber,
                        modifier = Modifier.size(ForgeSize.iconXs),
                    )
                }
            }
        }
    }
}

@Composable
private fun RecentActivity(events: List<WorkflowWatchEvent>) {
    val colors = Forge.colors
    val type = Forge.type
    Column(
        Modifier
            .fillMaxWidth()
            .padding(horizontal = ForgeSpace.lg, vertical = ForgeSpace.md),
        verticalArrangement = Arrangement.spacedBy(ForgeSpace.xxs),
    ) {
        Text(
            stringResource(R.string.workflow_recent),
            style = type.sessionMeta,
            color = colors.textSoft,
        )
        events.take(RECENT_ROWS).forEach { event ->
            Text(
                stringResource(
                    R.string.workflow_event_row,
                    event.cursor ?: "",
                    eventWord(event),
                ),
                style = type.sessionMeta,
                color = colors.textMuted,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
        }
    }
}

private const val RECENT_ROWS = 4

@Composable
private fun Hint(text: String) {
    Text(
        text,
        style = Forge.type.emptyBody,
        color = Forge.colors.textMuted,
        modifier = Modifier
            .fillMaxWidth()
            .widthIn(max = ForgeSize.proseMax)
            .padding(horizontal = ForgeSpace.xl, vertical = ForgeSpace.lg),
    )
}

// ---------- phase vocabulary ----------

/**
 * Phase → colour. The pin: nothing but a completed node is green, and neither
 * an unrecognised phase nor an unpublished one ever is.
 */
@Composable
fun phaseColor(phase: WorkflowNodePhase): Color {
    val colors = Forge.colors
    return when (phase) {
        WorkflowNodePhase.Waiting -> colors.stateIdle
        WorkflowNodePhase.Activated -> colors.accent
        WorkflowNodePhase.Completed -> colors.green
        WorkflowNodePhase.Rejected -> colors.red
        // A phase this build does not know, and one the daemon never published,
        // are both neutral. Guessing at either would colour a lie.
        WorkflowNodePhase.Unknown -> colors.stateUnknown
        WorkflowNodePhase.Unpublished -> colors.stateUnknown
    }
}

@Composable
fun phaseWord(phase: WorkflowNodePhase, raw: String?): String = when (phase) {
    WorkflowNodePhase.Waiting -> stringResource(R.string.workflow_phase_waiting)
    WorkflowNodePhase.Activated -> stringResource(R.string.workflow_phase_activated)
    WorkflowNodePhase.Completed -> stringResource(R.string.workflow_phase_completed)
    WorkflowNodePhase.Rejected -> stringResource(R.string.workflow_phase_rejected)
    // The daemon's own word, shown rather than translated into a phase this
    // build happens to have a name for.
    WorkflowNodePhase.Unknown -> raw.orEmpty()
    WorkflowNodePhase.Unpublished -> stringResource(R.string.workflow_phase_unpublished)
}

@Composable
private fun eventWord(event: WorkflowWatchEvent): String = when (event.kind) {
    WorkflowWatchEventKind.GraphStarted -> stringResource(R.string.workflow_event_started)
    WorkflowWatchEventKind.NodeActivated ->
        stringResource(R.string.workflow_event_activated, event.node.orEmpty())
    WorkflowWatchEventKind.NodeCompleted ->
        stringResource(R.string.workflow_event_completed, event.node.orEmpty())
    WorkflowWatchEventKind.NodeRejected ->
        stringResource(R.string.workflow_event_rejected, event.node.orEmpty())
    // An unrecognised journal fact is shown with its raw type, never dropped.
    WorkflowWatchEventKind.Unknown ->
        stringResource(R.string.workflow_event_unknown, event.typeRaw.orEmpty())
}
