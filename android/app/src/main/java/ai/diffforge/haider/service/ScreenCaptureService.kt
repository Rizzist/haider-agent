package ai.diffforge.haider.service

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.graphics.Bitmap
import android.graphics.PixelFormat
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.Image
import android.media.ImageReader
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import ai.diffforge.haider.R
import ai.diffforge.haider.transport.CapabilityBus
import androidx.core.content.IntentCompat
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withTimeoutOrNull
import java.io.ByteArrayOutputStream
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicReference
import kotlin.coroutines.resume

class ScreenCaptureService : Service() {
    private val captureMutex = Mutex()
    private val projectionLock = Any()
    private val mainHandler = Handler(Looper.getMainLooper())

    @Volatile
    private var projection: MediaProjection? = null

    private var projectionCallback: MediaProjection.Callback? = null

    override fun onCreate() {
        super.onCreate()
        instance = this
        createNotificationChannel()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        startInForeground()
        if (intent?.action != ACTION_SET_PROJECTION) {
            stopForeground(STOP_FOREGROUND_REMOVE)
            stopSelf(startId)
        } else {
            val resultCode = intent.getIntExtra(EXTRA_RESULT_CODE, RESULT_CODE_MISSING)
            val resultData = IntentCompat.getParcelableExtra(
                intent,
                EXTRA_RESULT_DATA,
                Intent::class.java,
            )
            if (resultCode != RESULT_CODE_MISSING && resultData != null) {
                configureProjection(resultCode, resultData)
            } else {
                stopForeground(STOP_FOREGROUND_REMOVE)
                stopSelf(startId)
            }
        }
        return START_NOT_STICKY
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onDestroy() {
        releaseProjection()
        if (instance === this) instance = null
        super.onDestroy()
    }

    /**
     * Captures exactly one PNG from the current consent token. Android 14 permits only one virtual
     * display per consent session, so the projection is consumed and fresh consent is requested for
     * the next call.
     */
    suspend fun captureOnce(): ByteArray? = captureMutex.withLock {
        val activeProjection = projection
        if (activeProjection == null) {
            requestConsent(this)
            return@withLock null
        }

        val png = try {
            captureProjection(activeProjection)
        } finally {
            releaseProjection(activeProjection)
            stopForeground(STOP_FOREGROUND_REMOVE)
            stopSelf()
        }
        if (png == null) requestConsent(this)
        png
    }

    fun hasProjection(): Boolean = projection != null

    private fun configureProjection(resultCode: Int, resultData: Intent) {
        releaseProjection()
        val manager = getSystemService(MediaProjectionManager::class.java)
        val newProjection = try {
            manager.getMediaProjection(resultCode, resultData)
        } catch (_: SecurityException) {
            null
        }
        if (newProjection == null) {
            CapabilityBus.set(CAPABILITY, false)
            stopForeground(STOP_FOREGROUND_REMOVE)
            stopSelf()
            return
        }

        val callback = object : MediaProjection.Callback() {
            override fun onStop() {
                onProjectionStopped(newProjection)
            }
        }
        try {
            newProjection.registerCallback(callback, mainHandler)
        } catch (_: RuntimeException) {
            newProjection.stop()
            CapabilityBus.set(CAPABILITY, false)
            stopForeground(STOP_FOREGROUND_REMOVE)
            stopSelf()
            return
        }
        synchronized(projectionLock) {
            projection = newProjection
            projectionCallback = callback
        }
        CapabilityBus.set(CAPABILITY, true)
    }

    private suspend fun captureProjection(activeProjection: MediaProjection): ByteArray? =
        withTimeoutOrNull(CAPTURE_TIMEOUT_MS) {
            suspendCancellableCoroutine { continuation ->
                val metrics = resources.displayMetrics
                val width = metrics.widthPixels.coerceAtLeast(1)
                val height = metrics.heightPixels.coerceAtLeast(1)
                val reader = ImageReader.newInstance(width, height, PixelFormat.RGBA_8888, 2)
                val display = AtomicReference<VirtualDisplay?>()
                val completed = AtomicBoolean(false)

                fun cleanUp() {
                    display.getAndSet(null)?.release()
                    reader.setOnImageAvailableListener(null, null)
                    reader.close()
                }

                fun complete(value: ByteArray?) {
                    if (!completed.compareAndSet(false, true)) return
                    cleanUp()
                    if (continuation.isActive) continuation.resume(value)
                }

                reader.setOnImageAvailableListener({ source ->
                    val image = source.acquireLatestImage() ?: return@setOnImageAvailableListener
                    val png = try {
                        imageToPng(image, width, height)
                    } catch (_: RuntimeException) {
                        null
                    } finally {
                        image.close()
                    }
                    complete(png)
                }, mainHandler)

                continuation.invokeOnCancellation {
                    if (completed.compareAndSet(false, true)) cleanUp()
                }

                try {
                    display.set(
                        activeProjection.createVirtualDisplay(
                            DISPLAY_NAME,
                            width,
                            height,
                            metrics.densityDpi,
                            DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR,
                            reader.surface,
                            null,
                            mainHandler,
                        ),
                    )
                } catch (_: RuntimeException) {
                    complete(null)
                }
            }
        }

    private fun imageToPng(image: Image, width: Int, height: Int): ByteArray? {
        val plane = image.planes.firstOrNull() ?: return null
        val pixelStride = plane.pixelStride
        val rowStride = plane.rowStride
        if (pixelStride <= 0 || rowStride < pixelStride * width) return null
        val paddedWidth = width + (rowStride - pixelStride * width) / pixelStride
        val paddedBitmap = Bitmap.createBitmap(paddedWidth, height, Bitmap.Config.ARGB_8888)
        plane.buffer.rewind()
        paddedBitmap.copyPixelsFromBuffer(plane.buffer)
        val outputBitmap = if (paddedWidth == width) {
            paddedBitmap
        } else {
            Bitmap.createBitmap(paddedBitmap, 0, 0, width, height)
        }

        return try {
            ByteArrayOutputStream().use { output ->
                if (outputBitmap.compress(Bitmap.CompressFormat.PNG, 100, output)) {
                    output.toByteArray()
                } else {
                    null
                }
            }
        } finally {
            if (outputBitmap !== paddedBitmap) outputBitmap.recycle()
            paddedBitmap.recycle()
        }
    }

    private fun releaseProjection(expected: MediaProjection? = null) {
        val projectionToStop: MediaProjection
        val callbackToRemove: MediaProjection.Callback?
        synchronized(projectionLock) {
            val current = projection ?: return
            if (expected != null && current !== expected) return
            projectionToStop = current
            callbackToRemove = projectionCallback
            projection = null
            projectionCallback = null
        }
        callbackToRemove?.let {
            try {
                projectionToStop.unregisterCallback(it)
            } catch (_: RuntimeException) {
                // The projection may already have been stopped by the system.
            }
        }
        try {
            projectionToStop.stop()
        } catch (_: RuntimeException) {
            // A consumed MediaProjection may already be stopped.
        }
        CapabilityBus.set(CAPABILITY, false)
    }

    private fun onProjectionStopped(stoppedProjection: MediaProjection) {
        synchronized(projectionLock) {
            if (projection !== stoppedProjection) return
            projection = null
            projectionCallback = null
        }
        CapabilityBus.set(CAPABILITY, false)
        stopForeground(STOP_FOREGROUND_REMOVE)
        stopSelf()
    }

    private fun createNotificationChannel() {
        val channel = NotificationChannel(
            NOTIFICATION_CHANNEL_ID,
            getString(R.string.screen_capture_channel_name),
            NotificationManager.IMPORTANCE_LOW,
        ).apply {
            description = getString(R.string.screen_capture_channel_description)
            setShowBadge(false)
        }
        getSystemService(NotificationManager::class.java).createNotificationChannel(channel)
    }

    private fun startInForeground() {
        val notification = Notification.Builder(this, NOTIFICATION_CHANNEL_ID)
            .setSmallIcon(android.R.drawable.ic_menu_camera)
            .setContentTitle(getString(R.string.screen_capture_notification_title))
            .setContentText(getString(R.string.screen_capture_notification_text))
            .setCategory(Notification.CATEGORY_SERVICE)
            .setOngoing(true)
            .build()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(
                NOTIFICATION_ID,
                notification,
                ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION,
            )
        } else {
            startForeground(NOTIFICATION_ID, notification)
        }
    }

    companion object {
        private const val ACTION_SET_PROJECTION =
            "ai.diffforge.haider.action.SET_MEDIA_PROJECTION"
        private const val EXTRA_RESULT_CODE = "resultCode"
        private const val EXTRA_RESULT_DATA = "resultData"
        private const val RESULT_CODE_MISSING = Int.MIN_VALUE
        private const val NOTIFICATION_CHANNEL_ID = "screen_capture"
        private const val NOTIFICATION_ID = 1001
        private const val DISPLAY_NAME = "Haider screen capture"
        private const val CAPTURE_TIMEOUT_MS = 5_000L
        private const val CAPABILITY = "screenCapture"

        @Volatile
        var instance: ScreenCaptureService? = null
            private set

        fun projectionIntent(context: Context, resultCode: Int, data: Intent): Intent =
            Intent(context, ScreenCaptureService::class.java)
                .setAction(ACTION_SET_PROJECTION)
                .putExtra(EXTRA_RESULT_CODE, resultCode)
                .putExtra(EXTRA_RESULT_DATA, data)

        fun requestConsent(context: Context) {
            context.startActivity(
                Intent(context, ScreenConsentActivity::class.java)
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            )
        }
    }
}
