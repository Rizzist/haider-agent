package ai.diffforge.haider.ui.loom

/**
 * Typed view models for the Loom registry and its authoring cycle, checked
 * against `haider-protocol/src/loom.rs` and the `loom.*` frames in
 * `haider-rpc/src/frame.rs`.
 *
 * Two house laws carry over from the desktop `loomModel.js` and are the reason
 * this file has types the wire does not:
 *
 *  1. **`cli_present` is tri-state.** A key absent from the map means NOT
 *     PROBED — an older daemon, or a name that arrived some other way — which
 *     is not the same claim as "missing from this device" (frame.rs:4783-4790).
 *  2. **`confirmed: null` is a first-class outcome, and it is not success.**
 *     `loom.author.confirm` answers with `{confirmed?, errors[]}`
 *     (frame.rs:5416): no registry entry exists, and the draft is still a
 *     draft. [LoomAuthoringState.Refused] is that answer; nothing collapses it
 *     into [LoomAuthoringState.Confirmed].
 */

// ---------- registry ----------

/** `LoomRegistryEntryKind` (loom.rs:249). `Unknown` is a real serde variant. */
enum class LoomEntryKind { AgentType, Workflow, Unknown }

/**
 * `LoomAgentType` (loom.rs:41-70). `color` and `glyph` are display fields that
 * participate in the content digest: an accent-only edit is a real revision.
 */
data class LoomAgentTypeEntry(
    val id: String,
    val name: String,
    val job: String = "",
    val inType: String = "",
    val outType: String = "",
    val clis: List<String> = emptyList(),
    val apis: List<String> = emptyList(),
    /** Explicitly withheld capability keys — an authored decision, kept. */
    val denials: List<String> = emptyList(),
    val skills: List<String> = emptyList(),
    val scripts: List<String> = emptyList(),
    /** A hex accent, or empty. Never defaulted to a colour the daemon did not send. */
    val color: String = "",
    val glyph: String = "",
    val rev: Int? = null,
    val digest: String? = null,
    val archived: Boolean = false,
)

/** `LoomWorkflow` (loom.rs:212). The pipe source is the structure of record. */
data class LoomWorkflowEntry(
    val id: String,
    val pipeVersion: String = "",
    val source: String = "",
    val inType: String = "",
    val outType: String = "",
    val templateName: String = "",
    val nodeCount: Int = 0,
    val rev: Int? = null,
    val digest: String? = null,
    val archived: Boolean = false,
)

/** Whether a declared CLI is on the device. Absence is its own answer. */
enum class LoomCliPresence { Present, Missing, NotProbed }

/**
 * One `loom.list` read.
 *
 * [archived] is null until `include_archived` was actually requested: an empty
 * list after a default read must never be presented as proof that nothing is
 * archived.
 */
data class LoomRegistry(
    val agentTypes: List<LoomAgentTypeEntry> = emptyList(),
    val workflows: List<LoomWorkflowEntry> = emptyList(),
    /** Keyed by the declared CLI name verbatim; an absent key is NOT PROBED. */
    val cliPresent: Map<String, Boolean> = emptyMap(),
    val archived: List<LoomArchivedRef>? = null,
    /** From `loom.watch`'s baseline, when one was taken. */
    val throughCursor: String? = null,
) {
    fun cliPresence(name: String): LoomCliPresence = when (cliPresent[name]) {
        true -> LoomCliPresence.Present
        false -> LoomCliPresence.Missing
        null -> LoomCliPresence.NotProbed
    }

    /** Every CLI any registered type declares, in a stable order. */
    val declaredClis: List<String>
        get() = agentTypes.flatMap { it.clis }.distinct().sorted()
}

/** `LoomRegistryEntryRef` (loom.rs:280) — the CAS coordinate plus archive state. */
data class LoomArchivedRef(
    val kind: LoomEntryKind,
    val id: String,
    val rev: Int,
    val digest: String,
    val archived: Boolean,
)

/** `TypedAgentInstallJob` states, kept verbatim when unrecognised. */
enum class LoomInstallState { Queued, Installing, Verifying, Succeeded, Failed, Unknown }

data class LoomInstallJob(
    val jobId: String,
    val agentTypeId: String?,
    val state: LoomInstallState,
    val stateRaw: String?,
    val reason: String? = null,
) {
    /** Retry is offered for `failed` and nothing else (desktop house law). */
    val retryable: Boolean get() = state == LoomInstallState.Failed
}

/**
 * The CAS fence a mutation must carry.
 *
 * Fences **echo** what the client actually read. Nothing here increments,
 * defaults, hashes or otherwise manufactures a revision: `expected_rev` and
 * `expected_digest` are `skip_serializing_if` on the wire, so an unknown one is
 * omitted rather than invented (frame.rs:4358-4374).
 */
data class LoomFence(val expectedRev: Int?, val expectedDigest: String?) {
    val empty: Boolean get() = expectedRev == null && expectedDigest == null

    companion object {
        fun of(entry: LoomAgentTypeEntry) = LoomFence(entry.rev, entry.digest)
        fun of(entry: LoomWorkflowEntry) = LoomFence(entry.rev, entry.digest)
        fun of(entry: LoomArchivedRef) = LoomFence(entry.rev, entry.digest)
    }
}

// ---------- authoring ----------

/** `LoomAuthorKind` (loom.rs:385). */
enum class LoomAuthorKind { AgentType, Workflow }

/** `LoomAuthorValidationError` (loom.rs:416); line/column are one-based. */
data class LoomAuthorError(
    val code: String,
    val message: String,
    val line: Int,
    val column: Int,
    val field: String,
)

/**
 * `LoomAuthorDraft` (loom.rs:425).
 *
 * [revision] is the daemon-owned edit fence for this authoring session. It is
 * echoed back on the next revise/confirm as `expected_revision`; a client that
 * adds one to it is guessing at a value only the daemon mints.
 */
data class LoomAuthorDraft(
    val authoringId: String,
    val revision: Long,
    val kind: LoomAuthorKind,
    val text: String,
    val errors: List<LoomAuthorError> = emptyList(),
) {
    val valid: Boolean get() = errors.isEmpty()
}

/** `LoomAuthorConfirmed` (loom.rs:439) + `LoomRegistration` (loom.rs:235). */
data class LoomAuthorConfirmed(
    val authoringId: String,
    val kind: LoomAuthorKind,
    val canonicalText: String,
    val registrationId: String,
    val rev: Int,
    val digest: String,
    /** False when the call was an idempotent same-content no-op. */
    val updated: Boolean,
    /** Daemon-issued. Clients never compute this. */
    val executionDigest: String,
    val installJobId: String? = null,
)

/** `loom.validate` (frame.rs:5431): non-mutating, so its digest is a preview. */
data class LoomValidation(
    val errors: List<LoomAuthorError> = emptyList(),
    /**
     * Named a preview on purpose: it is not a digest stored on a registry
     * entry, and presenting it as one would claim a registration that has not
     * happened.
     */
    val canonicalDigestPreview: String? = null,
)

/**
 * The authoring cycle: prose → draft → revise → confirm.
 *
 * Every state that can act carries the exact draft whose fence the next call
 * must echo, so there is no path that sends a revise without one.
 */
sealed interface LoomAuthoringState {
    data object Idle : LoomAuthoringState

    /** `loom.author.draft` is in flight. */
    data class Drafting(val kind: LoomAuthorKind, val prose: String) : LoomAuthoringState

    /**
     * A draft came back and is being edited. [text] is the user's exact text,
     * which may differ from `draft.text` before a revise re-parses it.
     */
    data class Editing(
        val draft: LoomAuthorDraft,
        val text: String,
        val busy: Boolean = false,
        /** From a `loom.validate` the user asked for, if any. */
        val validation: LoomValidation? = null,
    ) : LoomAuthoringState

    data class Confirming(val draft: LoomAuthorDraft, val text: String) : LoomAuthoringState

    data class Confirmed(val receipt: LoomAuthorConfirmed) : LoomAuthoringState

    /**
     * The daemon answered `confirmed: null`. Nothing was registered; the draft
     * survives so the errors can be fixed in place.
     */
    data class Refused(
        val draft: LoomAuthorDraft,
        val text: String,
        val errors: List<LoomAuthorError>,
        val reason: String?,
    ) : LoomAuthoringState

    /** A transport or daemon error, with the daemon's own code. */
    data class Failed(val reason: String, val draft: LoomAuthorDraft? = null) : LoomAuthoringState

    /**
     * The daemon does not offer authoring, or has no provider/model to draft
     * with. The reason is its typed code, shown as-is — this screen never
     * pretends a missing model produced a draft.
     */
    data class Unavailable(val reason: String) : LoomAuthoringState
}

object LoomAuthoringMachine {

    /** The draft whose fence the next call must echo, if this state has one. */
    fun draftOf(state: LoomAuthoringState): LoomAuthorDraft? = when (state) {
        is LoomAuthoringState.Editing -> state.draft
        is LoomAuthoringState.Confirming -> state.draft
        is LoomAuthoringState.Refused -> state.draft
        is LoomAuthoringState.Failed -> state.draft
        is LoomAuthoringState.Confirmed -> null
        is LoomAuthoringState.Drafting -> null
        is LoomAuthoringState.Unavailable -> null
        LoomAuthoringState.Idle -> null
    }

    /** The user's current text, or the draft's when it has not been edited. */
    fun textOf(state: LoomAuthoringState): String = when (state) {
        is LoomAuthoringState.Editing -> state.text
        is LoomAuthoringState.Confirming -> state.text
        is LoomAuthoringState.Refused -> state.text
        is LoomAuthoringState.Confirmed -> state.receipt.canonicalText
        else -> ""
    }

    fun requestDraft(kind: LoomAuthorKind, prose: String): LoomAuthoringState =
        LoomAuthoringState.Drafting(kind, prose)

    fun drafted(draft: LoomAuthorDraft): LoomAuthoringState =
        LoomAuthoringState.Editing(draft = draft, text = draft.text)

    fun edited(state: LoomAuthoringState, text: String): LoomAuthoringState = when (state) {
        is LoomAuthoringState.Editing -> state.copy(text = text, validation = null)
        // A refusal is still editable: fixing the text is the whole point.
        is LoomAuthoringState.Refused ->
            LoomAuthoringState.Editing(draft = state.draft, text = text)
        else -> state
    }

    /**
     * Revise re-parses the user's exact text under the draft's own fence.
     *
     * Without a draft there is no `authoring_id`/`expected_revision` to send,
     * and a revise cannot be manufactured from the text alone — the state is
     * returned unchanged rather than sending a call the daemon would refuse.
     */
    fun requestRevise(state: LoomAuthoringState): LoomAuthoringState = when (state) {
        is LoomAuthoringState.Editing -> state.copy(busy = true)
        is LoomAuthoringState.Refused ->
            LoomAuthoringState.Editing(draft = state.draft, text = state.text, busy = true)
        else -> state
    }

    /** The revised draft replaces the old one, fence included. */
    fun revised(draft: LoomAuthorDraft): LoomAuthoringState =
        LoomAuthoringState.Editing(draft = draft, text = draft.text)

    /**
     * Confirm is offered only for a draft the daemon last called valid.
     *
     * Sending a confirm for a draft carrying errors asks the daemon to reject
     * it, and the round trip is the user's time.
     */
    fun canConfirm(state: LoomAuthoringState): Boolean = when (state) {
        is LoomAuthoringState.Editing -> !state.busy && state.draft.valid && state.text.isNotBlank()
        else -> false
    }

    fun requestConfirm(state: LoomAuthoringState): LoomAuthoringState =
        if (canConfirm(state) && state is LoomAuthoringState.Editing) {
            LoomAuthoringState.Confirming(state.draft, state.text)
        } else {
            state
        }

    /**
     * `{confirmed, errors}` — the daemon's two answers, kept apart.
     *
     * A null receipt is [LoomAuthoringState.Refused]: no entry was registered
     * and the draft is unchanged. Collapsing it into success is the exact
     * mistake this signature exists to prevent.
     */
    fun confirmAnswered(
        state: LoomAuthoringState,
        receipt: LoomAuthorConfirmed?,
        errors: List<LoomAuthorError>,
        reason: String? = null,
    ): LoomAuthoringState {
        if (receipt != null) return LoomAuthoringState.Confirmed(receipt)
        val draft = draftOf(state) ?: return LoomAuthoringState.Failed(
            reason ?: "confirm_not_confirmed",
        )
        return LoomAuthoringState.Refused(
            draft = draft,
            text = textOf(state).ifEmpty { draft.text },
            errors = errors,
            reason = reason,
        )
    }

    fun failed(state: LoomAuthoringState, reason: String): LoomAuthoringState =
        LoomAuthoringState.Failed(reason, draftOf(state))

    /** Every error the user should see: the draft's, plus a refusal's. */
    fun errorsOf(state: LoomAuthoringState): List<LoomAuthorError> = when (state) {
        is LoomAuthoringState.Editing -> state.draft.errors + state.validation?.errors.orEmpty()
        is LoomAuthoringState.Refused -> state.errors
        else -> emptyList()
    }
}
