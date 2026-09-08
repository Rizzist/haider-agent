package ai.diffforge.haider.ui.daemon

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The `session.fleet` / `session.observe` seam, against frames shaped exactly
 * as the Rust serialises them.
 *
 * Every golden here is written the way `frame.rs` emits it: fields with
 * `skip_serializing_if = "Option::is_none"` are **absent**, not null;
 * `folded_children` is skipped when it is zero (`is_zero_u32`); and
 * `lockdown_bound` / `lockdown_auto_hermetic_bound` are `#[serde(skip)]`, so
 * they never appear at all. Each test states a fact a permissive default would
 * have turned into a claim the daemon never made.
 */
class FleetWireShapeTest {

    /** `ResponseBody::SessionFleet` as the daemon sends it (frame.rs:4699). */
    private val boundedFrame = """
    {
      "snapshot": {
        "session_id": "s-parent",
        "generated_at_ms": 1772000000000,
        "node_limit": 64,
        "depth_limit": 4,
        "roots": [
          {
            "agent_id": "agent-scout",
            "session_id": "s-child-a",
            "callsign": "scout",
            "model": "claude-sonnet-4-5",
            "provider": "anthropic",
            "task": "Map every transport call site",
            "depth": 1,
            "parent_session_id": "s-parent",
            "state": "live",
            "metrics": {
              "session_id": "s-child-a",
              "head_seq": 88,
              "started_at_ms": 1771999880000,
              "live": true,
              "tool_attempts": 14,
              "usage": {
                "logical_input_tokens": 41200,
                "billed_output_tokens": 3100,
                "additional_reasoning_tokens": 0,
                "cache_read_tokens": 18000,
                "cache_write_tokens": 900
              }
            },
            "children": [
              {
                "agent_id": "agent-probe",
                "session_id": "s-child-a1",
                "task": "Read the frame tests",
                "depth": 2,
                "parent_session_id": "s-child-a",
                "parent_agent_id": "agent-scout",
                "state": "queued"
              }
            ]
          },
          {
            "agent_id": "agent-scribe",
            "session_id": "s-child-c",
            "callsign": "scribe",
            "task": "Write the migration note",
            "depth": 1,
            "parent_session_id": "s-parent",
            "state": "done",
            "folded_children": 2
          }
        ],
        "rollup": {
          "node_count": 3,
          "states": {"queued": 1, "live": 1, "waiting": 0, "done": 1, "failed": 0, "cancelled": 0},
          "max_depth": 2,
          "metrics": {"elapsed_ms": 214000, "tool_attempts": 17},
          "metrics_complete": false,
          "complete": false
        },
        "truncated": true
      }
    }
    """.trimIndent()

    private fun bounded(): FleetSnapshot = FleetRpcAdapter.parseFleet(JSONObject(boundedFrame))

    // ---------- lineage and identity ----------

    @Test
    fun `the tree parses with its real lineage coordinates`() {
        val snapshot = bounded()
        assertEquals("s-parent", snapshot.sessionId)
        assertEquals(2, snapshot.roots.size)
        val scout = snapshot.roots.first()
        assertEquals("agent-scout", scout.agentId)
        assertEquals("s-child-a", scout.sessionId)
        assertEquals("s-parent", scout.parentSessionId)
        assertEquals(1, scout.depth)
        val probe = scout.children.single()
        assertEquals("s-child-a", probe.parentSessionId)
        assertEquals("agent-scout", probe.parentAgentId)
        assertEquals(2, probe.depth)
    }

    @Test
    fun `an absent parent agent id stays unknown rather than being inferred`() {
        // The root child's `parent_agent_id` is absent in the frame: the parent
        // is the *session*, and no agent id may be invented for it.
        assertNull(bounded().roots.first().parentAgentId)
    }

    @Test
    fun `a node without a callsign gets a marked client fallback`() {
        val probe = bounded().roots.first().children.single()
        assertNull(probe.callsign)
        val label = FleetModel.label(probe)
        assertTrue(label.fallback)
        assertEquals("agent-probe", label.text)

        val scout = FleetModel.label(bounded().roots.first())
        assertFalse(scout.fallback)
        assertEquals("scout", scout.text)
    }

    @Test
    fun `a long agent id is shortened but never renamed`() {
        val label = FleetModel.label(null, "agent-7f2c91ab4de0f1")
        assertTrue(label.fallback)
        assertEquals(FleetModel.AGENT_ID_FALLBACK_LENGTH, label.text.length)
        assertTrue("agent-7f2c91ab4de0f1".startsWith(label.text))
    }

    // ---------- folded_children is honest ----------

    @Test
    fun `a node with no children and folded children is bounded, not a leaf`() {
        val scribe = bounded().roots.last()
        assertTrue(scribe.children.isEmpty())
        assertEquals(2, scribe.foldedChildren)
        assertTrue(scribe.bounded)
        assertFalse(scribe.leaf)
    }

    @Test
    fun `an omitted folded children field really is zero`() {
        // `is_zero_u32` skips the field, so absence here is the wire's own
        // default rather than an unknown — and only then is a leaf a leaf.
        val probe = bounded().roots.first().children.single()
        assertEquals(0, probe.foldedChildren)
        assertTrue(probe.leaf)
        assertFalse(probe.bounded)
    }

    // ---------- completeness is tri-state ----------

    @Test
    fun `truncated with an incomplete rollup reads as bounded`() {
        assertEquals(FleetCompleteness.Bounded, FleetModel.completeness(bounded()))
    }

    @Test
    fun `only an explicit not-truncated and complete pair reads as complete`() {
        val frame = """
        {"snapshot":{"session_id":"s","roots":[],
         "rollup":{"node_count":0,"states":{},"max_depth":0,
                   "metrics":{"elapsed_ms":0,"tool_attempts":0},
                   "metrics_complete":true,"complete":true},
         "truncated":false}}
        """.trimIndent()
        val snapshot = FleetRpcAdapter.parseFleet(JSONObject(frame))
        assertEquals(FleetCompleteness.Complete, FleetModel.completeness(snapshot))
    }

    @Test
    fun `a snapshot that published no completeness flags is unknown`() {
        val snapshot = FleetRpcAdapter.parseFleet(JSONObject("""{"snapshot":{"session_id":"s"}}"""))
        assertNull(snapshot.truncated)
        assertNull(snapshot.rollup)
        // Not "complete": a client that presented this as the whole tree would
        // be making a claim the daemon never made.
        assertEquals(FleetCompleteness.Unknown, FleetModel.completeness(snapshot))
    }

    // ---------- metrics absence is no data, never zero ----------

    @Test
    fun `an absent usage total stays null rather than becoming zero`() {
        val rollup = bounded().rollup!!
        assertEquals(214_000L, rollup.elapsedMs)
        assertEquals(17L, rollup.toolAttempts)
        assertNull(rollup.usage)
        assertFalse(rollup.metricsComplete!!)
    }

    @Test
    fun `elapsed is measured against the snapshot instant, not the phone clock`() {
        val metrics = bounded().roots.first().metrics!!
        // live node: no terminal instant, so the snapshot's generated_at_ms is
        // the end of the interval.
        assertEquals(120_000L, metrics.elapsedMs(bounded().generatedAtMs))
        // With no snapshot instant either, elapsed is unknown — not zero.
        assertNull(metrics.elapsedMs(null))
    }

    @Test
    fun `a terminal node measures to its own terminal instant`() {
        val metrics = FleetNodeMetrics(startedAtMs = 1_000, terminalAtMs = 4_000)
        assertEquals(3_000L, metrics.elapsedMs(9_999_999))
    }

    // ---------- state words ----------

    @Test
    fun `an unrecognised state keeps its raw word and never lands on a known one`() {
        val view = FleetStateView.of("reticulating")
        assertEquals(FleetAgentState.Unknown, view.state)
        assertEquals("reticulating", view.raw)
        assertFalse(view.active)
        assertFalse(view.terminal)
    }

    @Test
    fun `a state the daemon never published is not the same as unknown`() {
        val absent = FleetStateView.of(null)
        assertEquals(FleetAgentState.Unknown, absent.state)
        assertNull(absent.raw)
    }

    @Test
    fun `queued live and waiting are the closed active set`() {
        listOf("queued", "live", "waiting").forEach {
            assertTrue(it, FleetStateView.of(it).active)
        }
        listOf("done", "failed", "cancelled").forEach {
            assertTrue(it, FleetStateView.of(it).terminal)
            assertFalse(it, FleetStateView.of(it).active)
        }
    }

    // ---------- the observe digest ----------

    /** `SessionObserveDigest.subagents` (frame.rs:2271). */
    private val observeFrame = """
    {
      "session_id": "s-parent",
      "head_seq": 612,
      "worker_generation": 5,
      "title": "Refactor the transport layer",
      "run_state": "running",
      "updated_at_ms": 1772000000000,
      "subagents": [
        {
          "agent_id": "agent-scout",
          "callsign": "scout",
          "task": "Map every transport call site",
          "state": "live",
          "provider": "anthropic",
          "lockdown": {
            "provider": "anthropic",
            "reason": "automatic_quota_guard",
            "tools_allowed": ["FsRead"],
            "quota_used": 12,
            "quota_limit": 40
          }
        },
        {
          "agent_id": "agent-7f2c91ab4de0",
          "task": "Re-run the wire fixtures",
          "state": "queued"
        }
      ]
    }
    """.trimIndent()

    @Test
    fun `observe chips parse with their own provider and lockdown`() {
        val chips = FleetRpcAdapter.parseSubagents(JSONObject(observeFrame))
        assertEquals(2, chips.size)
        val scout = chips.first()
        assertEquals("scout", scout.callsign)
        assertEquals("anthropic", scout.provider)
        assertEquals(FleetAgentState.Live, scout.state.state)
        assertEquals(12L, scout.lockdown!!.quotaUsed)
        assertEquals("automatic_quota_guard", scout.lockdown!!.reason)
    }

    @Test
    fun `an observe chip with no provider draws no provider mark`() {
        // `provider` is absent for observations reduced from manifests written
        // before provider lockdown state was exposed (frame.rs:2226): the chip
        // must not borrow the parent session's provider.
        val chip = FleetRpcAdapter.parseSubagents(JSONObject(observeFrame)).last()
        assertNull(chip.provider)
        assertNull(chip.callsign)
        assertNull(chip.lockdown)
        assertTrue(FleetModel.label(chip).fallback)
    }

    @Test
    fun `an observe digest with no subagents key lists none`() {
        val digest = JSONObject("""{"session_id":"s","head_seq":1,"worker_generation":1}""")
        assertTrue(FleetRpcAdapter.parseSubagents(digest).isEmpty())
    }

    @Test
    fun `a chip carries no session id, so a transcript needs the fleet snapshot`() {
        // `ObserveSubagentWire` has no session_id at all (frame.rs:2219). The
        // pairing exists only in the fleet snapshot, and an agent the bounded
        // snapshot omitted stays unaddressable rather than borrowing one.
        val chips = FleetRpcAdapter.parseSubagents(JSONObject(observeFrame))
        val roots = bounded().roots
        assertEquals("s-child-a", FleetModel.sessionOf(roots, chips.first().agentId))
        assertNull(FleetModel.sessionOf(roots, chips.last().agentId))
        assertNull(FleetModel.sessionOf(roots, null))
    }

    // ---------- selection and feeds ----------

    @Test
    fun `find uses the real agent id and never substitutes a neighbour`() {
        val roots = bounded().roots
        assertEquals("s-child-a1", FleetModel.find(roots, "agent-probe")!!.sessionId)
        assertNull(FleetModel.find(roots, "agent-nope"))
        assertNull(FleetModel.find(roots, ""))
    }

    @Test
    fun `descendant session ids come out in tree order, deduped`() {
        assertEquals(
            listOf("s-child-a", "s-child-a1", "s-child-c"),
            FleetModel.sessionIds(bounded().roots),
        )
    }

    @Test
    fun `the aggregate fold puts a waiting child ahead of a failure`() {
        val states = listOf("done", "failed", "waiting", "live").map(FleetStateView::of)
        assertEquals(FleetAgentState.Waiting, FleetModel.aggregate(states)!!.state)
        assertNull(FleetModel.aggregate(emptyList()))
    }

    // ---------- addressing ----------

    @Test
    fun `a node without a published parent session cannot be addressed`() {
        val node = FleetNode(agentId = "a", sessionId = "s", parentSessionId = null)
        assertFalse(node.addressable)
        assertTrue(node.copy(parentSessionId = "p").addressable)
        assertFalse(node.copy(agentId = "", parentSessionId = "p").addressable)
    }

    // ---------- the facade's own default ----------

    @Test
    fun `an unwired fleet seam is unavailable, not an empty tree`() {
        val load: FleetLoad = FleetLoad.Unavailable(FLEET_NOT_WIRED)
        assertEquals(FLEET_NOT_WIRED, (load as FleetLoad.Unavailable).reason)
        // Unread and "the daemon said none" are different claims too: an
        // unread fleet carries no snapshot to read an empty root list from.
        val unread: FleetLoad = FleetLoad.Unread
        assertNull((unread as? FleetLoad.Snapshot)?.snapshot)
        assertTrue(SubagentLoad.Unread.subagents.isEmpty())
        assertTrue(SubagentLoad.Unavailable(SUBAGENTS_NOT_WIRED).subagents.isEmpty())
    }
}
