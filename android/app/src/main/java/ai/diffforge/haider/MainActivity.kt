package ai.diffforge.haider

import ai.diffforge.haider.ui.accounts.AccountsRepository
import ai.diffforge.haider.ui.accounts.FakeAccountsRepository
import ai.diffforge.haider.ui.daemon.DaemonIntents
import ai.diffforge.haider.ui.daemon.DaemonService
import ai.diffforge.haider.ui.daemon.FakeDaemonService
import ai.diffforge.haider.ui.daemon.FakeScenario
import ai.diffforge.haider.ui.chat.ChatViewModel
import ai.diffforge.haider.ui.scaffold.BANNER_PREFERENCES
import ai.diffforge.haider.ui.scaffold.HaiderApp
import ai.diffforge.haider.ui.scaffold.SharedPreferencesBannerDismissals
import ai.diffforge.haider.ui.scaffold.SystemAction
import ai.diffforge.haider.ui.state.Overlay
import ai.diffforge.haider.ui.theme.ThemeMode
import ai.diffforge.haider.ui.theme.ThemePreferences
import ai.diffforge.haider.update.ApkUpdateCoordinator
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.provider.Settings
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.platform.LocalView
import androidx.compose.runtime.SideEffect
import androidx.core.view.WindowCompat
import androidx.lifecycle.ViewModelProvider

/**
 * Process-wide wiring.
 *
 * [AppContainer] is the seam lane 971-3 moves: swapping [FakeDaemonService] for
 * `TransportDaemonService` (and the fake accounts repository for the RPC-backed
 * one) is a change to two lambdas, because everything above depends on the
 * interfaces, not the implementations.
 */
object AppContainer {
    var daemonFactory: (Context) -> DaemonService = { FakeDaemonService(FakeScenario.FirstRun) }
    var accountsFactory: (Context) -> AccountsRepository = { FakeAccountsRepository() }

    /** Robolectric hosts the composables in this Activity; it must not boot WorkManager. */
    var bootstrapEnabled: Boolean = true

    private var daemon: DaemonService? = null
    private var accounts: AccountsRepository? = null

    fun daemon(context: Context): DaemonService =
        daemon ?: daemonFactory(context.applicationContext).also { daemon = it }

    fun accounts(context: Context): AccountsRepository =
        accounts ?: accountsFactory(context.applicationContext).also { accounts = it }

    /** Tests reset the container between cases. */
    fun reset() {
        daemon = null
        accounts = null
    }
}

class MainActivity : ComponentActivity() {

    private val notificationPermission = registerForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) { /* reflected through the daemon snapshot */ }

    private val smsPermissions = registerForActivityResult(
        ActivityResultContracts.RequestMultiplePermissions(),
    ) { /* reflected when the transport reports capabilities.changed */ }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        if (AppContainer.bootstrapEnabled) {
            ApkUpdateCoordinator.start(applicationContext)
        }
        val service = AppContainer.daemon(this)
        val accounts = AppContainer.accounts(this)
        val viewModel = ViewModelProvider(
            this,
            ChatViewModel.factory(
                service = service,
                notificationsSupported = Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU,
            ),
        )[ChatViewModel::class.java]

        val themeStore = ThemePreferences.store(this)
        val bannerPreferences = getSharedPreferences(BANNER_PREFERENCES, Context.MODE_PRIVATE)

        handleDeepLink(intent, viewModel)

        setContent {
            var themeMode by rememberSaveable { mutableStateOf(ThemePreferences.load(themeStore)) }
            val updateState by ApkUpdateCoordinator.state.collectAsState()
            val dismissals = remember { SharedPreferencesBannerDismissals(bannerPreferences) }
            val view = LocalView.current
            val dark = when (themeMode) {
                ThemeMode.Dark -> true
                ThemeMode.Light -> false
                ThemeMode.System -> androidx.compose.foundation.isSystemInDarkTheme()
            }
            if (!view.isInEditMode) {
                SideEffect {
                    WindowCompat.getInsetsController(window, view).apply {
                        isAppearanceLightStatusBars = !dark
                        isAppearanceLightNavigationBars = !dark
                    }
                }
            }

            HaiderApp(
                viewModel = viewModel,
                accounts = accounts,
                appVersion = BuildConfigVersion.name(this),
                themeMode = themeMode,
                onThemeMode = { mode ->
                    themeMode = mode
                    ThemePreferences.save(themeStore, mode)
                },
                updateState = updateState,
                onUpdateAction = { ApkUpdateCoordinator.onAffordanceTapped(this) },
                onOpenUrl = ::openUrl,
                onSystemAction = ::handleSystemAction,
                dismissals = dismissals,
            )
        }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        val service = AppContainer.daemon(this)
        handleDeepLink(
            intent,
            ViewModelProvider(this, ChatViewModel.factory(service))[ChatViewModel::class.java],
        )
    }

    /**
     * Two navigation paths, neither of which is authority for anything.
     *
     * `haider://oauth/return` is the callback landing page's return link. It
     * carries no code, state, token, flow id or ready reference, and another app
     * can claim the scheme, so all it does is open Settings. It never starts a
     * disabled daemon, creates a flow or commits an account; the sign-in result
     * is read from `account.oauth_status` on the original RPC connection.
     *
     * `OPEN_SESSION` / `OPEN_INPUT` are explicit package-scoped notification
     * intents. Their coordinates are hints: the UI re-reads the current snapshot
     * before rendering or answering.
     */
    private fun handleDeepLink(intent: Intent?, viewModel: ChatViewModel) {
        if (intent == null) return
        val data = intent.data
        if (data != null &&
            data.scheme == DaemonIntents.DEEP_LINK_SCHEME &&
            data.host == DaemonIntents.OAUTH_RETURN_HOST
        ) {
            viewModel.openOverlay(Overlay.Settings)
            return
        }
        when (intent.action) {
            DaemonIntents.OPEN_SESSION, DaemonIntents.OPEN_INPUT -> {
                val sessionId = intent.getStringExtra(DaemonIntents.EXTRA_SESSION_ID)
                if (!sessionId.isNullOrBlank()) viewModel.activate(sessionId)
            }
            DaemonIntents.OPEN_DAEMON_STATUS -> viewModel.openOverlay(Overlay.DaemonDetails)
        }
    }

    override fun onResume() {
        super.onResume()
        if (AppContainer.bootstrapEnabled) ApkUpdateCoordinator.onActivityResumed(this)
    }

    override fun onPause() {
        if (AppContainer.bootstrapEnabled) ApkUpdateCoordinator.onActivityPaused(this)
        super.onPause()
    }

    private fun handleSystemAction(action: SystemAction) {
        when (action) {
            SystemAction.RequestNotifications ->
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                    notificationPermission.launch(android.Manifest.permission.POST_NOTIFICATIONS)
                }
            SystemAction.OpenBattery -> startSettings(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, true)
            SystemAction.OpenAccessibility -> startSettings(Settings.ACTION_ACCESSIBILITY_SETTINGS, false)
            SystemAction.GrantSms -> smsPermissions.launch(
                arrayOf(
                    android.Manifest.permission.READ_SMS,
                    android.Manifest.permission.RECEIVE_SMS,
                ),
            )
            SystemAction.Screenshot, SystemAction.PickFile -> Unit
            is SystemAction.CopyText -> {
                val clipboard = getSystemService(ClipboardManager::class.java)
                clipboard?.setPrimaryClip(ClipData.newPlainText("haider", action.text))
            }
        }
    }

    private fun startSettings(action: String, withPackage: Boolean) {
        val intent = Intent(action).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        if (withPackage) intent.data = Uri.fromParts("package", packageName, null)
        runCatching { startActivity(intent) }
    }

    private fun openUrl(url: String) {
        runCatching {
            startActivity(
                Intent(Intent.ACTION_VIEW, Uri.parse(url)).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            )
        }
    }
}

/** The version string shown in the header, drawer and start surface. */
object BuildConfigVersion {
    fun name(context: Context): String = runCatching {
        context.packageManager.getPackageInfo(context.packageName, 0).versionName
    }.getOrNull().orEmpty().ifEmpty { "0.0.971" }
}
