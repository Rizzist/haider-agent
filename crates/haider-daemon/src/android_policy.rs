//! Hard standalone platform policy. Grants and provider trust can only narrow it.
use crate::worker::RegisteredToolRoute;
use haider_protocol::effect::EffectClass;
use haider_rpc::RequestBody;

pub(crate) const fn enabled() -> bool {
    cfg!(feature = "android-standalone")
}

pub(crate) const fn route_allowed(route: RegisteredToolRoute) -> bool {
    !enabled()
        || matches!(
            route,
            RegisteredToolRoute::RequestInput
                | RegisteredToolRoute::ListTools
                | RegisteredToolRoute::Plan
                | RegisteredToolRoute::LoomRegister
                | RegisteredToolRoute::TodoWrite
                | RegisteredToolRoute::GraphEvidence
                | RegisteredToolRoute::FsRead
                | RegisteredToolRoute::FsGlob
                | RegisteredToolRoute::FsSearch
                | RegisteredToolRoute::FsWrite
                | RegisteredToolRoute::FsEdit
                | RegisteredToolRoute::FsPath
                | RegisteredToolRoute::WebFetch
                | RegisteredToolRoute::WebSearch
                | RegisteredToolRoute::Mobile
                | RegisteredToolRoute::Monitor
                | RegisteredToolRoute::ListModels
                | RegisteredToolRoute::SpawnSubagent
                | RegisteredToolRoute::MessageSubagent
        )
}

pub(crate) const HARD_DENIED_EFFECTS: &[EffectClass] = &[
    EffectClass::ProcessExec,
    EffectClass::RemoteExecution,
    EffectClass::GitOp,
    EffectClass::CredentialAccess,
    EffectClass::GuiAct,
    EffectClass::ScreenObserve,
    EffectClass::ScreenControl,
    EffectClass::PeerMessage,
];

pub(crate) fn effect_allowed(class: &EffectClass) -> bool {
    !enabled() || !HARD_DENIED_EFFECTS.contains(class)
}

pub(crate) fn request_denied(body: &RequestBody) -> bool {
    if enabled()
        && matches!(
            body,
            RequestBody::HeadlessRunStart {
                trust_hooks: true,
                ..
            }
        )
    {
        return true;
    }
    enabled()
        && matches!(
            body,
            RequestBody::ShellExec { .. }
                | RequestBody::ShellExecScoped { .. }
                | RequestBody::ShellList
                | RequestBody::ShellClose { .. }
                | RequestBody::SshList { .. }
                | RequestBody::SshAdd { .. }
                | RequestBody::SshUpdate { .. }
                | RequestBody::SshRemove { .. }
                | RequestBody::SshTest { .. }
                | RequestBody::SshShell { .. }
                | RequestBody::SshShellOpen { .. }
                | RequestBody::SshShellInput { .. }
                | RequestBody::SshShellResize { .. }
                | RequestBody::SshShellEof { .. }
                | RequestBody::PeerList { .. }
                | RequestBody::PeerInject { .. }
                | RequestBody::PeerSend { .. }
                | RequestBody::PeerName { .. }
                | RequestBody::PeerNotifyWhenIdle { .. }
                | RequestBody::SessionSetSshScope { .. }
                | RequestBody::ComputerPermissionOpenSettings { .. }
                | RequestBody::TranscriptionSecretGet
                | RequestBody::TranscriptionSecretSet { .. }
                | RequestBody::AccountOAuthImportSources
                | RequestBody::AccountOAuthImport { .. }
                | RequestBody::AccountSourceList
                | RequestBody::AccountSourceAdd { .. }
                | RequestBody::AccountSourceRemove { .. }
                | RequestBody::AccountSourceScan
                | RequestBody::AccountImportDevice { .. }
                | RequestBody::HooksTrust { .. }
                | RequestBody::TurnSubmitWithHookTrust { .. }
                | RequestBody::LoomInstallRetry { .. }
                | RequestBody::LoomInstallCancel { .. }
        )
}

pub(crate) fn denied() -> haider_protocol::error::HaiderError {
    haider_protocol::error::HaiderError::new(
        haider_protocol::error::ErrorCode::PermissionDenied,
        "capability is unavailable in android-standalone",
        false,
    )
}
