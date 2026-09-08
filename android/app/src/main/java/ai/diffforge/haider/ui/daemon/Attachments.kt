package ai.diffforge.haider.ui.daemon

/**
 * `AttachmentBlock` (haider-protocol `tool.rs:386`), as the UI sees it.
 *
 * The wire form is internally tagged on `kind` in snake_case and carries a CAS
 * address, never bytes: `artifact` is an `ArtifactRef`, a `blake3:…` string.
 * The kinds are exactly the four the protocol names — an unknown kind is
 * [Unsupported] and says so rather than being dropped, because a turn that
 * silently loses an attachment is worse than one that admits it cannot draw it.
 */
sealed interface Attachment {
    val artifact: String

    data class Image(
        override val artifact: String,
        val mime: String,
        val width: Int? = null,
        val height: Int? = null,
    ) : Attachment

    data class PastedText(
        override val artifact: String,
        val lines: Int,
    ) : Attachment

    /** `name` is a sanitised basename, never a path (protocol `tool.rs:399`). */
    data class TextFile(
        override val artifact: String,
        val name: String,
        val lines: Int,
    ) : Attachment

    data class Pdf(
        override val artifact: String,
        val name: String,
        val pages: Int,
    ) : Attachment

    data class Unsupported(
        override val artifact: String,
        val kind: String,
    ) : Attachment
}

/** Why the daemon refused a turn's attachments (`frame.rs:243`, `:245`). */
object AttachmentLimits {
    const val TOO_MANY = "too_many_attachments"
    const val TOO_LARGE = "attachments_too_large"
}

/**
 * Mid-turn delivery (`haider-protocol/lib.rs:172`).
 *
 * `Steer` is the wire default. `Subturn` exists on the wire and is not offered
 * here: it is a delegation shape, not something a phone composer should pick
 * for somebody.
 */
enum class Delivery(val wire: String) {
    Steer("steer"),
    Queue("queue"),
}

/** One held message behind an active turn (`haider-protocol/queue.rs:13`). */
data class QueuedMessage(
    val id: String,
    val text: String,
    val delivery: Delivery,
    val ordinal: Int,
    val createdAtMs: Long,
)

/**
 * A queue snapshot, fenced by its revision.
 *
 * `queue.remove` and `queue.promote_steer` both carry the revision they were
 * read at, so a stale click is refused rather than applied to whatever moved
 * into that row — the same compare-and-set discipline as menu answers.
 *
 * [supported] separates "nothing is queued" from "this daemon has no queue":
 * the frame doc is explicit that failure or feature absence is never an empty
 * snapshot.
 */
data class QueueSnapshot(
    val revision: Long = 0,
    val rows: List<QueuedMessage> = emptyList(),
    val supported: Boolean = false,
    val error: String? = null,
)

/** `LocalUsageStatsV1` for one turn or one account (`usage.rs:359`). */
data class TokenUsage(
    val inputTokens: Long = 0,
    val outputTokens: Long = 0,
    val reasoningTokens: Long = 0,
    val cachedTokens: Long = 0,
    /**
     * `est_cost_usd`. The protocol calls this "never a bill — an estimate",
     * and the UI says the same thing.
     */
    val estCostUsd: Double? = null,
) {
    val total: Long get() = inputTokens + outputTokens + reasoningTokens
}

/** `usage.report` (`usage.rs:296`), reduced to what a phone footer shows. */
data class UsageSnapshot(
    val generatedAtMs: Long? = null,
    val accounts: List<AccountUsage> = emptyList(),
    val supported: Boolean = false,
) {
    val totals: TokenUsage
        get() = accounts.fold(TokenUsage()) { acc, entry ->
            TokenUsage(
                inputTokens = acc.inputTokens + entry.usage.inputTokens,
                outputTokens = acc.outputTokens + entry.usage.outputTokens,
                reasoningTokens = acc.reasoningTokens + entry.usage.reasoningTokens,
                cachedTokens = acc.cachedTokens + entry.usage.cachedTokens,
                estCostUsd = listOfNotNull(acc.estCostUsd, entry.usage.estCostUsd)
                    .takeIf { it.isNotEmpty() }
                    ?.sum(),
            )
        }
}

data class AccountUsage(
    val provider: String,
    val alias: String,
    val identity: String? = null,
    val plan: String? = null,
    val usage: TokenUsage = TokenUsage(),
)

/** A `turn.submit` the daemon refused, carrying its own public code. */
class TurnRefused(val code: String) : Exception(code)

/** A queue mutation whose revision no longer names the current snapshot. */
class StaleQueueRevision(val revision: Long) : Exception("stale_queue_revision:$revision")
