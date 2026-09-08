package ai.diffforge.haider.ui.settings

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.accounts.Account
import ai.diffforge.haider.ui.accounts.AccountResult
import ai.diffforge.haider.ui.accounts.AccountsRepository
import ai.diffforge.haider.ui.accounts.AddAccountForm
import ai.diffforge.haider.ui.accounts.AddAccountFormPolicy
import ai.diffforge.haider.ui.accounts.AuthKind
import ai.diffforge.haider.ui.accounts.CustomAuthMode
import ai.diffforge.haider.ui.accounts.CustomServerController
import ai.diffforge.haider.ui.accounts.OAuthAttemptController
import ai.diffforge.haider.ui.accounts.OAuthStyle
import ai.diffforge.haider.ui.accounts.ProviderDescriptor
import ai.diffforge.haider.ui.accounts.SecretBuffer
import ai.diffforge.haider.ui.components.BrandMarkOnly
import ai.diffforge.haider.ui.components.ForgeButton
import ai.diffforge.haider.ui.components.ForgeButtonKind
import ai.diffforge.haider.ui.components.ForgeChip
import ai.diffforge.haider.ui.components.ForgeIconButton
import ai.diffforge.haider.ui.components.LabelledField
import ai.diffforge.haider.ui.components.SessionGlyph
import ai.diffforge.haider.ui.daemon.SessionVisualState
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawing
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.foundation.clickable
import androidx.compose.material.icons.rounded.Add
import androidx.compose.material.icons.rounded.ArrowBack
import androidx.compose.material.icons.rounded.Check
import androidx.compose.material.icons.rounded.Visibility
import androidx.compose.material.icons.rounded.VisibilityOff
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextOverflow
import kotlinx.coroutines.launch

/** Staging failed for a reason that is not the key: connection, expiry, capacity. */
const val STAGING_UNAVAILABLE = "staging_unavailable"

const val ACCOUNTS_KEY_FIELD_TAG = "accounts_key_field"
const val ACCOUNTS_OAUTH_WAITING_TAG = "accounts_oauth_waiting"

/**
 * Settings -> Accounts: add an API key (provider pick, masked entry, validate,
 * save, remove) or sign in through a provider's OAuth flow.
 *
 * Two things here are deliberate and load-bearing:
 *
 *  - the key lives in a [SecretBuffer], and every read of it runs inside
 *    `use { }`, so the bytes are wiped on success, on failure and on
 *    cancellation. The buffer is also wiped when the form is cancelled, when
 *    the mode changes, and in `onDispose`;
 *  - the OAuth attempt lives in [OAuthAttemptController], **outside** this
 *    composition, because returning through `haider://oauth/return` navigates
 *    to Settings and disposes this screen. A `remember`ed flow id would be gone
 *    exactly when the status poll needs it.
 */
@Composable
fun AccountsScreen(
    repository: AccountsRepository,
    oauth: OAuthAttemptController,
    onBack: () -> Unit,
    onOpenUrl: (String) -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = Forge.colors
    val type = Forge.type
    val scope = rememberCoroutineScope()
    val providers by repository.providers.collectAsState()
    val snapshot by repository.snapshot.collectAsState()
    val attempt by oauth.attempt.collectAsState()
    val controllerNotice by oauth.notice.collectAsState()

    var mode by remember { mutableStateOf(AddAccountForm.None) }
    var provider by remember { mutableStateOf<String?>(null) }
    var alias by remember { mutableStateOf("") }
    var keyText by remember { mutableStateOf("") }
    var revealKey by remember { mutableStateOf(false) }
    var localNotice by remember { mutableStateOf<String?>(null) }
    var busy by remember { mutableStateOf(false) }
    val secret = remember { SecretBuffer() }
    // Once staged, the plaintext is not needed again: the reference is what
    // `account.login_api` claims. Holding the key past staging is the whole
    // finding — "Key validated" left the masked field populated.
    var stagedReference by remember { mutableStateOf<String?>(null) }
    var addSheet by remember { mutableStateOf(false) }
    var openAccount by remember { mutableStateOf<String?>(null) }

    // The custom-server card runs four doors deep and holds its own key, so it
    // has its own controller and its own buffer — the API-key form's `secret`
    // is not shared with it, and neither buffer can outlive its form.
    val custom = remember(repository) { CustomServerController(repository) }
    val customForm by custom.form.collectAsState()
    val customNotice by custom.notice.collectAsState()
    var customKeyText by remember { mutableStateOf("") }
    val notice = localNotice ?: customNotice ?: controllerNotice

    // Neither buffer outlives the screen, however the screen ends.
    DisposableEffect(Unit) {
        onDispose { secret.wipe(); custom.forget() }
    }
    // Staging clears the field; validation happens only when Save commits it.
    val stagedHint = stagedReference != null && keyText.isBlank()
    // There is one pending add form, ever (971-tui-fixes F1b), and whatever was
    // typed into the outgoing one goes with it: leaving a form clears its key,
    // its staged reference and its fields, so a replaced form cannot leave an
    // abandoned secret behind the one now on screen.
    LaunchedEffect(mode) {
        if (AddAccountFormPolicy.discardsSecret(AddAccountForm.ApiKey, mode)) {
            secret.wipe()
            keyText = ""
            revealKey = false
            stagedReference = null
        }
        if (AddAccountFormPolicy.discardsSecret(AddAccountForm.CustomServer, mode)) {
            custom.close()
            customKeyText = ""
        } else if (customForm == null) {
            custom.open()
        }
    }
    LaunchedEffect(Unit) {
        repository.refresh()
        repository.refreshProviders()
    }

    fun clearSecret() {
        secret.wipe()
        keyText = ""
        revealKey = false
    }

    fun forgetStaging() {
        clearSecret()
        stagedReference = null
    }

    Column(
        modifier = modifier
            .fillMaxSize()
            .background(colors.bg)
            .windowInsetsPadding(WindowInsets.safeDrawing),
    ) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .heightIn(min = ForgeSize.header)
                .padding(horizontal = ForgeSpace.xs),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            ForgeIconButton(
                onClick = { forgetStaging(); onBack() },
                contentDescription = stringResource(R.string.action_back),
            ) {
                Icon(
                    Icons.Rounded.ArrowBack,
                    contentDescription = null,
                    tint = colors.textSoft,
                    modifier = Modifier.size(ForgeSize.icon),
                )
            }
            Text(
                stringResource(R.string.accounts_title),
                style = type.sessionTitle,
                color = colors.text,
                modifier = Modifier.padding(start = ForgeSpace.md),
            )
        }

        Column(
            Modifier
                .weight(1f)
                .verticalScroll(rememberScrollState())
                .padding(horizontal = ForgeSpace.xl, vertical = ForgeSpace.lg),
            verticalArrangement = Arrangement.spacedBy(ForgeSpace.lg),
        ) {
            notice?.let { Text(it, style = type.sessionMeta, color = colors.accent) }

            if (snapshot.accounts.isEmpty()) {
                Text(
                    stringResource(R.string.accounts_empty),
                    style = type.chatBody,
                    color = colors.textMuted,
                )
            } else {
                snapshot.accounts.forEach { account ->
                    AccountRow(account = account, onOpen = { openAccount = account.alias })
                }
            }

            // One action, two options behind it (addition F, A2).
            if (mode == AddAccountForm.None) {
                ForgeButton(
                    text = stringResource(R.string.accounts_add),
                    onClick = { addSheet = true },
                    leading = {
                        Icon(
                            Icons.Rounded.Add,
                            contentDescription = null,
                            tint = colors.accentInk,
                            modifier = Modifier.size(ForgeSize.iconSm),
                        )
                    },
                )
            }

            // Independent of the add form, and of whether this screen was
            // recreated while the browser had the foreground (verify-6 O4).
            attempt?.let { live ->
                Card { LiveOAuthPanel(live = live, onOpenUrl = onOpenUrl, onCancel = { oauth.cancel() }) }
            }

            when (mode) {
                AddAccountForm.None -> Unit

                AddAccountForm.ApiKey -> Card {
                    ProviderPicker(
                        providers = providers.filter { it.supportsApiKey },
                        selected = provider,
                        onSelect = { provider = it },
                    )
                    LabelledField(
                        label = stringResource(R.string.accounts_alias),
                        value = alias,
                        onValueChange = { alias = it },
                    )
                    LabelledField(
                        label = stringResource(R.string.accounts_api_key),
                        value = keyText,
                        onValueChange = {
                            keyText = it
                            secret.set(it)
                            // Editing invalidates a previous staging.
                            stagedReference = null
                        },
                        masked = !revealKey,
                        tag = ACCOUNTS_KEY_FIELD_TAG,
                        trailing = {
                            ForgeIconButton(
                                onClick = { revealKey = !revealKey },
                                contentDescription = stringResource(
                                    if (revealKey) R.string.cd_hide_key else R.string.cd_show_key,
                                ),
                            ) {
                                Icon(
                                    if (revealKey) Icons.Rounded.VisibilityOff else Icons.Rounded.Visibility,
                                    contentDescription = null,
                                    tint = colors.textMuted,
                                    modifier = Modifier.size(ForgeSize.iconSm),
                                )
                            }
                        },
                    )
                    Text(
                        stringResource(
                            when {
                                // Staging is not validation and the commit has
                                // not returned: saying "validated" while both
                                // buttons are disabled was a claim about a call
                                // still in flight (verify-8 O7).
                                busy -> R.string.accounts_key_signing_in
                                stagedHint -> R.string.accounts_key_staged
                                else -> R.string.accounts_key_never_leaves
                            },
                        ),
                        style = type.sessionMeta,
                        color = if (stagedHint) colors.accent else colors.textMuted,
                    )
                    Row(horizontalArrangement = Arrangement.spacedBy(ForgeSpace.md)) {
                        // No Validate button. The frozen wire has no
                        // validation-only door: account.login_api is what
                        // checks a key, and it commits at the same time. A
                        // button that answered "Key validated." was describing
                        // a call that does not exist (lane 971-3, UI-14).
                        ForgeButton(
                            text = stringResource(R.string.action_save),
                            onClick = {
                                val chosen = provider ?: return@ForgeButton
                                busy = true
                                scope.launch {
                                    var result: AccountResult? = null
                                    try {
                                        val reference = stagedReference
                                            ?: secret.use { repository.stageApiKey(it) }
                                        // The moment the vault has it, this
                                        // screen does not. Round 7 kept the
                                        // plaintext in the field — revealable
                                        // with the eye button — for the whole
                                        // login round trip, which is exactly
                                        // the window an over-the-shoulder
                                        // reader needs (verify-7 P2).
                                        clearSecret()
                                        stagedReference = reference
                                        result = if (reference == null) {
                                            AccountResult.Failed(STAGING_UNAVAILABLE)
                                        } else {
                                            repository.commitStagedApiKey(
                                                chosen,
                                                alias.takeIf(String::isNotBlank),
                                                reference,
                                            )
                                        }
                                    } finally {
                                        // Whatever happened — success, refusal,
                                        // cancellation — the key is gone.
                                        forgetStaging()
                                        busy = false
                                    }
                                    when (result) {
                                        AccountResult.Ok -> {
                                            localNotice = "API key added and validated."
                                            mode = AddAccountForm.None
                                            alias = ""
                                            repository.refresh()
                                        }
                                        is AccountResult.Failed -> localNotice = result.publicCode
                                        null -> Unit
                                    }
                                }
                            },
                            enabled = provider != null &&
                                (keyText.isNotBlank() || stagedReference != null) && !busy,
                        )
                        ForgeButton(
                            text = stringResource(R.string.action_cancel),
                            onClick = {
                                forgetStaging()
                                mode = AddAccountForm.None
                            },
                            kind = ForgeButtonKind.Ghost,
                            enabled = !busy,
                        )
                    }
                }

                // The live attempt is rendered above, outside this form: it
                // belongs to a process-scoped controller and outlives the
                // composition, so a browser return that resets `mode` must not
                // make a running sign-in disappear (verify-6 O4).
                AddAccountForm.SignIn -> if (attempt == null) Card {
                        ProviderPicker(
                            providers = providers.filter { it.supportsOAuth },
                            selected = provider,
                            onSelect = { provider = it },
                        )
                        LabelledField(
                            label = stringResource(R.string.accounts_alias),
                            value = alias,
                            onValueChange = { alias = it },
                        )
                        ForgeButton(
                            text = stringResource(R.string.accounts_oauth_start),
                            onClick = {
                                val chosen = provider ?: return@ForgeButton
                                busy = true
                                scope.launch {
                                    try {
                                        // The daemon composed the URL and owns
                                        // the loopback listener; the UI never
                                        // invents a redirect.
                                        oauth.start(chosen, alias.takeIf(String::isNotBlank))
                                            ?.let(onOpenUrl)
                                    } finally {
                                        busy = false
                                    }
                                }
                            },
                            enabled = provider != null && !busy,
                        )
                }

                // The custom-server card. Its whole flow lives in the
                // controller, so this branch only renders what the form says
                // and hands gestures back — the same split the OAuth panel has.
                AddAccountForm.CustomServer -> customForm?.let { form ->
                    Card {
                        CustomServerCard(
                            form = form,
                            keyText = customKeyText,
                            onKeyText = { value ->
                                customKeyText = value
                                custom.setKey(value)
                            },
                            onEdit = custom::edit,
                            onAuthMode = { authMode ->
                                if (authMode == CustomAuthMode.None) customKeyText = ""
                                custom.setAuthMode(authMode)
                            },
                            onSubmit = {
                                // The plaintext leaves this screen with the
                                // gesture that submits it, not when the round
                                // trip happens to finish.
                                customKeyText = ""
                                scope.launch {
                                    if (custom.submit()) mode = AddAccountForm.None
                                }
                            },
                            onCancel = {
                                customKeyText = ""
                                mode = AddAccountForm.None
                            },
                        )
                    }
                }
            }
        }
    }

    if (addSheet) {
        AddAccountSheet(
            onDismiss = { addSheet = false },
            onApiKey = { addSheet = false; mode = AddAccountForm.ApiKey; localNotice = null },
            onSignIn = { addSheet = false; mode = AddAccountForm.SignIn; localNotice = null },
            onCustomServer = {
                addSheet = false
                // Opening the card replaces any pending form, and the effect on
                // `mode` wipes what the outgoing one was holding.
                mode = AddAccountFormPolicy.open(AddAccountForm.CustomServer)
                localNotice = null
            },
        )
    }
    openAccount?.let { alias ->
        snapshot.accounts.firstOrNull { it.alias == alias }?.let { account ->
            AccountDetailSheet(
                account = account,
                onDismiss = { openAccount = null },
                // Both of these return a result, and round 6 threw both away:
                // a refused revision, a lost connection or a confirmation
                // requirement announced "Account removed." anyway (lane 971-3
                // handoff). Announce only what actually happened.
                onSetActive = {
                    openAccount = null
                    scope.launch {
                        when (val result = repository.setActive(account.alias)) {
                            AccountResult.Ok -> localNotice = null
                            is AccountResult.Failed -> localNotice = result.publicCode
                        }
                        repository.refresh()
                    }
                },
                onRemove = {
                    openAccount = null
                    scope.launch {
                        when (val result = repository.remove(account.alias)) {
                            AccountResult.Ok -> localNotice = "Account removed."
                            is AccountResult.Failed -> localNotice = result.publicCode
                        }
                        repository.refresh()
                    }
                },
            )
        }
    }
}

/** The one Add action's three options. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun AddAccountSheet(
    onDismiss: () -> Unit,
    onApiKey: () -> Unit,
    onSignIn: () -> Unit,
    onCustomServer: () -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    ModalBottomSheet(
        onDismissRequest = onDismiss,
        sheetState = rememberModalBottomSheetState(),
        containerColor = colors.surfaceRaised,
        shape = ForgeShapes.sheet,
    ) {
        Column(
            Modifier.padding(start = ForgeSpace.xl, end = ForgeSpace.xl, bottom = ForgeSpace.xxxl),
            verticalArrangement = Arrangement.spacedBy(ForgeSpace.md),
        ) {
            Text(stringResource(R.string.accounts_add), style = type.h4, color = colors.text)
            listOf(
                stringResource(R.string.accounts_option_api_key) to onApiKey,
                stringResource(R.string.accounts_option_sign_in) to onSignIn,
                stringResource(R.string.accounts_option_custom_server) to onCustomServer,
            ).forEach { (label, action) ->
                Row(
                    modifier = Modifier
                        .fillMaxWidth()
                        .heightIn(min = ForgeSize.touch)
                        .clip(ForgeShapes.row)
                        .clickable(onClick = action)
                        .padding(horizontal = ForgeSpace.lg)
                        .semantics { contentDescription = label; role = Role.Button },
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Text(label, style = type.button, color = colors.text)
                }
            }
        }
    }
}

/**
 * Provider glyph, label, identity line, and a check when it is the one in use.
 * No ACTIVE badge (the check says it) and no Delete button on the row — a
 * destructive action does not belong on a list row that is one mis-tap wide
 * (addition F, A1/G3).
 */
@Composable
private fun AccountRow(account: Account, onOpen: () -> Unit) {
    val colors = Forge.colors
    val type = Forge.type
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .heightIn(min = ForgeSize.touch)
            .clip(ForgeShapes.row)
            .clickable(onClick = onOpen)
            .padding(horizontal = ForgeSpace.lg, vertical = ForgeSpace.sm),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        BrandMarkOnly(model = null, provider = account.provider)
        Column(
            Modifier
                .weight(1f)
                .padding(start = ForgeSpace.lg),
        ) {
            Text(
                account.label ?: account.alias,
                style = type.sessionTitle,
                color = colors.text,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            Text(
                listOfNotNull(
                    account.identity,
                    // An unknown auth kind says so; it does not default to a
                    // credential type the daemon never named (971-3, UI-12).
                    when (account.authKind) {
                        AuthKind.OAuth -> "sign-in"
                        AuthKind.ApiKey -> "API key"
                        AuthKind.Unknown -> "sign-in method unknown"
                    },
                ).joinToString(" · "),
                style = type.sessionMeta,
                color = colors.textMuted,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
        }
        if (account.active) {
            Icon(
                Icons.Rounded.Check,
                contentDescription = stringResource(R.string.accounts_active),
                tint = colors.accent,
                modifier = Modifier.size(ForgeSize.iconSm),
            )
        }
    }
}

/** Delete lives here, behind a confirmation (addition F, A1). */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun AccountDetailSheet(
    account: Account,
    onDismiss: () -> Unit,
    onSetActive: () -> Unit,
    onRemove: () -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    var confirming by remember { mutableStateOf(false) }
    ModalBottomSheet(
        onDismissRequest = onDismiss,
        sheetState = rememberModalBottomSheetState(),
        containerColor = colors.surfaceRaised,
        shape = ForgeShapes.sheet,
    ) {
        Column(
            Modifier.padding(start = ForgeSpace.xl, end = ForgeSpace.xl, bottom = ForgeSpace.xxxl),
            verticalArrangement = Arrangement.spacedBy(ForgeSpace.lg),
        ) {
            Text(account.label ?: account.alias, style = type.h4, color = colors.text)
            Text(
                listOfNotNull(account.provider, account.identity).joinToString(" · "),
                style = type.sessionMeta,
                color = colors.textMuted,
            )
            if (!account.active) {
                ForgeButton(
                    text = stringResource(R.string.accounts_set_active),
                    onClick = onSetActive,
                    kind = ForgeButtonKind.Ghost,
                )
            }
            if (confirming) {
                Text(
                    stringResource(R.string.accounts_delete_confirm),
                    style = type.sessionMeta,
                    color = colors.red,
                )
                Row(horizontalArrangement = Arrangement.spacedBy(ForgeSpace.md)) {
                    ForgeButton(
                        text = stringResource(R.string.action_cancel),
                        onClick = { confirming = false },
                        kind = ForgeButtonKind.Ghost,
                    )
                    ForgeButton(
                        text = stringResource(R.string.action_delete),
                        onClick = onRemove,
                        kind = ForgeButtonKind.Destructive,
                    )
                }
            } else {
                ForgeButton(
                    text = stringResource(R.string.action_delete),
                    onClick = { confirming = true },
                    kind = ForgeButtonKind.Destructive,
                )
            }
        }
    }
}

@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun ProviderPicker(
    providers: List<ProviderDescriptor>,
    selected: String?,
    onSelect: (String) -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    Column(verticalArrangement = Arrangement.spacedBy(ForgeSpace.md)) {
        Text(stringResource(R.string.accounts_provider), style = type.sessionMeta, color = colors.textMuted)
        // A fixed Row squeezed the fifth provider to a four-pixel sliver on a
        // 360 dp phone. Wrapping keeps every label its own full width.
        FlowRow(
            horizontalArrangement = Arrangement.spacedBy(ForgeSpace.md),
            verticalArrangement = Arrangement.spacedBy(ForgeSpace.xs),
        ) {
            providers.forEach { descriptor ->
                ForgeChip(
                    onClick = { onSelect(descriptor.id) },
                    selected = selected == descriptor.id,
                    enabled = descriptor.available,
                    contentDescription = descriptor.label,
                ) {
                    Text(
                        descriptor.label,
                        style = type.chip,
                        color = when {
                            !descriptor.available -> colors.textMuted
                            selected == descriptor.id -> colors.accent
                            else -> colors.textSoft
                        },
                    )
                }
            }
        }
    }
}


/**
 * A sign-in that is already running.
 *
 * It is drawn from the process-scoped controller alone, so leaving for the
 * browser and coming back — which recreates this screen with a fresh, empty
 * add-form mode — still shows the waiting panel and its Cancel (verify-6 O4).
 */
@Composable
private fun LiveOAuthPanel(
    live: OAuthAttemptController.Attempt,
    onOpenUrl: (String) -> Unit,
    onCancel: () -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    Column(
        verticalArrangement = Arrangement.spacedBy(ForgeSpace.md),
        modifier = Modifier.testTag(ACCOUNTS_OAUTH_WAITING_TAG),
    ) {
        Text(
            when (live.phase) {
                OAuthAttemptController.Phase.Waiting ->
                    if (live.flow.style == OAuthStyle.Device) {
                        stringResource(R.string.accounts_oauth_device)
                    } else {
                        stringResource(R.string.accounts_oauth_waiting)
                    }
                OAuthAttemptController.Phase.Exchanging ->
                    stringResource(R.string.accounts_oauth_exchanging)
                OAuthAttemptController.Phase.Claiming ->
                    stringResource(R.string.accounts_oauth_claiming)
                OAuthAttemptController.Phase.Failed,
                OAuthAttemptController.Phase.Committed,
                -> stringResource(R.string.accounts_oauth_waiting)
            },
            style = type.chatBody,
            color = colors.text,
        )
        live.flow.userCode?.let {
            Text(
                stringResource(R.string.accounts_oauth_code, it),
                // mono: a device code the person reads out character by
                // character; proportional digits invite a misread.
                style = type.numeric,
                color = colors.accent,
            )
        }
        Text(
            stringResource(R.string.accounts_oauth_return_hint),
            style = type.sessionMeta,
            color = colors.textMuted,
        )
        Row(horizontalArrangement = Arrangement.spacedBy(ForgeSpace.md)) {
            live.flow.authorizationUrl?.let { url ->
                ForgeButton(
                    text = stringResource(R.string.accounts_oauth_open_again),
                    onClick = { onOpenUrl(url) },
                    kind = ForgeButtonKind.Ghost,
                )
            }
            ForgeButton(
                text = stringResource(R.string.action_cancel),
                onClick = onCancel,
                kind = ForgeButtonKind.Ghost,
            )
        }
    }
}
