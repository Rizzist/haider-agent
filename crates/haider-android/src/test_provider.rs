//! Explicit disposable-device fixture. Requires Android FLAG_DEBUGGABLE AND an
//! empty owner-only marker. Release APKs cannot enable it, even with the marker.
use haider_daemon::{
    DaemonDependencies, ProviderFactory, ProviderFactoryConfig, ResolvedTurnProvider,
};
use haider_protocol::{error::HaiderError, session::SessionMetadataV1};
use haider_provider::{FakeInputKind, FakeInputOption, FakeProvider, FakeStep};
use std::sync::Arc;

pub fn install(dependencies: &mut DaemonDependencies) {
    dependencies.provider_factory = ProviderFactoryConfig::Injected {
        factory: Arc::new(FixtureFactory),
        providers: haider_provider::BUILTIN_PROVIDER_NAMES
            .into_iter()
            .chain(["fake"])
            .map(str::to_owned)
            .collect(),
    };
}

struct FixtureFactory;

#[async_trait::async_trait]
impl ProviderFactory for FixtureFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, HaiderError> {
        let mut steps = Vec::new();
        if metadata.model == "fake-needs-input" {
            steps.extend([
                FakeStep::EmitRequestInput {
                    call_id: "integration-input".into(),
                    kind: FakeInputKind::Choice,
                    title: "Continue the integration check?".into(),
                    body: vec!["This is a synthetic device verification question.".into()],
                    options: vec![FakeInputOption {
                        key: "continue".into(),
                        label: "Continue".into(),
                        detail: None,
                    }],
                },
                FakeStep::Finish {
                    reason: haider_protocol::provider::FinishReason::ToolUse,
                },
            ]);
        }
        steps.extend([
            FakeStep::EmitText { text: "Haider integration fixture: the embedded daemon received your message over h.sock.".into() },
            FakeStep::Finish { reason: haider_protocol::provider::FinishReason::EndTurn },
        ]);
        let fake = FakeProvider::new(steps)
            .without_request_recording()
            .with_route_status(Arc::new(std::sync::Mutex::new(
                haider_platform::RouteStatus::Available,
            )));
        Ok(ResolvedTurnProvider {
            provider: Arc::new(fake),
            provider_name: metadata.provider.clone(),
            model: metadata.model.clone(),
            context_window: None,
            account_alias: None,
            active_no_auth: true,
            initial_rotation: None,
            rotation_budget_consumed: false,
            attempt_resolver: None,
            compaction_promotion: None,
        })
    }
}
