package ai.diffforge.haider.ui.settings

import ai.diffforge.haider.R
import ai.diffforge.haider.accounts.Account
import ai.diffforge.haider.accounts.AccountResult
import ai.diffforge.haider.accounts.AccountsRepository
import ai.diffforge.haider.accounts.AuthKind
import ai.diffforge.haider.accounts.OAuthFlow
import ai.diffforge.haider.accounts.OAuthStatus
import ai.diffforge.haider.accounts.ProviderDescriptor
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
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch

const val ACCOUNTS_KEY_FIELD_TAG = "accounts_key_field"
const val ACCOUNTS_OAUTH_WAITING_TAG = "accounts_oauth_waiting"

/**
 * Settings -> Accounts on the phone: add an API key (provider pick, masked entry,
 * validate, save, remove) or sign in through a provider's OAuth flow.
 *
 * The key lives in a local `CharArray`, is passed straight to the repository and
 * is wiped and cleared the moment the call returns. It is never stored on an
 * [Account], never rendered back, and never written into a log line — the
 * account list shows the daemon's masked hint instead.
 */
@Composable
fun AccountsScreen(
    repository: AccountsRepository,
    onBack: () -> Unit,
    onOpenUrl: (String) -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = Forge.colors
    val type = Forge.type
    val scope = rememberCoroutineScope()
    val providers by repository.providers.collectAsState()
    val snapshot by repository.snapshot.collectAsState()

    var mode by remember { mutableStateOf(AddMode.None) }
    var provider by remember { mutableStateOf<String?>(null) }
    var alias by remember { mutableStateOf("") }
    var key by remember { mutableStateOf("") }
    var revealKey by remember { mutableStateOf(false) }
    var notice by remember { mutableStateOf<String?>(null) }
    var busy by remember { mutableStateOf(false) }
    var flow by remember { mutableStateOf<OAuthFlow.Started?>(null) }

    LaunchedEffect(Unit) { repository.refresh() }

    // Poll a live flow until it is claimable, exactly as the desktop does.
    LaunchedEffect(flow?.flowId) {
        val live = flow ?: return@LaunchedEffect
        while (true) {
            delay(OAUTH_POLL_MS)
            when (val status = repository.pollOAuth(live)) {
                OAuthStatus.Waiting -> Unit
                is OAuthStatus.Ready -> {
                    val result = repository.completeOAuth(live, status.oauthReference)
                    notice = when (result) {
                        AccountResult.Ok -> "Signed in: ${status.identity ?: live.alias}"
                        is AccountResult.Failed -> result.publicCode
                    }
                    flow = null
                    mode = AddMode.None
                    repository.refresh()
                    return@LaunchedEffect
                }
                is OAuthStatus.Failed -> {
                    notice = status.publicCode ?: status.terminalKind
                    flow = null
                    return@LaunchedEffect
                }
            }
        }
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
            ForgeIconButton(onClick = onBack, contentDescription = stringResource(R.string.action_back)) {
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
            notice?.let {
                Text(it, style = type.sessionMeta, color = colors.accent)
            }

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
                                notice = "Account removed."
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
                        notice = null
                    },
                    kind = if (mode == AddMode.ApiKey) ForgeButtonKind.Filled else ForgeButtonKind.Ghost,
                )
                ForgeButton(
                    text = stringResource(R.string.accounts_sign_in),
                    onClick = {
                        mode = if (mode == AddMode.OAuth) AddMode.None else AddMode.OAuth
                        notice = null
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
                        value = key,
                        onValueChange = { key = it },
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
                                    val secret = key.toCharArray()
                                    val result = repository.validateApiKey(chosen, secret)
                                    secret.fill(' ')
                                    notice = when (result) {
                                        AccountResult.Ok -> "Key validated."
                                        is AccountResult.Failed -> result.publicCode
                                    }
                                    busy = false
                                }
                            },
                            kind = ForgeButtonKind.Ghost,
                            enabled = provider != null && key.isNotBlank() && !busy,
                        )
                        ForgeButton(
                            text = stringResource(R.string.action_save),
                            onClick = {
                                val chosen = provider ?: return@ForgeButton
                                busy = true
                                scope.launch {
                                    val secret = key.toCharArray()
                                    val result = repository.addApiKey(
                                        chosen,
                                        alias.takeIf { it.isNotBlank() },
                                        secret,
                                    )
                                    // Wipe the copy, then clear the field: the
                                    // secret never outlives the call.
                                    secret.fill(' ')
                                    key = ""
                                    busy = false
                                    when (result) {
                                        AccountResult.Ok -> {
                                            notice = "API key added and validated."
                                            mode = AddMode.None
                                            alias = ""
                                            repository.refresh()
                                        }
                                        is AccountResult.Failed -> notice = result.publicCode
                                    }
                                }
                            },
                            enabled = provider != null && key.isNotBlank() && !busy,
                        )
                    }
                }

                AddMode.OAuth -> Card {
                    val live = flow
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
                                    when (val started = repository.startOAuth(chosen, alias.takeIf { it.isNotBlank() })) {
                                        is OAuthFlow.Started -> {
                                            flow = started
                                            notice = null
                                            started.authorizationUrl?.let(onOpenUrl)
                                        }
                                        is OAuthFlow.Unavailable -> notice = started.reason
                                            ?: "Sign-in is unavailable for $chosen."
                                    }
                                    busy = false
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
                                stringResource(R.string.accounts_oauth_waiting),
                                style = type.chatBody,
                                color = colors.text,
                            )
                            live.userCode?.let {
                                Text(
                                    stringResource(R.string.accounts_oauth_code, it),
                                    style = type.numeric,
                                    color = colors.accent,
                                )
                            }
                            Row(horizontalArrangement = Arrangement.spacedBy(ForgeSpace.md)) {
                                live.authorizationUrl?.let { url ->
                                    ForgeButton(
                                        text = stringResource(R.string.accounts_oauth_open_again),
                                        onClick = { onOpenUrl(url) },
                                        kind = ForgeButtonKind.Ghost,
                                    )
                                }
                                ForgeButton(
                                    text = stringResource(R.string.action_cancel),
                                    onClick = {
                                        val cancelling = live
                                        flow = null
                                        scope.launch { repository.cancelOAuth(cancelling) }
                                    },
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
                    account.label ?: account.alias,
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
                    .let { if (tag != null) it.testTag(tag) else it },
            )
            trailing?.let {
                Box(Modifier.padding(end = ForgeSpace.xs)) { it() }
            }
        }
    }
}

private const val OAUTH_POLL_MS = 1_500L
