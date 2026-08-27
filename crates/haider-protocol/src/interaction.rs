//! Typed policy for gates that would otherwise wait for a human.

use crate::session::SessionInteractionModeV1;

/// A gate whose behavior depends on whether a human can answer this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractionGate {
    RequestInputWithDefault,
    RequestInputWithoutDefault,
    PartialProviderStream,
    WorkflowUnfinishedFirst,
    WorkflowUnfinishedRecurrence,
    EffectBrokerAsk,
    OsOrDesktopPermission,
    CredentialOrLogin,
    MobileOrDeviceGrant,
    GraphHumanConfirm,
    UnknownEffectAfterCrash,
    DestructiveOrClobber,
    CacheEpochOrConfiguration,
}

/// The complete decision vocabulary for [`InteractionResolutionPolicy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractionResolution {
    AwaitHuman,
    UseDeclaredDefault,
    ReturnNoHumanAvailable,
    ContinuePartial,
    ContinueWorkflow,
    ReturnWorkflowUnfinished,
    FailClosed,
}

/// The single deterministic policy derived from durable session metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InteractionResolutionPolicy {
    mode: SessionInteractionModeV1,
}

impl InteractionResolutionPolicy {
    #[must_use]
    pub const fn new(mode: SessionInteractionModeV1) -> Self {
        Self { mode }
    }

    #[must_use]
    pub const fn mode(self) -> SessionInteractionModeV1 {
        self.mode
    }

    #[must_use]
    pub const fn resolve(self, gate: InteractionGate) -> InteractionResolution {
        use InteractionGate as Gate;
        use InteractionResolution as Resolution;

        match (self.mode, gate) {
            (_, Gate::WorkflowUnfinishedFirst) => Resolution::ContinueWorkflow,
            (SessionInteractionModeV1::Interactive, Gate::RequestInputWithDefault)
            | (SessionInteractionModeV1::Interactive, Gate::RequestInputWithoutDefault)
            | (SessionInteractionModeV1::Interactive, Gate::PartialProviderStream)
            | (SessionInteractionModeV1::Interactive, Gate::WorkflowUnfinishedRecurrence)
            | (SessionInteractionModeV1::Interactive, Gate::EffectBrokerAsk)
            | (SessionInteractionModeV1::Interactive, Gate::OsOrDesktopPermission)
            | (SessionInteractionModeV1::Interactive, Gate::CredentialOrLogin)
            | (SessionInteractionModeV1::Interactive, Gate::MobileOrDeviceGrant)
            | (SessionInteractionModeV1::Interactive, Gate::GraphHumanConfirm)
            | (SessionInteractionModeV1::Interactive, Gate::UnknownEffectAfterCrash)
            | (SessionInteractionModeV1::Interactive, Gate::DestructiveOrClobber)
            | (SessionInteractionModeV1::Interactive, Gate::CacheEpochOrConfiguration) => {
                Resolution::AwaitHuman
            }
            (SessionInteractionModeV1::Autonomous, Gate::RequestInputWithDefault) => {
                Resolution::UseDeclaredDefault
            }
            (SessionInteractionModeV1::Autonomous, Gate::RequestInputWithoutDefault) => {
                Resolution::ReturnNoHumanAvailable
            }
            (SessionInteractionModeV1::Autonomous, Gate::PartialProviderStream) => {
                Resolution::ContinuePartial
            }
            (SessionInteractionModeV1::Autonomous, Gate::WorkflowUnfinishedRecurrence) => {
                Resolution::ReturnWorkflowUnfinished
            }
            (SessionInteractionModeV1::Autonomous, Gate::EffectBrokerAsk)
            | (SessionInteractionModeV1::Autonomous, Gate::OsOrDesktopPermission)
            | (SessionInteractionModeV1::Autonomous, Gate::CredentialOrLogin)
            | (SessionInteractionModeV1::Autonomous, Gate::MobileOrDeviceGrant)
            | (SessionInteractionModeV1::Autonomous, Gate::GraphHumanConfirm)
            | (SessionInteractionModeV1::Autonomous, Gate::UnknownEffectAfterCrash)
            | (SessionInteractionModeV1::Autonomous, Gate::DestructiveOrClobber)
            | (SessionInteractionModeV1::Autonomous, Gate::CacheEpochOrConfiguration) => {
                Resolution::FailClosed
            }
        }
    }
}

impl Default for InteractionResolutionPolicy {
    fn default() -> Self {
        Self::new(SessionInteractionModeV1::Interactive)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn autonomous_policy_resolves_only_the_four_audited_gates() {
        let policy = InteractionResolutionPolicy::new(SessionInteractionModeV1::Autonomous);
        assert_eq!(
            policy.resolve(InteractionGate::RequestInputWithDefault),
            InteractionResolution::UseDeclaredDefault
        );
        assert_eq!(
            policy.resolve(InteractionGate::RequestInputWithoutDefault),
            InteractionResolution::ReturnNoHumanAvailable
        );
        assert_eq!(
            policy.resolve(InteractionGate::PartialProviderStream),
            InteractionResolution::ContinuePartial
        );
        assert_eq!(
            policy.resolve(InteractionGate::WorkflowUnfinishedFirst),
            InteractionResolution::ContinueWorkflow
        );
        assert_eq!(
            policy.resolve(InteractionGate::WorkflowUnfinishedRecurrence),
            InteractionResolution::ReturnWorkflowUnfinished
        );
    }

    #[test]
    fn autonomous_policy_keeps_sensitive_gates_fail_closed() {
        let policy = InteractionResolutionPolicy::new(SessionInteractionModeV1::Autonomous);
        for gate in [
            InteractionGate::EffectBrokerAsk,
            InteractionGate::OsOrDesktopPermission,
            InteractionGate::CredentialOrLogin,
            InteractionGate::MobileOrDeviceGrant,
            InteractionGate::GraphHumanConfirm,
            InteractionGate::UnknownEffectAfterCrash,
            InteractionGate::DestructiveOrClobber,
            InteractionGate::CacheEpochOrConfiguration,
        ] {
            assert_eq!(policy.resolve(gate), InteractionResolution::FailClosed);
        }
    }

    #[test]
    fn interactive_policy_preserves_existing_waits() {
        let policy = InteractionResolutionPolicy::default();
        for gate in [
            InteractionGate::RequestInputWithDefault,
            InteractionGate::RequestInputWithoutDefault,
            InteractionGate::PartialProviderStream,
            InteractionGate::WorkflowUnfinishedRecurrence,
        ] {
            assert_eq!(policy.resolve(gate), InteractionResolution::AwaitHuman);
        }
    }
}
