package ai.diffforge.haider.ui.loom

import org.json.JSONArray
import org.json.JSONObject

/**
 * The one file that knows the Loom wire shapes.
 *
 * | door | Rust | canonical shape |
 * |---|---|---|
 * | `loom.list` | req frame.rs:4772 / resp frame.rs:3534 | `{include_archived?}` → `{agent_types[], workflows[], cli_present{}, workflow_catalog[], archived_entries[]}` — every collection is `skip_serializing_if empty`, so absent is empty **except** `cli_present`, where an absent key means NOT PROBED |
 * | `loom.registered` | resp frame.rs:4799 | `{registration: {id, rev, digest, updated}, install_job_id?}` |
 * | `loom.register_agent_type` | frame.rs:3541 | `{record: LoomAgentType, expected_rev?, expected_digest?}` |
 * | `loom.register_workflow` | frame.rs:3561 | `{source, expected_rev?, expected_digest?}` — pipe source, **not** a template |
 * | `loom.archive` / `loom.unarchive` | frame.rs:4358/:4366 | `{kind, id, expected_rev, expected_digest?}` → `{receipt:{kind,id,outcome:{status,entry?}}}` |
 * | `loom.validate` | frame.rs:4375 | `{kind, text}` → `{errors[], canonical_digest?}` — non-mutating |
 * | `loom.watch` | frame.rs:4381 | `{after_cursor}` → `{watch_id, requested_after_cursor, baseline: LoomRegistrySnapshot}` |
 * | `loom.install.status` | frame.rs:3551 | `{job_id?, agent_type_id?}` → `{jobs[], items[]}` |
 * | `loom.author.draft` | frame.rs:4327 | `{session_id, kind, prose}` — the session supplies the provider/model; the exchange does **not** append to its journal |
 * | `loom.author.revise` | frame.rs:4336 | `{authoring_id, expected_revision, kind, text}` |
 * | `loom.author.confirm` | frame.rs:4344 | `{authoring_id, expected_revision, kind, text, expected_rev?, expected_digest?}` → `{confirmed?, errors[]}` |
 *
 * `expected_rev` / `expected_digest` are `skip_serializing_if` on every door
 * that takes them: an unknown fence is **omitted**, never sent as null and
 * never invented. `loom.archive` is the exception — its `expected_rev` is
 * required, so an archive without a read revision cannot be sent at all.
 */
object LoomRpcAdapter {

    const val METHOD_LIST = "loom.list"
    const val METHOD_REGISTER_AGENT_TYPE = "loom.register_agent_type"
    const val METHOD_REGISTER_WORKFLOW = "loom.register_workflow"
    const val METHOD_ARCHIVE = "loom.archive"
    const val METHOD_UNARCHIVE = "loom.unarchive"
    const val METHOD_VALIDATE = "loom.validate"
    const val METHOD_WATCH = "loom.watch"
    const val METHOD_INSTALL_STATUS = "loom.install.status"
    const val METHOD_INSTALL_RETRY = "loom.install.retry"
    const val METHOD_INSTALL_CANCEL = "loom.install.cancel"
    const val METHOD_AUTHOR_DRAFT = "loom.author.draft"
    const val METHOD_AUTHOR_REVISE = "loom.author.revise"
    const val METHOD_AUTHOR_CONFIRM = "loom.author.confirm"

    /** Feature names (frame.rs:507-525), used as honest unavailable reasons. */
    const val FEATURE_LOOM_V1 = "loom_v1"
    const val FEATURE_LOOM_AUTHORING_V1 = "loom_authoring_v1"
    const val FEATURE_LOOM_REGISTRY_ARCHIVE_V1 = "loom_registry_archive_v1"
    const val FEATURE_LOOM_CLI_PRESENCE_V1 = "loom_cli_presence_v1"

    /** `LoomAuthorKind` (loom.rs:385) is snake_case on the wire. */
    fun kindWire(kind: LoomAuthorKind): String = when (kind) {
        LoomAuthorKind.AgentType -> "agent_type"
        LoomAuthorKind.Workflow -> "workflow"
    }

    fun entryKind(raw: String?): LoomEntryKind = when (raw) {
        "agent_type" -> LoomEntryKind.AgentType
        "workflow" -> LoomEntryKind.Workflow
        else -> LoomEntryKind.Unknown
    }

    fun authorKind(raw: String?): LoomAuthorKind? = when (raw) {
        "agent_type" -> LoomAuthorKind.AgentType
        "workflow" -> LoomAuthorKind.Workflow
        else -> null
    }

    // ---------- requests ----------

    fun listRequest(includeArchived: Boolean = false): JSONObject = JSONObject()
        .put("method", METHOD_LIST)
        .also { if (includeArchived) it.put("include_archived", true) }

    fun installStatusRequest(jobId: String? = null, agentTypeId: String? = null): JSONObject =
        JSONObject()
            .put("method", METHOD_INSTALL_STATUS)
            .also { if (!jobId.isNullOrEmpty()) it.put("job_id", jobId) }
            .also { if (!agentTypeId.isNullOrEmpty()) it.put("agent_type_id", agentTypeId) }

    /**
     * `expected_rev` is required here, so a fence without one is refused before
     * it becomes a request. Archiving an entry whose revision was never read is
     * a compare-and-set against a value the client made up.
     */
    fun archiveRequest(
        archive: Boolean,
        kind: LoomEntryKind,
        id: String,
        fence: LoomFence,
    ): JSONObject {
        val rev = requireNotNull(fence.expectedRev) { "archive needs the revision it read" }
        return JSONObject()
            .put("method", if (archive) METHOD_ARCHIVE else METHOD_UNARCHIVE)
            .put("kind", entryKindWire(kind))
            .put("id", id)
            .put("expected_rev", rev)
            .also { obj -> fence.expectedDigest?.let { obj.put("expected_digest", it) } }
    }

    fun validateRequest(kind: LoomAuthorKind, text: String): JSONObject = JSONObject()
        .put("method", METHOD_VALIDATE)
        .put("kind", kindWire(kind))
        .put("text", text)

    fun authorDraftRequest(sessionId: String, kind: LoomAuthorKind, prose: String): JSONObject =
        JSONObject()
            .put("method", METHOD_AUTHOR_DRAFT)
            .put("session_id", sessionId)
            .put("kind", kindWire(kind))
            .put("prose", prose)

    /** The fence is echoed from the draft the daemon issued, never incremented. */
    fun authorReviseRequest(draft: LoomAuthorDraft, text: String): JSONObject = JSONObject()
        .put("method", METHOD_AUTHOR_REVISE)
        .put("authoring_id", draft.authoringId)
        .put("expected_revision", draft.revision)
        .put("kind", kindWire(draft.kind))
        .put("text", text)

    fun authorConfirmRequest(
        draft: LoomAuthorDraft,
        text: String,
        registryFence: LoomFence? = null,
    ): JSONObject = JSONObject()
        .put("method", METHOD_AUTHOR_CONFIRM)
        .put("authoring_id", draft.authoringId)
        .put("expected_revision", draft.revision)
        .put("kind", kindWire(draft.kind))
        .put("text", text)
        .also { obj ->
            registryFence?.expectedRev?.let { obj.put("expected_rev", it) }
            registryFence?.expectedDigest?.let { obj.put("expected_digest", it) }
        }

    private fun entryKindWire(kind: LoomEntryKind): String = when (kind) {
        LoomEntryKind.AgentType -> "agent_type"
        LoomEntryKind.Workflow -> "workflow"
        LoomEntryKind.Unknown -> "unknown"
    }

    // ---------- parsing ----------

    private fun JSONObject.stringOrNull(key: String): String? =
        if (has(key) && !isNull(key)) optString(key).takeIf { it.isNotEmpty() } else null

    private fun JSONObject.intOrNull(key: String): Int? =
        if (has(key) && !isNull(key)) optInt(key) else null

    private fun JSONArray.objects(): List<JSONObject> =
        (0 until length()).mapNotNull { optJSONObject(it) }

    private fun JSONArray.strings(): List<String> =
        (0 until length()).map { optString(it) }.filter { it.isNotEmpty() }

    private fun JSONObject.stringList(key: String): List<String> =
        optJSONArray(key)?.strings().orEmpty()

    fun parseAgentType(record: JSONObject, archived: Boolean = false): LoomAgentTypeEntry =
        LoomAgentTypeEntry(
            id = record.optString("id"),
            name = record.optString("name"),
            job = record.optString("job"),
            inType = record.optString("in_type"),
            outType = record.optString("out_type"),
            clis = record.stringList("clis"),
            apis = record.stringList("apis"),
            denials = record.stringList("denials"),
            skills = record.stringList("skills"),
            scripts = record.stringList("scripts"),
            // Empty is the wire's own default; it is never replaced with a
            // colour or a letter this client picked (loom.rs:66-69).
            color = record.optString("color"),
            glyph = record.optString("glyph"),
            rev = record.intOrNull("rev"),
            digest = record.stringOrNull("digest"),
            archived = archived || record.optBoolean("archived", false),
        )

    fun parseWorkflow(record: JSONObject, archived: Boolean = false): LoomWorkflowEntry {
        val template = record.optJSONObject("template")
        return LoomWorkflowEntry(
            id = record.optString("id"),
            pipeVersion = record.optString("pipe_version"),
            source = record.optString("source"),
            inType = record.optString("in_type"),
            outType = record.optString("out_type"),
            templateName = template?.optString("name").orEmpty(),
            nodeCount = template?.optJSONArray("nodes")?.length() ?: 0,
            rev = record.intOrNull("rev"),
            digest = record.stringOrNull("digest"),
            archived = archived || record.optBoolean("archived", false),
        )
    }

    /**
     * `loom.list`.
     *
     * `archived_entries` is "present only when the request explicitly included
     * archived entries" (frame.rs:4795), so [includeArchived] — the client's own
     * request — decides whether [LoomRegistry.archived] is a list or null. An
     * empty list after a default read is not evidence that nothing is archived.
     */
    fun parseList(response: JSONObject, includeArchived: Boolean = false): LoomRegistry {
        val cliPresent = mutableMapOf<String, Boolean>()
        response.optJSONObject("cli_present")?.let { map ->
            val keys = map.keys()
            while (keys.hasNext()) {
                val key = keys.next()
                cliPresent[key] = map.optBoolean(key)
            }
        }
        val archived = if (includeArchived) {
            response.optJSONArray("archived_entries")?.objects()?.map(::parseArchivedRef).orEmpty()
        } else {
            null
        }
        return LoomRegistry(
            agentTypes = response.optJSONArray("agent_types")?.objects()
                ?.map { parseAgentType(it) }.orEmpty(),
            workflows = response.optJSONArray("workflows")?.objects()
                ?.map { parseWorkflow(it) }.orEmpty(),
            cliPresent = cliPresent,
            archived = archived,
        )
    }

    fun parseArchivedRef(entry: JSONObject): LoomArchivedRef = LoomArchivedRef(
        kind = entryKind(entry.stringOrNull("kind")),
        id = entry.optString("id"),
        rev = entry.optInt("rev"),
        digest = entry.optString("digest"),
        archived = entry.optBoolean("archived", true),
    )

    /**
     * The `loom.watch` baseline. Its entries carry a tagged record, so an
     * agent-type lineage can never be collapsed into a workflow graph
     * (loom.rs:293).
     */
    fun parseWatchBaseline(response: JSONObject): LoomRegistry {
        val baseline = response.optJSONObject("baseline") ?: response
        val agents = mutableListOf<LoomAgentTypeEntry>()
        val workflows = mutableListOf<LoomWorkflowEntry>()
        val archivedRefs = mutableListOf<LoomArchivedRef>()
        baseline.optJSONArray("entries")?.objects()?.forEach { row ->
            val entry = row.optJSONObject("entry")
            val ref = entry?.let(::parseArchivedRef)
            val wrapper = row.optJSONObject("record")
            val record = wrapper?.optJSONObject("record") ?: wrapper
            val kind = entryKind(wrapper?.stringOrNull("kind") ?: entry?.stringOrNull("kind"))
            // Archive state is the baseline's, not the record's: only an entry
            // the baseline itself marked archived joins the archived list.
            val archivedRef = ref?.takeIf { it.archived }
            archivedRef?.let { archivedRefs += it }
            val isArchived = archivedRef != null
            when (kind) {
                LoomEntryKind.AgentType -> record?.let {
                    agents += parseAgentType(it, isArchived).copy(
                        rev = ref?.rev ?: it.intOrNull("rev"),
                        digest = ref?.digest ?: it.stringOrNull("digest"),
                    )
                }
                LoomEntryKind.Workflow -> record?.let {
                    workflows += parseWorkflow(it, isArchived).copy(
                        rev = ref?.rev ?: it.intOrNull("rev"),
                        digest = ref?.digest ?: it.stringOrNull("digest"),
                    )
                }
                // A record kind this build does not know is skipped rather
                // than forced into the wrong list; the baseline says so.
                LoomEntryKind.Unknown -> Unit
            }
        }
        return LoomRegistry(
            agentTypes = agents,
            workflows = workflows,
            archived = archivedRefs,
            throughCursor = baseline.stringOrNull("through_cursor")
                ?: baseline.intOrNull("through_cursor")?.toString(),
        )
    }

    fun parseInstallJobs(response: JSONObject): List<LoomInstallJob> =
        response.optJSONArray("jobs")?.objects()?.map { job ->
            val stateRaw = job.stringOrNull("state")
            // `cancelled` is the additive terminal discriminator: the legacy
            // `state` stays `failed` as a frozen-enum carrier, and a client
            // that ignores the flag reports a cancel as a failure
            // (typed_agent.rs:321-325).
            val cancelled = job.optBoolean("cancelled", false)
            LoomInstallJob(
                jobId = job.optString("job_id"),
                agentTypeId = job.stringOrNull("agent_type_id"),
                state = if (cancelled) LoomInstallState.Unknown else installState(stateRaw),
                stateRaw = if (cancelled) "cancelled" else stateRaw,
                reason = job.stringOrNull("error"),
            )
        }.orEmpty()

    fun installState(raw: String?): LoomInstallState = when (raw) {
        "queued" -> LoomInstallState.Queued
        "installing" -> LoomInstallState.Installing
        "verifying" -> LoomInstallState.Verifying
        "succeeded" -> LoomInstallState.Succeeded
        "failed" -> LoomInstallState.Failed
        else -> LoomInstallState.Unknown
    }

    // ---------- authoring ----------

    fun parseAuthorError(error: JSONObject): LoomAuthorError {
        val location = error.optJSONObject("location")
        return LoomAuthorError(
            code = error.optString("code"),
            message = error.optString("message"),
            // One-based, verbatim: nothing here subtracts one or rounds a
            // location down to its line (loom.rs:407-413).
            line = location?.optInt("line") ?: 0,
            column = location?.optInt("column") ?: 0,
            field = location?.optString("field").orEmpty(),
        )
    }

    private fun JSONObject.errors(): List<LoomAuthorError> =
        optJSONArray("errors")?.objects()?.map(::parseAuthorError).orEmpty()

    fun parseDraft(response: JSONObject): LoomAuthorDraft? {
        val draft = response.optJSONObject("draft") ?: return null
        val kind = authorKind(draft.stringOrNull("kind")) ?: return null
        return LoomAuthorDraft(
            authoringId = draft.optString("authoring_id"),
            revision = draft.optLong("revision"),
            kind = kind,
            text = draft.optString("text"),
            errors = draft.errors(),
        )
    }

    /**
     * `{confirmed?, errors[]}`.
     *
     * A null (or absent) `confirmed` is the daemon saying it did not register
     * anything. This returns null for the receipt and the errors beside it; the
     * caller must not read "no errors" as success.
     */
    fun parseConfirm(response: JSONObject): Pair<LoomAuthorConfirmed?, List<LoomAuthorError>> {
        val errors = response.errors()
        val confirmed = response.optJSONObject("confirmed") ?: return null to errors
        val kind = authorKind(confirmed.stringOrNull("kind")) ?: return null to errors
        val registration = confirmed.optJSONObject("registration")
        return LoomAuthorConfirmed(
            authoringId = confirmed.optString("authoring_id"),
            kind = kind,
            canonicalText = confirmed.optString("canonical_text"),
            registrationId = registration?.optString("id").orEmpty(),
            rev = registration?.optInt("rev") ?: 0,
            digest = registration?.optString("digest").orEmpty(),
            updated = registration?.optBoolean("updated", false) ?: false,
            executionDigest = confirmed.optString("execution_digest"),
            installJobId = confirmed.stringOrNull("install_job_id"),
        ) to errors
    }

    fun parseValidation(response: JSONObject): LoomValidation = LoomValidation(
        errors = response.errors(),
        canonicalDigestPreview = response.stringOrNull("canonical_digest"),
    )
}
