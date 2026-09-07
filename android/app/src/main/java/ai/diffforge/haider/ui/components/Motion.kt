package ai.diffforge.haider.ui.components

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.database.ContentObserver
import android.os.Handler
import android.os.Looper
import android.os.PowerManager
import android.provider.Settings
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.platform.LocalContext
import androidx.core.content.ContextCompat

/**
 * True when looping animation is allowed: honours power-save and
 * ANIMATOR_DURATION_SCALE. Every looping animation in the app is gated on this.
 * Errored states never animate regardless (haider-tui render.rs:1129-1144).
 */
@Composable
fun motionEnabled(): Boolean {
    val context = LocalContext.current
    var enabled by remember(context) { mutableStateOf(readMotionEnabled(context)) }
    DisposableEffect(context) {
        val refresh = { enabled = readMotionEnabled(context) }
        val powerReceiver = object : BroadcastReceiver() {
            override fun onReceive(receiverContext: Context?, intent: Intent?) = refresh()
        }
        val animatorObserver = object : ContentObserver(Handler(Looper.getMainLooper())) {
            override fun onChange(selfChange: Boolean) = refresh()
        }
        ContextCompat.registerReceiver(
            context,
            powerReceiver,
            IntentFilter(PowerManager.ACTION_POWER_SAVE_MODE_CHANGED),
            ContextCompat.RECEIVER_NOT_EXPORTED,
        )
        context.contentResolver.registerContentObserver(
            Settings.Global.getUriFor(Settings.Global.ANIMATOR_DURATION_SCALE),
            false,
            animatorObserver,
        )
        onDispose {
            runCatching { context.unregisterReceiver(powerReceiver) }
            runCatching { context.contentResolver.unregisterContentObserver(animatorObserver) }
        }
    }
    return enabled
}

private fun readMotionEnabled(context: Context): Boolean {
    val animatorScale = runCatching {
        Settings.Global.getFloat(
            context.contentResolver,
            Settings.Global.ANIMATOR_DURATION_SCALE,
            1f,
        )
    }.getOrDefault(1f)
    val powerSaver = runCatching {
        context.getSystemService(PowerManager::class.java)?.isPowerSaveMode == true
    }.getOrDefault(false)
    return animatorScale > 0f && !powerSaver
}
