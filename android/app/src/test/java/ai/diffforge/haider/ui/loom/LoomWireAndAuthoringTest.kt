package ai.diffforge.haider.ui.loom

import ai.diffforge.haider.ui.daemon.FakeWorkflowLoom
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The Loom registry's wire shapes and the authoring state machine.
 *
 * The two defects these pin are the ones that look like features: a `cli_present`
 * map read as a complete statement about the device, and a
 * `loom.author.confirm` that answered `confirmed: null` being treated as
 * success because it carried no errors either.
 */
class LoomWireAndAuthoringTest {

    // ---------- loom.list ----------

    private val listFrame = """
        {"agent_types":[
           {"id":"implementer","name":"Implementer","job":"Write the change.",
            "in_type":"Plan","out_type":"Patch","clis":["git","rg"],
            "denials":["cli:curl"],"color":"#3B82F6","glyph":"I","rev":4,"digest":"a91c33f0"},
           {"id":"shipper","name":"Shipper","job":"Land it.",
            "in_type":"Verdict","out_type":"Ship","clis":["gh"],"rev":1,"digest":"cc7715a3"}
         ],
         "workflows":[
           {"id":"implement_verify","pipe_version":"pipe/v1","source":"plan @planner :cmd",
            "in_type":"Task","out_type":"Ship","rev":7,"digest":"9f31c0a7",
            "template":{"name":"implement_verify","version":1,
                        "nodes":[{"name":"PLAN"},{"name":"SHIP"}]},
            "meta":[]}
         ],
         "cli_present":{"git":true,"gh":false}}
    """.trimIndent()

    @Test
    fun `cli presence is tri-state and absence is not a missing program`() {
        val registry = LoomRpcAdapter.parseList(JSONObject(listFrame))
        assertEquals(LoomCliPresence.Present, registry.cliPresence("git"))
        assertEquals(LoomCliPresence.Missing, registry.cliPresence("gh"))
        // "rg" is declared by an agent type but absent from the probe map. That
        // is NOT PROBED — an older daemon, or a name that arrived some other
        // way — and rendering it as missing would invent a device fact.
        assertTrue("rg" in registry.declaredClis)
        assertEquals(LoomCliPresence.NotProbed, registry.cliPresence("rg"))
    }

    @Test
    fun `archived is null after a default read and a list only when asked for`() {
        // An empty archived list after a default read would read as "nothing is
        // archived", which the response never said.
        assertNull(LoomRpcAdapter.parseList(JSONObject(listFrame)).archived)
        assertEquals(
            emptyList<LoomArchivedRef>(),
            LoomRpcAdapter.parseList(JSONObject(listFrame), includeArchived = true).archived,
        )
    }

    @Test
    fun `an agent type keeps its colour, glyph and withheld capabilities`() {
        val registry = LoomRpcAdapter.parseList(JSONObject(listFrame))
        val implementer = registry.agentTypes.single { it.id == "implementer" }
        assertEquals("#3B82F6", implementer.color)
        assertEquals("I", implementer.glyph)
        assertEquals(listOf("cli:curl"), implementer.denials)
        assertEquals(4, implementer.rev)

        // An entry with no colour and no glyph keeps both empty. A colour
        // hashed from the id would look exactly like a registry fact.
        val shipper = registry.agentTypes.single { it.id == "shipper" }
        assertEquals("", shipper.color)
        assertEquals("", shipper.glyph)
    }

    @Test
    fun `a workflow carries its pipe source and its template's node count`() {
        val workflow = LoomRpcAdapter.parseList(JSONObject(listFrame)).workflows.single()
        assertEquals("pipe/v1", workflow.pipeVersion)
        assertEquals("implement_verify", workflow.templateName)
        assertEquals(2, workflow.nodeCount)
        assertEquals(7, workflow.rev)
    }

    // ---------- archive ----------

    @Test
    fun `archive sends the revision it read and omits an unknown digest`() {
        val withDigest = LoomRpcAdapter.archiveRequest(
            archive = true,
            kind = LoomEntryKind.AgentType,
            id = "reviewer",
            fence = LoomFence(3, "b0d4e112"),
        )
        assertEquals("loom.archive", withDigest.getString("method"))
        assertEquals("agent_type", withDigest.getString("kind"))
        assertEquals(3, withDigest.getInt("expected_rev"))
        assertEquals("b0d4e112", withDigest.getString("expected_digest"))

        val weaker = LoomRpcAdapter.archiveRequest(
            archive = false,
            kind = LoomEntryKind.Workflow,
            id = "triage_only",
            fence = LoomFence(2, null),
        )
        assertEquals("loom.unarchive", weaker.getString("method"))
        // Digest absence is a typed weaker fence, not permission to invent one.
        assertFalse(weaker.has("expected_digest"))
    }

    @Test(expected = IllegalArgumentException::class)
    fun `an archive without a read revision cannot be built`() {
        // `expected_rev` is required on this door. A fence the client made up is
        // not a compare-and-set.
        LoomRpcAdapter.archiveRequest(true, LoomEntryKind.AgentType, "x", LoomFence(null, "d"))
    }

    // ---------- authoring wire ----------

    @Test
    fun `revise and confirm echo the draft's own fence`() {
        val draft = LoomAuthorDraft(
            authoringId = "authoring-7",
            revision = 3,
            kind = LoomAuthorKind.Workflow,
            text = "plan @planner :cmd",
        )
        val revise = LoomRpcAdapter.authorReviseRequest(draft, "plan @planner :cmd\n")
        assertEquals("loom.author.revise", revise.getString("method"))
        assertEquals("authoring-7", revise.getString("authoring_id"))
        // Echoed, never incremented: only the daemon mints this number.
        assertEquals(3L, revise.getLong("expected_revision"))
        assertEquals("workflow", revise.getString("kind"))

        val confirm = LoomRpcAdapter.authorConfirmRequest(draft, "plan @planner :cmd\n")
        assertEquals(3L, confirm.getLong("expected_revision"))
        // The registry fence is separate and optional; absent when unknown.
        assertFalse(confirm.has("expected_rev"))
        assertFalse(confirm.has("expected_digest"))

        val fenced = LoomRpcAdapter.authorConfirmRequest(draft, "x", LoomFence(7, "9f31c0a7"))
        assertEquals(7, fenced.getInt("expected_rev"))
        assertEquals("9f31c0a7", fenced.getString("expected_digest"))
    }

    @Test
    fun `a validation error keeps its one-based location verbatim`() {
        val validation = LoomRpcAdapter.parseValidation(
            JSONObject(
                """
                {"errors":[{"code":"unknown_agent_type","message":"No such agent type.",
                  "location":{"line":4,"column":18,"field":"nodes[0].agent_type"}}],
                 "canonical_digest":"preview-1234"}
                """.trimIndent(),
            ),
        )
        val error = validation.errors.single()
        assertEquals(4, error.line)
        assertEquals(18, error.column)
        assertEquals("nodes[0].agent_type", error.field)
        // Named a preview because `loom.validate` registers nothing: this is
        // not a digest stored on any registry entry.
        assertEquals("preview-1234", validation.canonicalDigestPreview)
    }

    @Test
    fun `confirmed null is parsed as no receipt, with its errors beside it`() {
        val (receipt, errors) = LoomRpcAdapter.parseConfirm(
            JSONObject(
                """
                {"confirmed":null,"errors":[{"code":"type_mismatch","message":"Patch is not Plan.",
                 "location":{"line":2,"column":1,"field":"nodes[1].in_type"}}]}
                """.trimIndent(),
            ),
        )
        assertNull(receipt)
        assertEquals(1, errors.size)
    }

    @Test
    fun `a confirm receipt carries the registration and the daemon's execution digest`() {
        val (receipt, errors) = LoomRpcAdapter.parseConfirm(
            JSONObject(
                """
                {"confirmed":{"authoring_id":"authoring-7","kind":"agent_type",
                  "canonical_text":"{}","registration":{"id":"planner","rev":1,
                  "digest":"aabb","updated":true},"execution_digest":"exec-aabb",
                  "install_job_id":"job-planner-1"}}
                """.trimIndent(),
            ),
        )
        assertTrue(errors.isEmpty())
        assertEquals("planner", receipt!!.registrationId)
        assertEquals(1, receipt.rev)
        assertTrue(receipt.updated)
        // Daemon-issued. A client never computes this value.
        assertEquals("exec-aabb", receipt.executionDigest)
        assertEquals("job-planner-1", receipt.installJobId)
    }

    @Test
    fun `a cancelled install job is not reported as a failure`() {
        val jobs = LoomRpcAdapter.parseInstallJobs(
            JSONObject(
                """
                {"jobs":[{"job_id":"job-1","agent_type_id":"reviewer","state":"failed",
                  "cancelled":true,"progress":{},"created_at_ms":1,"updated_at_ms":2}]}
                """.trimIndent(),
            ),
        )
        val job = jobs.single()
        // The legacy `state` stays `failed` as a frozen-enum carrier; a client
        // that ignores `cancelled` shows a cancel as a failure and offers a
        // retry for something nobody wants retried.
        assertEquals("cancelled", job.stateRaw)
        assertFalse(job.retryable)
    }

    // ---------- the authoring state machine ----------

    private fun draft(errors: List<LoomAuthorError> = emptyList()) = LoomAuthorDraft(
        authoringId = "authoring-1",
        revision = 1,
        kind = LoomAuthorKind.AgentType,
        text = "{}",
        errors = errors,
    )

    private val someError = LoomAuthorError("syntax", "bad", 1, 1, "id")

    @Test
    fun `the cycle runs prompt to draft to revise to confirm`() {
        var state: LoomAuthoringState =
            LoomAuthoringMachine.requestDraft(LoomAuthorKind.AgentType, "a planner")
        assertTrue(state is LoomAuthoringState.Drafting)

        state = LoomAuthoringMachine.drafted(draft(listOf(someError)))
        assertTrue(state is LoomAuthoringState.Editing)
        // A draft carrying errors is not confirmable: the round trip would be
        // the user's time spent asking for a refusal.
        assertFalse(LoomAuthoringMachine.canConfirm(state))

        state = LoomAuthoringMachine.edited(state, "{\"id\":\"planner\"}")
        state = LoomAuthoringMachine.requestRevise(state)
        assertTrue((state as LoomAuthoringState.Editing).busy)

        state = LoomAuthoringMachine.revised(draft().copy(revision = 2, text = "{\"id\":\"planner\"}"))
        assertTrue(LoomAuthoringMachine.canConfirm(state))
        assertEquals(2L, LoomAuthoringMachine.draftOf(state)!!.revision)

        state = LoomAuthoringMachine.requestConfirm(state)
        assertTrue(state is LoomAuthoringState.Confirming)
    }

    @Test
    fun `confirmed null leaves the draft on screen and registers nothing`() {
        val editing = LoomAuthoringMachine.drafted(draft())
        val confirming = LoomAuthoringMachine.requestConfirm(editing)
        val answered = LoomAuthoringMachine.confirmAnswered(
            confirming,
            receipt = null,
            errors = listOf(someError),
            reason = "type_mismatch",
        )
        // The pin: a null receipt is a Refusal, never a Confirmation — and the
        // fence survives so the text can be fixed and re-sent.
        assertTrue(answered is LoomAuthoringState.Refused)
        assertEquals(1, LoomAuthoringMachine.errorsOf(answered).size)
        assertNotNull(LoomAuthoringMachine.draftOf(answered))
        assertFalse(answered is LoomAuthoringState.Confirmed)
    }

    @Test
    fun `a refusal is editable again and does not lose the fence`() {
        val refused = LoomAuthoringState.Refused(draft(), "{}", listOf(someError), null)
        val edited = LoomAuthoringMachine.edited(refused, "{\"id\":\"x\"}")
        assertTrue(edited is LoomAuthoringState.Editing)
        assertEquals("authoring-1", LoomAuthoringMachine.draftOf(edited)!!.authoringId)
    }

    @Test
    fun `revise without a draft sends nothing at all`() {
        // There is no `authoring_id` to echo, and one cannot be made up.
        assertEquals(
            LoomAuthoringState.Idle,
            LoomAuthoringMachine.requestRevise(LoomAuthoringState.Idle),
        )
        assertNull(LoomAuthoringMachine.draftOf(LoomAuthoringState.Idle))
    }

    @Test
    fun `an unavailable daemon never becomes a draft`() {
        val state = LoomAuthoringState.Unavailable("no_model_selected")
        assertNull(LoomAuthoringMachine.draftOf(state))
        assertFalse(LoomAuthoringMachine.canConfirm(state))
        assertTrue(LoomAuthoringMachine.errorsOf(state).isEmpty())
    }

    // ---------- the fake's authoring cycle ----------

    @Test
    fun `the scripted daemon refuses a bad draft and registers a fixed one`() {
        val fake = FakeWorkflowLoom()
        val first = fake.authorDraft(LoomAuthorKind.AgentType, "a planner")
        // The first draft has a real defect, because that is the case revise
        // exists for.
        assertTrue(first.errors.isNotEmpty())

        val (refused, errors) = fake.authorConfirm(first, first.text)
        assertNull(refused)
        assertTrue(errors.isNotEmpty())

        val fixed = first.text.replace(FakeWorkflowLoom.BAD_MARKER, "rg")
        val revised = fake.authorRevise(first, fixed)
        assertTrue(revised.errors.isEmpty())
        // The daemon minted the next revision; the client echoed the old one.
        assertEquals(first.revision + 1, revised.revision)

        val (receipt, none) = fake.authorConfirm(revised, revised.text)
        assertTrue(none.isEmpty())
        assertEquals("planner", receipt!!.registrationId)
        // The new type is in the registry the list reads back.
        assertTrue(fake.loomList(false).agentTypes.any { it.id == "planner" })
    }

    @Test
    fun `a stale authoring fence loses`() {
        val fake = FakeWorkflowLoom()
        val first = fake.authorDraft(LoomAuthorKind.Workflow, "just plan")
        fake.authorRevise(first, first.text)
        val thrown = runCatching { fake.authorRevise(first, first.text) }.exceptionOrNull()
        assertNotNull(thrown)
    }

    @Test
    fun `an archive with the wrong revision loses the compare-and-set`() {
        val fake = FakeWorkflowLoom()
        val reviewer = fake.loomList(false).agentTypes.single { it.id == "reviewer" }
        assertFalse(
            fake.setArchived(
                LoomEntryKind.AgentType,
                "reviewer",
                archived = true,
                fence = LoomFence(reviewer.rev!! - 1, reviewer.digest),
            ),
        )
        assertTrue(
            fake.setArchived(
                LoomEntryKind.AgentType,
                "reviewer",
                archived = true,
                fence = LoomFence.of(reviewer),
            ),
        )
        assertTrue(fake.loomList(true).agentTypes.single { it.id == "reviewer" }.archived)
    }
}
