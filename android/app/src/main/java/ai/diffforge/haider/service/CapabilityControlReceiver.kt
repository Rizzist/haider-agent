package ai.diffforge.haider.service

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import ai.diffforge.haider.transport.ControlGate

/** Explicit UI -> :daemon opt-in. It is non-exported and never persists a control grant. */
class CapabilityControlReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action == ACTION_SET_CONTROL) ControlGate.enabled = intent.getBooleanExtra(EXTRA_ENABLED, false)
    }
    companion object {
        private const val ACTION_SET_CONTROL = "ai.diffforge.haider.action.SET_CAPABILITY_CONTROL"
        private const val EXTRA_ENABLED = "enabled"
        /** Call only when the user changes the device-control toggle; authentication alone never calls this. */
        fun setUserControlEnabled(context: Context, enabled: Boolean) {
            context.sendBroadcast(Intent(context, CapabilityControlReceiver::class.java)
                .setPackage(context.packageName).setAction(ACTION_SET_CONTROL).putExtra(EXTRA_ENABLED, enabled))
        }
    }
}
