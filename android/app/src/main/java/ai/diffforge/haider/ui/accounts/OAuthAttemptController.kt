package ai.diffforge.haider.ui.accounts

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch

/**
 * Owns the live OAuth attempt **outside the composition**.
 *
 * This is not tidiness. The callback landing page's return link navigates back
 * into Settings, which disposes the Accounts screen — and with it any
 * `remember`ed flow id, attempt id and polling job. The contract is explicit
 * that `account.oauth_status` must be polled on the *original* connection with
 * the in-memory `flow_id`/`attempt_id`, and that a lost flow cannot be moved to
 * a new one. So the attempt lives in a process-scoped owner and the screen
 * merely renders it.
 *
 * Rotation, backgrounding for consent and navigating to Settings and back all
 * leave the attempt intact. Only an explicit cancel, a terminal status, or the
 * death of the scope ends it.
 */
class OAuthAttemptController(
    private val repository: AccountsRepository,
    private val scope: CoroutineScope,
    private val pollIntervalMs: Long = DEFAULT_POLL_MS,
    private val attemptIds: () -> String = { "attempt-" + java.util.UUID.randomUUID() },
) {

    /** What the screen renders. */
    data class Attempt(
        val flow: OAuthFlow.Started,
        val phase: Phase,
        val detail: String? = null,
    )

    enum class Phase { Waiting, Exchanging, Claiming, Failed, Committed }

    private val _attempt = MutableStateFlow<Attempt?>(null)
    val attempt: StateFlow<Attempt?> = _attempt.asStateFlow()

    private val _notice = MutableStateFlow<String?>(null)
    val notice: StateFlow<String?> = _notice.asStateFlow()

    private var pollJob: Job? = null

    /** Returns the URL the caller should open, or null when there is nothing to open. */
    suspend fun start(provider: String, desiredAlias: String?): String? {
        cancel()
        _notice.value = null
        return when (val started = repository.startOAuth(provider, desiredAlias, attemptIds())) {
            is OAuthFlow.Unavailable -> {
                _notice.value = started.reason ?: "Sign-in is unavailable for $provider."
                null
            }
            is OAuthFlow.Started -> {
                _attempt.value = Attempt(started, Phase.Waiting)
                poll(started)
                started.authorizationUrl
            }
        }
    }

    fun cancel() {
        val live = _attempt.value?.flow
        pollJob?.cancel()
        pollJob = null
        _attempt.value = null
        if (live != null) scope.launch { repository.cancelOAuth(live) }
    }

    fun clearNotice() {
        _notice.value = null
    }

    private fun poll(flow: OAuthFlow.Started) {
        pollJob = scope.launch {
            while (true) {
                delay(pollIntervalMs)
                when (val status = repository.pollOAuth(flow)) {
                    OAuthStatus.Waiting -> setPhase(Phase.Waiting)
                    // The provider's success page is sent before the exchange
                    // finishes, so this is a state the user actually sees.
                    OAuthStatus.Exchanging -> setPhase(Phase.Exchanging)
                    is OAuthStatus.Ready -> {
                        setPhase(Phase.Claiming)
                        val result = repository.completeOAuth(flow, status.oauthReference)
                        _notice.value = when (result) {
                            AccountResult.Ok -> "Signed in: ${status.identity ?: flow.alias}"
                            is AccountResult.Failed -> result.publicCode
                        }
                        _attempt.value = null
                        repository.refresh()
                        return@launch
                    }
                    is OAuthStatus.Failed -> {
                        _notice.value = status.publicCode ?: status.terminalKind
                        _attempt.value = null
                        return@launch
                    }
                    // Flow ownership is bound to the daemon instance, the
                    // connection and the attempt: a lost flow is restarted, not
                    // resumed — after asking whether the commit already landed.
                    OAuthStatus.Lost -> {
                        val committed = repository.accountExists(flow.provider, flow.alias)
                        _notice.value = if (committed) {
                            "Signed in: ${flow.alias}"
                        } else {
                            "The sign-in was lost — start it again."
                        }
                        _attempt.value = null
                        repository.refresh()
                        return@launch
                    }
                }
            }
        }
    }

    private fun setPhase(phase: Phase) {
        _attempt.value = _attempt.value?.copy(phase = phase)
    }

    companion object {
        const val DEFAULT_POLL_MS = 1_500L
    }
}
