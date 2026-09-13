package ai.diffforge.haider

import ai.diffforge.haider.ui.accounts.AccountsRepository
import ai.diffforge.haider.ui.accounts.OAuthAttemptController
import ai.diffforge.haider.ui.daemon.DaemonIntents
import ai.diffforge.haider.ui.daemon.DaemonService
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
import ai.diffforge.haider.ui.theme.LogoPreferences
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
import androidx.activity.result.PickVisualMediaRequest
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
import androidx.browser.customtabs.CustomTabsIntent

/** Application-scoped Binder, RPC and OAuth ownership. Tests may inject their own factories. */
object AppContainer {
    var daemonFactory: (Context) -> DaemonService = { context ->
        // The native host independently enforces FLAG_DEBUGGABLE, owner, mode,
        // and link checks. This selects an honest fixture label for device tests.
        val fixture = BuildConfig.DEBUG && java.io.File(context.filesDir,
            "haider/runtime/android-default/fake-provider.enabled").isFile
        ai.diffforge.haider.transport.rpc.RpcDaemonService(scope,
            ai.diffforge.haider.daemon.BinderRpcControlPlane(context, scope),
            java.io.File(context.filesDir, "haider/ui-cache/display"),
            java.io.File(context.filesDir, "haider/profiles/default/workspace").path,
            if (fixture) "fake" else "anthropic",
            if (fixture) "fake-model" else context.getString(R.string.daemon_default_model), 8192)
    }
    var accountsFactory: (Context) -> AccountsRepository = { context ->
        (daemon(context) as ai.diffforge.haider.transport.rpc.RpcDaemonService).accounts
    }

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

    /**
     * The system photo picker: no storage permission, no file paths, and the
     * user chooses exactly what Haider sees.
     */
    private val imagePicker = registerForActivityResult(
        ActivityResultContracts.PickVisualMedia(),
    ) { uri -> stage(uri, fallbackMime = "image/*") }

    /**
     * Any document, for the attach sheet's "Photo or file".
     *
     * The filter is images and text: those are the two block kinds this client
     * can state truthfully on `turn.submit`, and offering a type that would be
     * refused after the picker closed is worse than not offering it.
     */
    private val filePicker = registerForActivityResult(
        ActivityResultContracts.OpenDocument(),
    ) { uri -> stage(uri, fallbackMime = "application/octet-stream") }

    /** Reads what the picker returned and hands the bytes to the view model. */
    private fun stage(uri: Uri?, fallbackMime: String) {
        val target = uri ?: return
        val bytes = runCatching {
            contentResolver.openInputStream(target)?.use { it.readBytes() }
        }.getOrNull() ?: return
        val mime = contentResolver.getType(target) ?: fallbackMime
        viewModel?.attach(bytes, mime, name = displayName(target))
    }

    /** `OpenableColumns.DISPLAY_NAME`, so a text file keeps the name the model sees. */
    private fun displayName(uri: Uri): String? = runCatching {
        contentResolver.query(uri, arrayOf(android.provider.OpenableColumns.DISPLAY_NAME), null, null, null)
            ?.use { cursor -> if (cursor.moveToFirst()) cursor.getString(0) else null }
    }.getOrNull()

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
        // The first explicit launcher action opts into the on-phone daemon.
        // Subsequent launches preserve Stop, and OAuth/notification returns never enable it.
        if (AppContainer.activityBootstrap && intent?.action == Intent.ACTION_MAIN &&
            !permissionPreferences().getBoolean("initial_launcher_start", false)) {
            permissionPreferences().edit().putBoolean("initial_launcher_start", true).apply()
            viewModel.startDaemon()
        }

        val themeStore = ThemePreferences.store(this)
        val bannerPreferences = getSharedPreferences(BANNER_PREFERENCES, Context.MODE_PRIVATE)

        handleDeepLink(intent, viewModel)

        if (!AppContainer.activityBootstrap) return

        setContent {
            var themeMode by rememberSaveable { mutableStateOf(ThemePreferences.load(themeStore)) }
            var logoStyle by rememberSaveable { mutableStateOf(LogoPreferences.load(themeStore)) }
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
                service = service,
                accounts = accounts,
                oauth = oauth,
                appVersion = BuildConfigVersion.name(this),
                themeMode = themeMode,
                onThemeMode = { mode ->
                    themeMode = mode
                    ThemePreferences.save(themeStore, mode)
                },
                logoStyle = logoStyle,
                onLogoStyle = { style ->
                    logoStyle = style
                    LogoPreferences.save(themeStore, style)
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
            intent.action == Intent.ACTION_VIEW && data.toString() == "haider://oauth/return"
        ) {
            viewModel.openOverlay(Overlay.Settings)
            return
        }
        when (intent.action) {
            ai.diffforge.haider.daemon.DaemonIntents.OPEN_SETTINGS -> viewModel.openOverlay(Overlay.Settings)
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
            // The picker returns bytes; staging them into the daemon's CAS is
            // the view model's job (turn.submit carries the block, not a path).
            //
            // Launching is guarded because the photo picker is not on every
            // image: the 16 KiB API 35 emulator has no activity for
            // ACTION_PICK_IMAGES at all, and the unhandled
            // ActivityNotFoundException killed the UI process (971-V F10).
            // A capability that is absent is reported, never fatal.
            SystemAction.PickImage -> launchPicker(SystemAction.PickImage) {
                imagePicker.launch(PickVisualMediaRequest(ActivityResultContracts.PickVisualMedia.ImageOnly))
            }
            SystemAction.PickFile -> launchPicker(SystemAction.PickFile) {
                filePicker.launch(arrayOf("image/*", "text/*"))
            }
            // The capture service owns the consent and the bytes. With a live
            // projection this is a real screenshot attachment; without one it
            // asks for consent and says so, instead of the sheet closing on
            // nothing at all (971-V F10).
            SystemAction.Screenshot -> {
                val capture = ai.diffforge.haider.service.ScreenCaptureService.instance
                if (capture == null) {
                    ai.diffforge.haider.service.ScreenCaptureService.requestConsent(this)
                    viewModel?.noteAttachmentUnavailable(SCREENSHOT_CONSENT_PENDING)
                } else {
                    lifecycleScope.launch {
                        val png = capture.captureOnce()
                        if (png == null) viewModel?.noteAttachmentUnavailable(SCREENSHOT_CONSENT_PENDING)
                        else viewModel?.attach(png, "image/png", name = null)
                    }
                }
            }
            is SystemAction.CopyText -> {
                val clipboard = getSystemService(ClipboardManager::class.java)
                clipboard?.setPrimaryClip(ClipData.newPlainText("haider", action.text))
            }
        }
    }

    /**
     * Launches a picker, or reports that this device has none.
     *
     * `ActivityResultLauncher.launch` throws when nothing resolves the intent,
     * and that throw is on the main thread inside a click handler — it is a
     * crash, not a failed attachment. The notice carries the action's own name
     * so the composer can say which affordance is unavailable here.
     */
    private inline fun launchPicker(action: SystemAction, launch: () -> Unit) {
        try { launch() } catch (_: android.content.ActivityNotFoundException) {
            viewModel?.noteAttachmentUnavailable(
                if (action == SystemAction.PickImage) IMAGE_PICKER_UNAVAILABLE else FILE_PICKER_UNAVAILABLE,
            )
        }
    }

    private fun startSettings(action: String, withPackage: Boolean) {
        val intent = Intent(action).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        if (withPackage) intent.data = Uri.fromParts("package", packageName, null)
        runCatching { startActivity(intent) }
    }

    private fun openUrl(url: String) {
        runCatching {
            CustomTabsIntent.Builder()
                .addMenuItem(getString(R.string.oauth_return_to_haider),
                    ai.diffforge.haider.daemon.DaemonIntents.returnToSettings(this))
                .build().launchUrl(this, Uri.parse(url))
        }
    }
}

private const val PERMISSION_PREFERENCES = "haider_permissions"
private const val KEY_NOTIFICATIONS_REQUESTED = "notifications_requested"

/**
 * Attachment affordances this device does not have.
 *
 * Stable snake_case codes, in the same shape as the daemon's own public codes,
 * because the composer renders an attachment notice verbatim.
 */
const val IMAGE_PICKER_UNAVAILABLE = "image_picker_unavailable"
const val FILE_PICKER_UNAVAILABLE = "file_picker_unavailable"
const val SCREENSHOT_CONSENT_PENDING = "screenshot_consent_pending"

/** The version string shown in the header, drawer and start surface. */
object BuildConfigVersion {
    fun name(context: Context): String = runCatching {
        context.packageManager.getPackageInfo(context.packageName, 0).versionName
    }.getOrNull().orEmpty().ifEmpty { BuildConfig.VERSION_NAME }
}
