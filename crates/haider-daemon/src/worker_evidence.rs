//! Resolve an explicit, ID-free evidence selector from terminal journal facts.
//! The store remains the authority for graph ownership, subject freshness,
//! process exit, mutation provenance, and slot type. This lookup grants none.

use haider_core::StoreHandle;
use haider_protocol::EventPayload;
use haider_protocol::effect::EffectPhase;
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::graph::{ProcessSignalRef, WorkspaceMutationRef};
use haider_protocol::ids::{RunId, SessionId};
use haider_tools::GraphEvidence;

pub(super) async fn resolve(
    store: &dyn StoreHandle,
    session: &SessionId,
    run: &RunId,
    request: &mut GraphEvidence,
) -> Result<(), HaiderError> {
    let Some(selector) = request.evidence_from.as_deref() else {
        return Ok(());
    };
    // Parser rejects mixed selectors/coordinates. Keep the boundary safe for
    // typed callers too; never overwrite a caller's contradictory claim.
    if request.signal.is_some()
        || request.workspace_mutation.is_some()
        || request.subject_digest.is_some()
    {
        return Err(unavailable(
            "evidence_from cannot overwrite explicit provenance",
        ));
    }
    if !matches!(
        selector,
        "latest_process" | "latest_mutation" | "latest_subject"
    ) {
        return Err(unavailable("unknown evidence_from selector"));
    }
    // A fixed durable head freezes selection while other tasks append. Read
    // bounded pages backwards; memory stays at 256 events even in long runs.
    let mut end = store.latest_seq(session).await?;
    while end > 0 {
        let start = end.saturating_sub(256);
        let page = store.read(session, start, 256).await?;
        for envelope in page.into_iter().rev() {
            if envelope.seq > end || envelope.run_id.as_ref() != Some(run) {
                continue;
            }
            let selected = match envelope.payload.decode_event() {
                Ok(EventPayload::ProcessSignalRecorded(signal))
                    if selector != "latest_mutation" && &signal.run_id == run =>
                {
                    request.subject_digest = Some(signal.subject_digest);
                    if selector == "latest_process" {
                        request.signal = Some(ProcessSignalRef {
                            run_id: signal.run_id,
                            call_id: signal.call_id,
                            effect_id: signal.effect_id,
                        });
                    }
                    true
                }
                Ok(EventPayload::Effect(EffectPhase::Outcome {
                    effect,
                    workspace_mutation: Some(mutation),
                    ..
                })) if selector != "latest_process" && effect == mutation.effect_id => {
                    if let Some(subject) = mutation.subject_digest {
                        request.subject_digest = Some(subject);
                        if selector == "latest_mutation" {
                            request.workspace_mutation = Some(WorkspaceMutationRef {
                                run_id: run.clone(),
                                effect_id: effect,
                            });
                        }
                        true
                    } else {
                        false
                    }
                }
                _ => false,
            };
            if selected {
                return Ok(());
            }
        }
        end = start;
    }
    Err(unavailable(format!(
        "no terminal {selector} evidence in this run; execute the relevant tool first"
    )))
}

fn unavailable(message: impl Into<String>) -> HaiderError {
    HaiderError::new(ErrorCode::InvalidArgument, message, false)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use haider_core::MemoryStore;
    use haider_protocol::effect::{EffectOutcome, WorkspaceMutation};
    use haider_protocol::envelope::{
        EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
    };
    use haider_protocol::graph::ProcessSignalRecorded;
    use haider_protocol::ids::{DeviceId, EffectId, EventId};

    fn fact(session: &SessionId, run: &RunId, id: &str, payload: EventPayload) -> RawEnvelope {
        EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new(id),
            seq: 0,
            session_id: session.clone(),
            branch_id: None,
            run_id: Some(run.clone()),
            agent_id: None,
            device_id: DeviceId::new("evidence-test"),
            authority_epoch: 0,
            worker_generation: 1,
            causation_id: None,
            correlation_id: None,
            committed_at_ms: 0,
            render: RenderTargets {
                ui: true,
                durable: true,
                prompt: PromptRender::Omit,
            },
            payload: serde_json::to_value(payload).expect("payload").into(),
        }
    }

    fn signal(run: &RunId, id: &str) -> EventPayload {
        EventPayload::ProcessSignalRecorded(ProcessSignalRecorded {
            run_id: run.clone(),
            call_id: id.into(),
            effect_id: EffectId::new(id),
            command_arg_digest: "blake3:command".into(),
            exit_code: Some(0),
            transcript_digest: "blake3:transcript".into(),
            workspace_revision: None,
            subject_digest: format!("blake3:{id}"),
            artifact: None,
        })
    }

    fn request(selector: &str) -> GraphEvidence {
        GraphEvidence::from_tool_args(serde_json::json!({"node":"VERIFY", "verdict":"green", "detail":"checked", "evidence_from":selector})).expect("request")
    }

    #[tokio::test]
    async fn evidence_lookup_is_latest_same_run_paged_and_replay_deterministic() {
        let store = MemoryStore::new();
        let session = SessionId::new("evidence-session");
        let run = RunId::new("evidence-run");
        let other = RunId::new("other-run");
        let mut events = vec![
            fact(&session, &run, "old", signal(&run, "old")),
            fact(&session, &run, "selected", signal(&run, "selected")),
        ];
        // Force the backward lookup to traverse more than one bounded page.
        events.extend((0..260).map(|i| {
            fact(
                &session,
                &other,
                &format!("other-{i}"),
                signal(&other, "foreign"),
            )
        }));
        store.append(&mut events).await.expect("journal facts");
        let original = serde_json::to_vec(&events).expect("journal bytes");
        let mut live = request("latest_process");
        resolve(&store, &session, &run, &mut live)
            .await
            .expect("live lookup");
        assert_eq!(live.signal.as_ref().expect("signal").call_id, "selected");
        assert_eq!(live.subject_digest.as_deref(), Some("blake3:selected"));
        let mut replayed: Vec<RawEnvelope> =
            serde_json::from_slice(&original).expect("replay facts");
        for event in &mut replayed {
            event.seq = 0;
        }
        let replay = MemoryStore::new();
        replay.append(&mut replayed).await.expect("replay journal");
        let mut replay_request = request("latest_process");
        resolve(&replay, &session, &run, &mut replay_request)
            .await
            .expect("replay lookup");
        assert_eq!(live, replay_request);
        assert_eq!(
            serde_json::to_vec(&events).expect("unchanged journal"),
            original
        );
        assert!(
            resolve(
                &store,
                &session,
                &RunId::new("missing"),
                &mut request("latest_process")
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn mutation_selection_and_model_subject_keep_distinct_authorities() {
        let store = MemoryStore::new();
        let session = SessionId::new("mutation-session");
        let run = RunId::new("mutation-run");
        let effect = EffectId::new("mutation");
        let payload = EventPayload::Effect(EffectPhase::Outcome {
            effect: effect.clone(),
            outcome: EffectOutcome::Ok,
            freshness: None,
            workspace_mutation: Some(WorkspaceMutation {
                effect_id: effect.clone(),
                mutation_digest: "blake3:content".into(),
                workspace_revision: None,
                subject_digest: Some("blake3:subject".into()),
            }),
        });
        store
            .append(&mut [fact(&session, &run, "mutation-fact", payload)])
            .await
            .expect("mutation");
        let mut mutation = request("latest_mutation");
        resolve(&store, &session, &run, &mut mutation)
            .await
            .expect("mutation selection");
        assert_eq!(mutation.workspace_mutation.expect("ref").effect_id, effect);
        let mut subject = request("latest_subject");
        resolve(&store, &session, &run, &mut subject)
            .await
            .expect("subject selection");
        assert_eq!(subject.subject_digest.as_deref(), Some("blake3:subject"));
        assert!(subject.signal.is_none() && subject.workspace_mutation.is_none());
        assert!(
            resolve(&store, &session, &run, &mut subject).await.is_err(),
            "never overwrite explicit claims"
        );
    }
}
