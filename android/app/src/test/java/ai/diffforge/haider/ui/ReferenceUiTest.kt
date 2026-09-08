package ai.diffforge.haider.ui

import ai.diffforge.haider.MainActivity
import ai.diffforge.haider.ui.chat.PickerKind
import ai.diffforge.haider.ui.daemon.FakeScenario
import ai.diffforge.haider.ui.daemon.DaemonService
import ai.diffforge.haider.ui.daemon.NeedsInput
import ai.diffforge.haider.ui.scaffold.HAIDER_TOP_BAR_TAG
import ai.diffforge.haider.ui.state.CapabilityApproval
import ai.diffforge.haider.ui.state.Overlay
import ai.diffforge.haider.ui.state.PermissionMode
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.assertIsNotEnabled
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.annotation.Config

/**
 * Owner addition H, the next-diffforge reference pass.
 */
@RunWith(AndroidJUnit4::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xhdpi")
class ReferenceUiTest {

    init {
        ComposeHost.install()
    }

    @get:Rule
    val rule = createAndroidComposeRule<MainActivity>()

    // ---------- H1: the header is controls only ----------

    @Test
    fun `the header holds no text at all`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.Populated))
        rule.waitForIdle()
        val bar = rule.onNodeWithTag(HAIDER_TOP_BAR_TAG).fetchSemanticsNode()
        // The state pill's word is the one string it may carry.
        val text = textUnder(bar)
        assertEquals("the header still prints prose: $text", 1, text.size)
        assertTrue(text.single() in setOf("Idle", "Running", "Needs you", "Waiting", "Error", "Offline"))
    }

    // ---------- H2: no suggestion rows ----------

    @Test
    fun `the empty state offers nothing to tap`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.EmptyRosterReady))
        rule.waitForIdle()
        rule.onNodeWithText("No session yet.").assertIsDisplayed()
        assertEquals(0, rule.onAllNodesWithTextSafe("Summarise the texts I missed today"))
        assertEquals(0, rule.onAllNodesWithTextSafe("Open Settings and turn on Wi-Fi calling"))
    }

    // ---------- H3: three labelled selects ----------

    @Test
    fun `the composer carries model, effort and permissions selects`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.Populated))
        rule.waitForIdle()
        listOf("Model", "Effort", "Permissions").forEach {
            assertTrue("$it select missing", rule.onAllNodesWithTextSafe(it) > 0)
        }
        // The default mode is Auto, and the pill says so.
        assertTrue(rule.onAllNodesWithTextSafe("Auto") > 0)
    }

    // ---------- H5: marks resolve by model before provider ----------

    @Test
    fun `the model wins over the provider when they disagree`() {
        // A session the roster says is "openai" but whose model is Claude's.
        val family = ai.diffforge.haider.ui.components.modelBrandFor(
            model = "claude-opus-4-5",
            provider = "openai",
        )
        assertEquals("claude", family?.key)
        // Provider only when the model says nothing.
        assertEquals(
            "openai",
            ai.diffforge.haider.ui.components.modelBrandFor(null, "openai")?.key,
        )
        // The daemon's own id is not a brand.
        assertEquals(null, ai.diffforge.haider.ui.components.modelBrandFor(null, "haider"))
    }

    @Test
    fun `a white mark takes its light-theme colour`() {
        val openai = ai.diffforge.haider.ui.components.modelBrandFor("gpt-5", null)!!
        assertEquals(androidx.compose.ui.graphics.Color(0xFFFFFFFF), openai.color)
        assertEquals(androidx.compose.ui.graphics.Color(0xFF0D0D0D), openai.colorLight)
        val grok = ai.diffforge.haider.ui.components.modelBrandFor("grok-4", null)!!
        assertEquals(androidx.compose.ui.graphics.Color(0xFF0D0D0D), grok.colorLight)
        // A brand with one colour keeps it in both themes.
        val claude = ai.diffforge.haider.ui.components.modelBrandFor("sonnet", null)!!
        assertEquals(claude.color, claude.colorLight)
    }

    // ---------- H6: Auto suppresses capability cards, and only those ----------

    private fun needsInput(kind: String, title: String, secret: Boolean = false) = NeedsInput(
        kind = kind,
        title = title,
        safeBody = listOf("body"),
        secretAnswer = secret,
    )

    @Test
    fun `Auto answers device capability approvals`() {
        listOf(
            needsInput("permission", "Allow sms.list for the last 20 messages?"),
            needsInput("permission", "Allow screen.capture once?"),
            needsInput("approval", "Run a11y.tap at 120,400?"),
            needsInput("permission", "Allow app.open for Settings?"),
        ).forEach {
            assertTrue(
                "${it.title} should be covered by Auto",
                CapabilityApproval.suppresses(PermissionMode.Auto, it),
            )
            // Ask means ask, always.
            assertFalse(CapabilityApproval.suppresses(PermissionMode.Ask, it))
        }
    }

    @Test
    fun `Auto never answers a question, a secret, or an unknown approval`() {
        listOf(
            needsInput("question", "Send this reply to Amir?"),
            needsInput("secret", "Paste the deploy token", secret = true),
            needsInput("permission", "Paste the deploy token", secret = true),
            needsInput("trust_hook", "Trust this provider certificate?"),
            needsInput("permission", "Allow the provider to bill this account?"),
            needsInput("conflict", "Two workers claim the same session"),
        ).forEach {
            assertFalse(
                "${it.kind}/${it.title} must still ask",
                CapabilityApproval.suppresses(PermissionMode.Auto, it),
            )
        }
    }

    @Test
    fun `the permissions picker sets the mode through the daemon`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        val viewModel = rule.setHaiderApp(service)
        viewModel.openOverlay(Overlay.Picker(PickerKind.Permissions))
        rule.waitForIdle()
        rule.onNodeWithText("Ask").performClick()
        rule.waitForIdle()
        // Not a local toggle: the facade was asked, and the UI shows what it
        // reported back (addition H6).
        assertTrue(service.calls.contains("tool.policy:ask"))
        assertEquals(PermissionMode.Ask, viewModel.state.value.permissionMode)
    }

    @Test
    fun `a production policy ceiling disables unsupported auto without sending a mutation`() {
        val service = ComposeHost.install(FakeScenario.Populated)
        service.setPermissionModeForTest(PermissionMode.Ask)
        val restricted = object : DaemonService by service {
            override val supportedPermissionModes = setOf(PermissionMode.Ask)
        }
        val viewModel = rule.setHaiderApp(restricted)
        viewModel.openOverlay(Overlay.Picker(PickerKind.Permissions))
        rule.waitForIdle()
        rule.onNodeWithText("Auto").assertIsNotEnabled()
        rule.onNodeWithText("Automatic approvals are not available in this build.").assertIsDisplayed()
        viewModel.selectPermissionMode(PermissionMode.Auto)
        rule.waitForIdle()
        assertEquals(PermissionMode.Ask, viewModel.state.value.permissionMode)
        assertFalse(service.calls.any { it.startsWith("tool.policy:") })
    }

    @Test
    fun `the first-run autonomy step names all four popups`() {
        rule.setHaiderApp(ComposeHost.install(FakeScenario.FirstRun))
        rule.waitForIdle()
        rule.onNodeWithText("Let Haider work on its own").performClick()
        rule.waitForIdle()
        listOf("Notifications", "Texts", "Tap and type", "See the screen").forEach {
            assertTrue("$it row missing", rule.onAllNodesWithTextSafe(it) > 0)
        }
    }
}
