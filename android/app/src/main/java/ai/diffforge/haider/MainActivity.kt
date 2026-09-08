package ai.diffforge.haider

import ai.diffforge.haider.ui.accounts.AccountsRepository
import ai.diffforge.haider.ui.accounts.FakeAccountsRepository
import ai.diffforge.haider.ui.accounts.OAuthAttemptController
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
import ai.diffforge.haider.service.HaiderAccessibilityService
import ai.diffforge.haider.transport.CapabilityBus
import ai.diffforge.haider.service.ScreenConsentActivity
import ai.diffforge.haider.ui.state.PermissionSnapshot
import ai.diffforge.haider.ui.state.PermissionStanding
import ai.diffforge.haider.ui.state.PermissionClassifier
import ai.diffforge.haider.ui.theme.ThemeMode
import ai.diffforge.haider.ui.theme.ThemePreferences
import ai.diffforge.haider.update.ApkUpdateCoordinator
import android.content.ClipData
import android.content.ClipboardManager
import android.content.ComponentName
import android.content.Context
import android.content.pm.PackageManager
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.provider.Settings
import androidx.core.content.ContextCompat
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
import androidx.lifecycle.lifecycleScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.launch
import androidx.lifecycle.ViewModelProvider
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob

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

    /**
     * False in the Robolectric Compose tests: the test rule owns this
     * Activity's content, and the update coordinator's WorkManager bootstrap
     * has no place in a JVM test. Nothing else reads it.
     */
    var activityBootstrap: Boolean = true

    private var daemon: DaemonService? = null
    private var accounts: AccountsRepository? = null
    private var oauth: OAuthAttemptController? = null

    /**
     * Process-scoped on purpose: a live OAuth attempt must survive the Accounts
     * screen being disposed by the `haider://oauth/return` navigation.
     */
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)

    fun daemon(context: Context): DaemonService =
        daemon ?: daemonFactory(context.applicationContext).also { daemon = it }

    fun accounts(context: Context): AccountsRepository =
        accounts ?: accountsFactory(context.applicationContext).also { accounts = it }

    fun oauth(context: Context): OAuthAttemptController =
        oauth ?: OAuthAttemptController(accounts(context), scope).also { oauth = it }

    /** Tests reset the container between cases. */
    fun reset() {
        daemon = null
        accounts = null
        oauth = null
    }
}

class MainActivity : ComponentActivity() {

    private var viewModel: ChatViewModel? = null

    /**
     * The result has to be propagated, not dropped. An empty callback left
     * first-run stuck on step 2 with "Notifications are off" *after* the user
     * had granted the permission, because nothing observable ever changed.
     *
     * A refusal with no rationale to show is a permanent denial: Android will
     * not present the dialog again, so the banner switches to app settings.
     */
    private val notificationPermission = registerForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) { granted ->
        // A request has now definitely happened, so a false rationale result
        // from here really does mean "will not ask again".
        markNotificationRequested()
        viewModel?.onNotificationPermissionResult(granted, !granted && !shouldShowNotificationRationale())
    }

    private val smsPermissions = registerForActivityResult(
        ActivityResultContracts.RequestMultiplePermissions(),
    ) {
        // Settings used to print "Not granted" for SMS no matter what the
        // dialog returned, because nothing read the answer back (verify-6 O3).
        publishPermissions()
    }

    /**
     * What Android currently says, read fresh.
     *
     * Accessibility comes from the secure setting the system Settings app
     * writes, so the row agrees with the switch the user just saw. Screen
     * capture is genuinely per-session: MediaProjection keeps no durable grant
     * to read, so "ask each time" is the fact, not a placeholder.
     */
    private fun observePermissions(): PermissionSnapshot = PermissionSnapshot(
        accessibility = if (accessibilityServiceEnabled()) {
            PermissionStanding.Granted
        } else {
            PermissionStanding.NotGranted
        },
        // The capture service publishes "screenCapture" on the bus while a
        // projection is live. Reporting AskEachTime regardless made Settings
        // claim consent was needed during an active projection (verify-8 O2).
        screenCapture = if (CapabilityBus.granted.value.contains("screenCapture")) {
            PermissionStanding.Granted
        } else {
            PermissionStanding.AskEachTime
        },
        sms = if (
            ContextCompat.checkSelfPermission(this, android.Manifest.permission.READ_SMS) ==
            PackageManager.PERMISSION_GRANTED &&
            ContextCompat.checkSelfPermission(this, android.Manifest.permission.RECEIVE_SMS) ==
            PackageManager.PERMISSION_GRANTED
        ) {
            PermissionStanding.Granted
        } else {
            PermissionStanding.NotGranted
        },
    )

    private fun publishPermissions() {
        viewModel?.onPermissionsObserved(observePermissions())
    }

    private fun accessibilityServiceEnabled(): Boolean {
        val enabled = Settings.Secure.getString(
            contentResolver,
            Settings.Secure.ENABLED_ACCESSIBILITY_SERVICES,
        ).orEmpty()
        val component = ComponentName(this, HaiderAccessibilityService::class.java)
        return enabled.split(':').any {
            it.equals(component.flattenToString(), ignoreCase = true) ||
                it.equals(component.flattenToShortString(), ignoreCase = true)
        }
    }



    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        if (AppContainer.activityBootstrap) {
            ApkUpdateCoordinator.start(applicationContext)
        }
        val service = AppContainer.daemon(this)
        val accounts = AppContainer.accounts(this)
        val oauth = AppContainer.oauth(this)
        val viewModel = ViewModelProvider(
            this,
            ChatViewModel.factory(
                service = service,
                notificationsSupported = Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU,
            ),
        )[ChatViewModel::class.java]
        this.viewModel = viewModel

        val themeStore = ThemePreferences.store(this)
        val bannerPreferences = getSharedPreferences(BANNER_PREFERENCES, Context.MODE_PRIVATE)

        handleDeepLink(intent, viewModel)

        if (!AppContainer.activityBootstrap) return

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
                oauth = oauth,
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

    /** Cancelled in `onPause`: the bus outlives the Activity, the watch does not. */
    private var capabilityWatch: Job? = null

    override fun onResume() {
        super.onResume()
        // A projection can start or die while this screen is up — the capture
        // service publishes it on the bus — and round 10 only read the bus
        // when something else happened to sample permissions, so Settings
        // showed a stale row (verify-9 V5).
        capabilityWatch?.cancel()
        capabilityWatch = lifecycleScope.launch {
            CapabilityBus.granted.collect { publishPermissions() }
        }
        if (AppContainer.activityBootstrap) ApkUpdateCoordinator.onActivityResumed(this)
        // The user may have granted it in system settings while we were away.
        syncNotificationPermission()
        // Same for accessibility and SMS: returning from system Settings is
        // the usual way either of those changes (verify-6 O3).
        publishPermissions()
    }

    private fun syncNotificationPermission() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) return
        val granted = checkSelfPermission(android.Manifest.permission.POST_NOTIFICATIONS) ==
            android.content.pm.PackageManager.PERMISSION_GRANTED
        viewModel?.onNotificationPermissionResult(
            granted,
            PermissionClassifier.permanentlyDenied(
                granted = granted,
                everRequested = hasRequestedNotifications(),
                shouldShowRationale = shouldShowNotificationRationale(),
            ),
        )
    }

    private fun shouldShowNotificationRationale(): Boolean =
        Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            shouldShowRequestPermissionRationale(android.Manifest.permission.POST_NOTIFICATIONS)

    private fun permissionPreferences() =
        getSharedPreferences(PERMISSION_PREFERENCES, Context.MODE_PRIVATE)

    private fun hasRequestedNotifications(): Boolean =
        permissionPreferences().getBoolean(KEY_NOTIFICATIONS_REQUESTED, false)

    private fun markNotificationRequested() {
        permissionPreferences().edit().putBoolean(KEY_NOTIFICATIONS_REQUESTED, true).apply()
    }

    override fun onPause() {
        if (AppContainer.activityBootstrap) ApkUpdateCoordinator.onActivityPaused(this)
        capabilityWatch?.cancel()
        capabilityWatch = null
        super.onPause()
    }

    private fun handleSystemAction(action: SystemAction) {
        when (action) {
            SystemAction.RequestNotifications ->
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                    notificationPermission.launch(android.Manifest.permission.POST_NOTIFICATIONS)
                }
            SystemAction.OpenAppSettings, SystemAction.OpenBattery ->
                startSettings(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, true)
            SystemAction.OpenAccessibility -> startSettings(Settings.ACTION_ACCESSIBILITY_SETTINGS, false)
            SystemAction.GrantSms -> smsPermissions.launch(
                arrayOf(
                    android.Manifest.permission.READ_SMS,
                    android.Manifest.permission.RECEIVE_SMS,
                ),
            )
            // MediaProjection consent: a transparent system activity issues
            // it and starts the capture service on OK (addition H6).
            SystemAction.RequestScreenCapture ->
                startActivity(Intent(this, ScreenConsentActivity::class.java))
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

private const val PERMISSION_PREFERENCES = "haider_permissions"
private const val KEY_NOTIFICATIONS_REQUESTED = "notifications_requested"

/** The version string shown in the header, drawer and start surface. */
object BuildConfigVersion {
    fun name(context: Context): String = runCatching {
        context.packageManager.getPackageInfo(context.packageName, 0).versionName
    }.getOrNull().orEmpty().ifEmpty { "0.0.971" }
}
