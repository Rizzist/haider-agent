package ai.diffforge.haider.update

import android.app.Activity
import android.app.PendingIntent
import android.content.ActivityNotFoundException
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.provider.Settings
import androidx.work.Constraints
import androidx.work.CoroutineWorker
import androidx.work.ExistingPeriodicWorkPolicy
import androidx.work.NetworkType
import androidx.work.PeriodicWorkRequestBuilder
import androidx.work.WorkManager
import androidx.work.WorkerParameters
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import java.lang.ref.WeakReference
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean

sealed interface UpdateUiState {
    data object Hidden : UpdateUiState
    data class Available(val release: AvailableUpdate) : UpdateUiState
    data class Downloading(val release: AvailableUpdate) : UpdateUiState
    data class PermissionRequired(val release: AvailableUpdate) : UpdateUiState
    data class AwaitingConfirmation(
        val tag: String,
        val confirmationReady: Boolean = false,
    ) : UpdateUiState
    data class Error(val message: String, val release: AvailableUpdate?) : UpdateUiState
}

/** Process owner for non-blocking update checks and user-initiated, whole-APK installs. */
object ApkUpdateCoordinator {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private val checker = UpdateChecker(JdkUpdateHttpClient())
    private val checkMutex = Mutex()
    private val started = AtomicBoolean(false)
    private val installing = AtomicBoolean(false)
    private val _state = MutableStateFlow<UpdateUiState>(UpdateUiState.Hidden)
    private var pendingInstall: VerifiedApk? = null
    private var resumedActivity = WeakReference<Activity>(null)

    val state: StateFlow<UpdateUiState> = _state.asStateFlow()

    fun start(context: Context) {
        val appContext = context.applicationContext
        if (!started.compareAndSet(false, true)) return
        scheduleDailyCheck(appContext)
        scope.launch { check(appContext, surfaceErrors = false) }
    }

    fun onAffordanceTapped(context: Context) {
        val appContext = context.applicationContext
        when (val current = _state.value) {
            is UpdateUiState.Available -> downloadThenInstall(appContext, current.release)
            is UpdateUiState.PermissionRequired -> openUnknownSourcesSettings(appContext, current.release)
            is UpdateUiState.AwaitingConfirmation -> {
                if (current.confirmationReady) openStoredConfirmation(context)
            }
            is UpdateUiState.Error -> {
                val ready = pendingInstall
                if (ready != null && ready.release == current.release) {
                    continueVerifiedInstall(appContext, ready)
                } else {
                    current.release?.let { downloadThenInstall(appContext, it) }
                }
            }
            else -> Unit
        }
    }

    fun resumePendingInstall(context: Context) {
        val pending = pendingInstall ?: return
        if (_state.value is UpdateUiState.PermissionRequired && context.packageManager.canRequestPackageInstalls()) {
            continueVerifiedInstall(context, pending)
        }
    }

    fun onActivityResumed(activity: Activity) {
        resumedActivity = WeakReference(activity)
        resumePendingInstall(activity)
        PendingInstallConfirmationStore.recover(activity)?.let { pending ->
            _state.value = UpdateUiState.AwaitingConfirmation(pending.tag, confirmationReady = true)
        }
    }

    fun onActivityPaused(activity: Activity) {
        if (resumedActivity.get() === activity) resumedActivity.clear()
    }

    internal suspend fun check(context: Context, surfaceErrors: Boolean) {
        checkMutex.withLock {
            try {
                val update = withContext(Dispatchers.IO) {
                    checker.check(installedVersion(context))
                }
                if (update != null) {
                    when (_state.value) {
                        UpdateUiState.Hidden,
                        is UpdateUiState.Available -> _state.value = UpdateUiState.Available(update)
                        else -> Unit
                    }
                }
            } catch (error: Exception) {
                if (surfaceErrors) {
                    _state.value = UpdateUiState.Error(error.message ?: "Couldn't check for updates", null)
                }
            }
        }
    }

    internal fun onInstallerStatus(status: Int, message: String?, tag: String?) {
        val release = pendingInstall?.release
        if (tag != null && release != null && tag != release.tag) return
        when (status) {
            android.content.pm.PackageInstaller.STATUS_PENDING_USER_ACTION -> Unit
            android.content.pm.PackageInstaller.STATUS_SUCCESS -> {
                pendingInstall = null
                _state.value = UpdateUiState.Hidden
            }
            else -> {
                _state.value = UpdateUiState.Error(
                    message?.takeIf(String::isNotBlank) ?: "The system installer refused the update",
                    release,
                )
            }
        }
    }

    internal fun onInstallerConfirmationRequired(
        context: Context,
        pending: PendingInstallConfirmation,
    ) {
        _state.value = UpdateUiState.AwaitingConfirmation(pending.tag, confirmationReady = true)
        if (resumedActivity.get() != null) {
            openConfirmation(context, pending)
        } else {
            UpdateConfirmationNotification.show(context, pending)
        }
    }

    internal fun onInstallerConfirmationOpened(tag: String?) {
        _state.value = UpdateUiState.AwaitingConfirmation(tag ?: "Haider update")
    }

    private fun downloadThenInstall(context: Context, release: AvailableUpdate) {
        if (_state.value is UpdateUiState.Downloading) return
        _state.value = UpdateUiState.Downloading(release)
        scope.launch {
            try {
                val verified = withContext(Dispatchers.IO) {
                    checker.downloadAndVerify(release, context.applicationContext.cacheDir)
                }
                pendingInstall = verified
                continueVerifiedInstall(context, verified)
            } catch (_: ChecksumMismatchException) {
                pendingInstall = null
                _state.value = UpdateUiState.Error(
                    "Update refused: the APK SHA-256 did not match the published checksum.",
                    release,
                )
            } catch (error: Exception) {
                pendingInstall = null
                _state.value = UpdateUiState.Error(error.message ?: "Couldn't download the update", release)
            }
        }
    }

    private fun continueVerifiedInstall(context: Context, verified: VerifiedApk) {
        if (!context.packageManager.canRequestPackageInstalls()) {
            _state.value = UpdateUiState.PermissionRequired(verified.release)
            openUnknownSourcesSettings(context, verified.release)
            return
        }
        if (!installing.compareAndSet(false, true)) return
        scope.launch {
            _state.value = UpdateUiState.AwaitingConfirmation(verified.release.tag)
            try {
                withContext(Dispatchers.IO) {
                    PackageInstallerLauncher.install(context.applicationContext, verified)
                }
            } catch (_: ChecksumMismatchException) {
                pendingInstall = null
                _state.value = UpdateUiState.Error(
                    "Update refused: the cached APK no longer matches its verified SHA-256.",
                    verified.release,
                )
            } catch (error: Exception) {
                _state.value = UpdateUiState.Error(
                    error.message ?: "Couldn't prepare the system installer",
                    verified.release,
                )
            } finally {
                installing.set(false)
            }
        }
    }

    private fun openStoredConfirmation(context: Context) {
        val pending = PendingInstallConfirmationStore.recover(context) ?: run {
            _state.value = UpdateUiState.Error("The system installer confirmation expired", pendingInstall?.release)
            return
        }
        openConfirmation(context, pending)
    }

    private fun openConfirmation(context: Context, pending: PendingInstallConfirmation) {
        try {
            pending.action.send()
            _state.value = UpdateUiState.AwaitingConfirmation(pending.tag)
        } catch (error: PendingIntent.CanceledException) {
            PendingInstallConfirmationStore.clear(context)
            _state.value = UpdateUiState.Error(
                error.message ?: "The system installer confirmation expired",
                pendingInstall?.release,
            )
        }
    }

    private fun openUnknownSourcesSettings(context: Context, release: AvailableUpdate) {
        _state.value = UpdateUiState.PermissionRequired(release)
        val intent = Intent(
            Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES,
            Uri.parse("package:${context.packageName}"),
        ).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        try {
            context.startActivity(intent)
        } catch (_: ActivityNotFoundException) {
            _state.value = UpdateUiState.Error(
                "Open Android Settings and allow Haider to install unknown apps, then try again.",
                release,
            )
        }
    }

    @Suppress("DEPRECATION")
    private fun installedVersion(context: Context): String =
        context.packageManager.getPackageInfo(context.packageName, 0).versionName ?: "0.0.0"

    private fun scheduleDailyCheck(context: Context) {
        val constraints = Constraints.Builder()
            .setRequiredNetworkType(NetworkType.CONNECTED)
            .build()
        val request = PeriodicWorkRequestBuilder<DailyUpdateWorker>(1, TimeUnit.DAYS)
            .setInitialDelay(1, TimeUnit.DAYS)
            .setConstraints(constraints)
            .build()
        WorkManager.getInstance(context).enqueueUniquePeriodicWork(
            DAILY_WORK_NAME,
            ExistingPeriodicWorkPolicy.KEEP,
            request,
        )
    }

    private const val DAILY_WORK_NAME = "haider-apk-update-check"
}

class DailyUpdateWorker(
    appContext: Context,
    params: WorkerParameters,
) : CoroutineWorker(appContext, params) {
    override suspend fun doWork(): Result {
        ApkUpdateCoordinator.check(applicationContext, surfaceErrors = false)
        return Result.success()
    }
}
