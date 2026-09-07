package ai.diffforge.haider.ui.settings

import ai.diffforge.haider.R
import ai.diffforge.haider.ui.accounts.Account
import ai.diffforge.haider.ui.accounts.AccountResult
import ai.diffforge.haider.ui.accounts.AccountsRepository
import ai.diffforge.haider.ui.accounts.AuthKind
import ai.diffforge.haider.ui.accounts.OAuthAttemptController
import ai.diffforge.haider.ui.accounts.OAuthStyle
import ai.diffforge.haider.ui.accounts.ProviderDescriptor
import ai.diffforge.haider.ui.accounts.SecretBuffer
import ai.diffforge.haider.ui.components.ForgeButton
import ai.diffforge.haider.ui.components.ForgeButtonKind
import ai.diffforge.haider.ui.components.ForgeChip
import ai.diffforge.haider.ui.components.ForgeIconButton
import ai.diffforge.haider.ui.components.StatePill
import ai.diffforge.haider.ui.theme.Forge
import ai.diffforge.haider.ui.theme.ForgeShapes
import ai.diffforge.haider.ui.theme.ForgeSize
import ai.diffforge.haider.ui.theme.ForgeSpace
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Arrangement
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
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.ArrowBack
import androidx.compose.material.icons.rounded.Visibility
import androidx.compose.material.icons.rounded.VisibilityOff
import androidx.compose.material3.Icon
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
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.text.style.TextOverflow
import kotlinx.coroutines.launch

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

    var mode by remember { mutableStateOf(AddMode.None) }
    var provider by remember { mutableStateOf<String?>(null) }
    var alias by remember { mutableStateOf("") }
    var keyText by remember { mutableStateOf("") }
    var revealKey by remember { mutableStateOf(false) }
    var localNotice by remember { mutableStateOf<String?>(null) }
    var busy by remember { mutableStateOf(false) }
    val secret = remember { SecretBuffer() }
    val notice = localNotice ?: controllerNotice

    // The buffer never outlives the screen, however the screen ends.
    DisposableEffect(Unit) {
        onDispose { secret.wipe() }
    }
    // Leaving the API-key form for any reason clears the key with it.
    LaunchedEffect(mode) {
        if (mode != AddMode.ApiKey) {
            secret.wipe()
            keyText = ""
            revealKey = false
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
                onClick = { clearSecret(); onBack() },
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
                    AccountCard(
                        account = account,
                        onSetActive = {
                            scope.launch { repository.setActive(account.alias); repository.refresh() }
                        },
                        onRemove = {
                            scope.launch {
                                repository.remove(account.alias)
                                localNotice = "Account removed."
                                repository.refresh()
                            }
                        },
                    )
                }
            }

            Row(horizontalArrangement = Arrangement.spacedBy(ForgeSpace.md)) {
                ForgeButton(
                    text = stringResource(R.string.accounts_add_api_key),
                    onClick = {
                        mode = if (mode == AddMode.ApiKey) AddMode.None else AddMode.ApiKey
                        localNotice = null
                        oauth.clearNotice()
                    },
                    kind = if (mode == AddMode.ApiKey) ForgeButtonKind.Filled else ForgeButtonKind.Ghost,
                )
                ForgeButton(
                    text = stringResource(R.string.accounts_sign_in),
                    onClick = {
                        mode = if (mode == AddMode.OAuth) AddMode.None else AddMode.OAuth
                        localNotice = null
                        oauth.clearNotice()
                    },
                    kind = if (mode == AddMode.OAuth) ForgeButtonKind.Filled else ForgeButtonKind.Ghost,
                )
            }

            when (mode) {
                AddMode.None -> Unit

                AddMode.ApiKey -> Card {
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
                        stringResource(R.string.accounts_key_never_leaves),
                        style = type.sessionMeta,
                        color = colors.textMuted,
                    )
                    Row(horizontalArrangement = Arrangement.spacedBy(ForgeSpace.md)) {
                        ForgeButton(
                            text = stringResource(R.string.accounts_validate),
                            onClick = {
                                val chosen = provider ?: return@ForgeButton
                                busy = true
                                scope.launch {
                                    try {
                                        // Validation reads the buffer and hands
                                        // back nothing: no copy escapes `use`.
                                        val result = secret.use { repository.validateApiKey(chosen, it) }
                                        localNotice = when (result) {
                                            AccountResult.Ok -> "Key validated."
                                            is AccountResult.Failed -> result.publicCode
                                        }
                                    } finally {
                                        busy = false
                                    }
                                }
                            },
                            kind = ForgeButtonKind.Ghost,
                            enabled = provider != null && keyText.isNotBlank() && !busy,
                        )
                        ForgeButton(
                            text = stringResource(R.string.action_save),
                            onClick = {
                                val chosen = provider ?: return@ForgeButton
                                busy = true
                                scope.launch {
                                    var result: AccountResult? = null
                                    try {
                                        result = secret.use {
                                            repository.addApiKey(chosen, alias.takeIf(String::isNotBlank), it)
                                        }
                                    } finally {
                                        // Whatever happened — success, refusal,
                                        // cancellation — the key is gone.
                                        clearSecret()
                                        busy = false
                                    }
                                    when (result) {
                                        AccountResult.Ok -> {
                                            localNotice = "API key added and validated."
                                            mode = AddMode.None
                                            alias = ""
                                            repository.refresh()
                                        }
                                        is AccountResult.Failed -> localNotice = result.publicCode
                                        null -> Unit
                                    }
                                }
                            },
                            enabled = provider != null && keyText.isNotBlank() && !busy,
                        )
                        ForgeButton(
                            text = stringResource(R.string.action_cancel),
                            onClick = {
                                clearSecret()
                                mode = AddMode.None
                            },
                            kind = ForgeButtonKind.Ghost,
                            enabled = !busy,
                        )
                    }
                }

                AddMode.OAuth -> Card {
                    val live = attempt
                    if (live == null) {
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
                    } else {
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
                                    onClick = { oauth.cancel() },
                                    kind = ForgeButtonKind.Ghost,
                                )
                            }
                        }
                    }
                }
            }
        }
    }
}

private enum class AddMode { None, ApiKey, OAuth }

@Composable
private fun AccountCard(account: Account, onSetActive: () -> Unit, onRemove: () -> Unit) {
    val colors = Forge.colors
    val type = Forge.type
    Card {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Column(Modifier.weight(1f)) {
                Text(
                    account.label ?: account.identity ?: account.alias,
                    style = type.sessionTitle,
                    color = colors.text,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                Text(
                    listOfNotNull(
                        account.provider,
                        if (account.authKind == AuthKind.OAuth) "sign-in" else "API key",
                        account.identity,
                    ).joinToString(" · "),
                    style = type.numeric,
                    color = colors.textMuted,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
            if (account.active) {
                StatePill(stringResource(R.string.accounts_active), colors.accent)
            }
        }
        Row(horizontalArrangement = Arrangement.spacedBy(ForgeSpace.md)) {
            if (!account.active) {
                ForgeButton(
                    text = stringResource(R.string.accounts_set_active),
                    onClick = onSetActive,
                    kind = ForgeButtonKind.Ghost,
                )
            }
            ForgeButton(
                text = stringResource(R.string.action_delete),
                onClick = onRemove,
                kind = ForgeButtonKind.Destructive,
            )
        }
    }
}

@Composable
private fun ProviderPicker(
    providers: List<ProviderDescriptor>,
    selected: String?,
    onSelect: (String) -> Unit,
) {
    val colors = Forge.colors
    val type = Forge.type
    Column(verticalArrangement = Arrangement.spacedBy(ForgeSpace.md)) {
        Text(stringResource(R.string.accounts_provider), style = type.label, color = colors.textMuted)
        Row(horizontalArrangement = Arrangement.spacedBy(ForgeSpace.md)) {
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

@Composable
private fun LabelledField(
    label: String,
    value: String,
    onValueChange: (String) -> Unit,
    masked: Boolean = false,
    tag: String? = null,
    trailing: (@Composable () -> Unit)? = null,
) {
    val colors = Forge.colors
    val type = Forge.type
    Column(verticalArrangement = Arrangement.spacedBy(ForgeSpace.xs)) {
        Text(label.uppercase(), style = type.label, color = colors.textMuted)
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .heightIn(min = ForgeSize.touch)
                .clip(ForgeShapes.cardTight)
                .background(colors.surfaceControl)
                .border(ForgeSize.hairline, colors.border, ForgeShapes.cardTight)
                .padding(start = ForgeSpace.lg),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            BasicTextField(
                value = value,
                onValueChange = onValueChange,
                singleLine = true,
                textStyle = type.chatBody.copy(color = colors.text),
                cursorBrush = SolidColor(colors.accent),
                visualTransformation = if (masked) {
                    PasswordVisualTransformation()
                } else {
                    VisualTransformation.None
                },
                keyboardOptions = KeyboardOptions(
                    keyboardType = if (masked) KeyboardType.Password else KeyboardType.Text,
                ),
                modifier = Modifier
                    .weight(1f)
                    .padding(vertical = ForgeSpace.md)
                    .heightIn(min = ForgeSize.touch)
                    .let { if (tag != null) it.testTag(tag) else it },
            )
            trailing?.let {
                Box(Modifier.padding(end = ForgeSpace.xs)) { it() }
            }
        }
    }
}
