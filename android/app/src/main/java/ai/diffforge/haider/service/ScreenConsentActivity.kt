package ai.diffforge.haider.service

import android.app.Activity
import android.content.Intent
import android.media.projection.MediaProjectionManager
import android.os.Bundle

class ScreenConsentActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        if (savedInstanceState == null) {
            val manager = getSystemService(MediaProjectionManager::class.java)
            startActivityForResult(manager.createScreenCaptureIntent(), REQUEST_CAPTURE)
        }
    }

    @Deprecated("Activity result API is sufficient for this transparent system consent activity")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode == REQUEST_CAPTURE && resultCode == RESULT_OK && data != null) {
            startForegroundService(
                ScreenCaptureService.projectionIntent(this, resultCode, data),
            )
        } else if (requestCode == REQUEST_CAPTURE) {
            // Consent Activity is in the UI process; release state in the capability process.
            stopService(Intent(this, ScreenCaptureService::class.java))
        }
        finish()
    }

    private companion object {
        const val REQUEST_CAPTURE = 2001
    }
}
