package ai.diffforge.haider.state

import ai.diffforge.haider.ui.state.PermissionClassifier
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The whole truth table. `shouldShowRequestPermissionRationale` is false both
 * before the first request and after the final refusal, so it cannot classify a
 * denial on its own.
 */
class PermissionClassifierTest {

    @Test
    fun `never asked is not permanent denial`() {
        assertFalse(
            PermissionClassifier.permanentlyDenied(
                granted = false,
                everRequested = false,
                shouldShowRationale = false,
            ),
        )
        assertTrue(PermissionClassifier.neverAsked(granted = false, everRequested = false))
    }

    @Test
    fun `refused once, with a rationale still available, is ordinary`() {
        assertFalse(
            PermissionClassifier.permanentlyDenied(
                granted = false,
                everRequested = true,
                shouldShowRationale = true,
            ),
        )
    }

    @Test
    fun `refused with no rationale left is permanent`() {
        assertTrue(
            PermissionClassifier.permanentlyDenied(
                granted = false,
                everRequested = true,
                shouldShowRationale = false,
            ),
        )
    }

    @Test
    fun `granted is never a denial, however it got there`() {
        listOf(true, false).forEach { asked ->
            listOf(true, false).forEach { rationale ->
                assertFalse(
                    PermissionClassifier.permanentlyDenied(
                        granted = true,
                        everRequested = asked,
                        shouldShowRationale = rationale,
                    ),
                )
            }
        }
        assertFalse(PermissionClassifier.neverAsked(granted = true, everRequested = false))
    }
}
