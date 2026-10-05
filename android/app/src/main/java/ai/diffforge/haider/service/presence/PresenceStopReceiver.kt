package ai.diffforge.haider.service.presence

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent

/** The presence notification's Stop action. Non-exported; runs in `:daemon` beside the overlay. */
class PresenceStopReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action == ACTION_STOP) CuPresence.controller.stopPressed()
    }

    companion object {
        const val ACTION_STOP = "ai.diffforge.haider.action.CU_PRESENCE_STOP"
    }
}
