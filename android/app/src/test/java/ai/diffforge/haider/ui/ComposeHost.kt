package ai.diffforge.haider.ui

import ai.diffforge.haider.AppContainer
import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.accounts.AccountsRepository
import ai.diffforge.haider.ui.accounts.FakeAccountsRepository
import ai.diffforge.haider.ui.accounts.OAuthAttemptController
import ai.diffforge.haider.ui.chat.ChatViewModel
import ai.diffforge.haider.ui.daemon.FakeDaemonService
import ai.diffforge.haider.ui.daemon.FakeScenario
import ai.diffforge.haider.ui.scaffold.HaiderApp
import ai.diffforge.haider.ui.scaffold.InMemoryBannerDismissals
import ai.diffforge.haider.ui.scaffold.SystemAction
import ai.diffforge.haider.ui.theme.ThemeMode
import androidx.compose.ui.test.junit4.ComposeContentTestRule

/**
 * Compose tests run on the JVM under Robolectric, in the app's own
 * [MainActivity], so no test-only activity has to be declared in the shipped
 * manifest. The activity's bootstrap (WorkManager-backed update checks) is
 * switched off; the rule then replaces its content.
 */
object ComposeHost {

    fun install(scenario: FakeScenario = FakeScenario.Populated): FakeDaemonService {
        AppContainer.activityBootstrap = false
        AppContainer.reset()
        val service = FakeDaemonService(scenario)
        AppContainer.daemonFactory = { service }
        AppContainer.accountsFactory = { FakeAccountsRepository() }
        return service
    }

    fun viewModel(service: FakeDaemonService): ChatViewModel =
        ChatViewModel(service, searchDebounceMs = 0)
}

/** Renders the whole app against a fake daemon, in a fixed theme. */
fun ComposeContentTestRule.setHaiderApp(
    service: FakeDaemonService,
    accounts: AccountsRepository = FakeAccountsRepository(),
    oauth: OAuthAttemptController? = null,
    dark: Boolean = true,
    themeMode: ThemeMode = ThemeMode.Dark,
    onSystemAction: (SystemAction) -> Unit = {},
    onOpenUrl: (String) -> Unit = {},
): ChatViewModel {
    val viewModel = ChatViewModel(service, searchDebounceMs = 0)
    val controller = oauth ?: OAuthAttemptController(accounts, kotlinx.coroutines.MainScope())
    setContent {
        HaiderApp(
            viewModel = viewModel,
            accounts = accounts,
            oauth = controller,
            appVersion = "0.0.971",
            themeMode = themeMode,
            onThemeMode = {},
            onSystemAction = onSystemAction,
            onOpenUrl = onOpenUrl,
            dismissals = InMemoryBannerDismissals(),
            nowMsProvider = { FakeDaemonService.FIXED_NOW },
            elapsedRealtimeProvider = { FakeDaemonService.FIXED_UPTIME },
            darkOverride = dark,
        )
    }
    return viewModel
}
