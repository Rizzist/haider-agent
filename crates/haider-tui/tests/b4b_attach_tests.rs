//! B4b — attachments in the live TUI (research §seam-plan item 6).
//!
//! `/attach` chips the draft with a magic-sniffed image and uploads it
//! through the receipt-free `artifact.put`; the big-paste pill is REAL
//! (a `PastedText` artifact, not zeroize-and-drop theater); the chips ride
//! both `turn.submit` wire forms at their captured coordinates; and every
//! refusal — demo, feature-ungated daemon, oversized, non-image — is an
//! honest notice with NO upload behind it.
#![allow(clippy::expect_used)]

use base64::Engine as _;
use haider_protocol::EventPayload;
use haider_protocol::envelope::{EventEnvelope, PromptRender, RawEnvelope, RenderTargets};
use haider_protocol::ids::{ArtifactRef, BranchId, DeviceId, EventId, NodeId, SessionId};
use haider_protocol::tool::AttachmentBlock;
use haider_rpc::{RequestBody, ResponseBody};
use haider_tui::app::{AppEvent, AppModel, AppRequest, Pasted, RuntimeMode, Screen};
use haider_tui::link::{CommandContext, map_response, request_body};
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply};
use haider_tui::projection::RawOutcome;
use haider_tui::runtime::{ShellRequest, attach_read_effects, live_pass};
use ratatui::crossterm::event::KeyCode;

mod common;
use common::{key, launcher_model, run_slash, submit};

fn sid() -> SessionId {
    SessionId::new("s-0")
}

fn bid(name: &str) -> BranchId {
    BranchId::new(name)
}

/// A live session with `artifact_put_v1` advertised.
fn live_model() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.daemon_features = [haider_rpc::FEATURE_ARTIFACT_PUT_V1.to_owned()]
        .into_iter()
        .collect();
    model.daemon_version = Some("0.0.50".to_owned());
    model.sessions.clear();
    model.upsert_live_session(&sid());
    model.open_session(&sid());
    model
}

fn raw(seq: u64, payload: &EventPayload) -> RawEnvelope {
    EventEnvelope {
        schema_version: 1,
        event_id: EventId::new(format!("evt-{seq}")),
        seq,
        session_id: sid(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("b4b-device"),
        authority_epoch: 1,
        worker_generation: 7,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::to_value(payload).expect("payload serializes"),
    }
}

/// A unique temp file holding `bytes`, cleaned up by the caller.
fn temp_file(name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "haider-b4b-{}-{}-{name}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::write(&path, bytes).expect("temp file writes");
    path
}

const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

fn png_bytes() -> Vec<u8> {
    let mut bytes = PNG_MAGIC.to_vec();
    bytes.extend_from_slice(b"haider-b4b-test-image");
    bytes
}

/// Drain the reducer's requests through the driver — the `live_pass`
/// routing without its selection/answer tails.
fn drain(driver: &mut LiveDriver, model: &mut AppModel) -> Vec<LiveCommand> {
    let requests: Vec<AppRequest> = model.requests.drain(..).collect();
    let mut commands = Vec::new();
    for request in requests {
        commands.extend(driver.handle_request(model, request));
    }
    commands
}

// ---- law 1: /attach uploads, then the submit carries the refs ---------

#[test]
fn attach_uploads_then_submit_carries_refs() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    let bytes = png_bytes();
    let path = temp_file("ok.png", &bytes);

    // /attach routes ONE shell-owned read request — the reducer touches
    // no filesystem.
    run_slash(&mut model, &format!("/attach {}", path.display()));
    let pass = live_pass(&mut driver, &mut model, None, std::time::Instant::now());
    assert_eq!(
        pass.shell,
        vec![ShellRequest::AttachRead(path.display().to_string())],
        "the read is shell-owned"
    );
    assert!(
        !pass
            .commands
            .iter()
            .any(|command| matches!(command, LiveCommand::ArtifactPut { .. })),
        "nothing uploads before the shell has read the file"
    );

    // The shell read chips the draft (uploading) and issues the upload.
    attach_read_effects(&mut model, &path.display().to_string());
    let chips = model.composer.attachments();
    assert_eq!(chips.len(), 1);
    assert!(chips[0].artifact.is_none(), "the chip is still uploading");
    assert!(
        chips[0].label.contains("ok.png"),
        "label: {}",
        chips[0].label
    );
    let commands = drain(&mut driver, &mut model);
    let put = commands
        .iter()
        .find(|command| matches!(command, LiveCommand::ArtifactPut { .. }))
        .expect("the upload command")
        .clone();
    let LiveCommand::ArtifactPut {
        upload,
        surface,
        bytes: sent,
    } = put.clone()
    else {
        unreachable!()
    };
    assert_eq!(
        sent.as_slice(),
        bytes.as_slice(),
        "the exact file bytes travel"
    );
    assert_eq!(surface, model.surface_key(), "captured at issuance");

    // EXACT WIRE: RFC 4648 STANDARD base64 of the file bytes, as the
    // `artifact.put` method body.
    let body = request_body(put.clone());
    assert_eq!(
        body,
        RequestBody::ArtifactPut {
            data_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
        }
    );
    let encoded = serde_json::to_value(&body).expect("encodes");
    assert_eq!(
        encoded.get("method").and_then(|v| v.as_str()),
        Some("artifact.put")
    );

    // A submit while the upload is in flight is an honest refusal — the
    // draft (text AND chip) survives it.
    submit(&mut model, "what is in this image?");
    assert_eq!(
        model.flash.as_deref(),
        Some("· attachment still uploading — a moment")
    );
    assert!(
        model.requests.is_empty(),
        "no submit request rode out mid-upload"
    );
    assert_eq!(model.composer, "what is in this image?");
    assert_eq!(model.composer.attachments().len(), 1);

    // The daemon's verified content address completes the chip through
    // the link's context (the wire is receipt-free).
    let artifact = ArtifactRef::new(format!("blake3:{:0>64}", "b4b"));
    let context = CommandContext::of(&put);
    let replies = map_response(
        &context,
        ResponseBody::ArtifactPut {
            artifact: artifact.clone(),
            bytes: bytes.len() as u64,
        },
    );
    assert_eq!(
        replies,
        vec![LiveReply::ArtifactUploaded {
            upload,
            surface,
            artifact: artifact.clone(),
        }]
    );
    for reply in replies {
        driver.apply(&mut model, reply);
    }
    assert!(
        !model.composer.has_uploading_attachment(),
        "the chip is ready"
    );

    // Plain ⏎ now sends the SAME surviving draft, chip riding.
    model.handle(key(KeyCode::Enter));
    let commands = drain(&mut driver, &mut model);
    let submit_command = commands
        .iter()
        .find(|command| matches!(command, LiveCommand::Submit { .. }))
        .expect("the submit")
        .clone();
    let LiveCommand::Submit {
        text, attachments, ..
    } = submit_command.clone()
    else {
        unreachable!()
    };
    assert_eq!(text, "what is in this image?");
    assert_eq!(
        attachments,
        vec![AttachmentBlock::Image {
            artifact: artifact.clone(),
            mime: "image/png".to_owned(),
            width: None,
            height: None,
        }]
    );
    // EXACT WIRE, legacy main-branch form: the block rides `turn.submit`.
    let encoded = serde_json::to_value(request_body(submit_command)).expect("encodes");
    assert_eq!(
        encoded.get("method").and_then(|v| v.as_str()),
        Some("turn.submit")
    );
    assert_eq!(
        encoded.get("attachments").expect("attachments ride"),
        &serde_json::json!([{
            "kind": "image",
            "artifact": artifact.as_str(),
            "mime": "image/png",
        }])
    );

    // The committed UserMessage envelope — not the submit — paints the
    // row, and the existing count vocabulary fires for the real send.
    assert_eq!(
        model.route_raw(&raw(
            1,
            &EventPayload::UserMessage {
                text: "what is in this image?".to_owned(),
                attachments: vec![AttachmentBlock::Image {
                    artifact,
                    mime: "image/png".to_owned(),
                    width: None,
                    height: None,
                }],
                mode: haider_protocol::DeliveryMode::Steer,
            }
        )),
        RawOutcome::Applied
    );
    let plain = haider_tui::plain::render_plain(&model.projection, 0);
    assert!(
        plain.contains("what is in this image? [+1 attachment(s)]"),
        "the transcript names the real attachment: {plain}"
    );

    let _ = std::fs::remove_file(&path);
}

// ---- law 2: oversized / non-image / over-count are honest, no upload --

#[test]
fn oversized_and_non_image_attach_are_honest_notices_with_no_upload() {
    let mut model = live_model();

    // Not an image (magic sniff, never the extension).
    let text_file = temp_file("notes.png", b"just some text, whatever the name says");
    attach_read_effects(&mut model, &text_file.display().to_string());
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|flash| flash.contains("not a JPEG, PNG, GIF, or WebP image")),
        "flash: {:?}",
        model.flash
    );
    assert!(model.composer.attachments().is_empty(), "no chip");
    assert!(model.requests.is_empty(), "no upload request");

    // Over the 5 MiB per-attachment cap (the client mirror of B4a's
    // acceptance validation).
    let mut big = PNG_MAGIC.to_vec();
    big.resize(5 * 1024 * 1024 + 1, 0xAB);
    let big_file = temp_file("big.png", &big);
    attach_read_effects(&mut model, &big_file.display().to_string());
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|flash| flash.contains("exceeds the 5 MiB per-attachment limit")),
        "flash: {:?}",
        model.flash
    );
    assert!(model.composer.attachments().is_empty(), "no chip");
    assert!(model.requests.is_empty(), "no upload request");

    // A sixth chip is refused at the reducer (the ≤5-a-turn mirror) —
    // no read is even issued.
    for lines in 0..5_u32 {
        model
            .composer
            .push_attachment(haider_tui::composer::PendingAttachment {
                upload: u64::from(lines) + 100,
                label: format!("[Pasted {} lines]", lines + 4),
                kind: haider_tui::composer::PendingKind::PastedText { lines: lines + 4 },
                artifact: Some(ArtifactRef::new(format!("blake3:{lines:0>64}"))),
            });
    }
    run_slash(&mut model, "/attach sixth.png");
    assert_eq!(
        model.flash.as_deref(),
        Some("· 5 attachments a turn — ⌫ at the start removes one")
    );
    assert!(model.requests.is_empty(), "no read request for the sixth");

    let _ = std::fs::remove_file(&text_file);
    let _ = std::fs::remove_file(&big_file);
}

// ---- law 3: the paste pill is REAL ------------------------------------

#[test]
fn paste_pill_uploads_pasted_text_and_rides_submit() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");

    model.handle(AppEvent::Paste(Pasted::new(
        "line one\r\nline two\nline three\nline four\nline five".to_owned(),
    )));
    // The REAL pill is a chip, never a text token the submit would carry
    // as fake prose.
    assert_eq!(model.composer, "");
    let chips = model.composer.attachments();
    assert_eq!(chips.len(), 1);
    assert_eq!(chips[0].label, "[Pasted 5 lines]");
    assert!(chips[0].artifact.is_none(), "uploading");

    // The upload carries the NORMALIZED UTF-8 paste bytes.
    let commands = drain(&mut driver, &mut model);
    let put = commands
        .iter()
        .find(|command| matches!(command, LiveCommand::ArtifactPut { .. }))
        .expect("the paste upload")
        .clone();
    let LiveCommand::ArtifactPut { bytes, .. } = put.clone() else {
        unreachable!()
    };
    let normalized = "line one\nline two\nline three\nline four\nline five";
    assert_eq!(bytes.as_slice(), normalized.as_bytes());
    assert_eq!(
        request_body(put.clone()),
        RequestBody::ArtifactPut {
            data_base64: base64::engine::general_purpose::STANDARD.encode(normalized.as_bytes()),
        }
    );

    // Complete the upload through the link context, then submit.
    let artifact = ArtifactRef::new(format!("blake3:{:0>64}", "paste"));
    for reply in map_response(
        &CommandContext::of(&put),
        ResponseBody::ArtifactPut {
            artifact: artifact.clone(),
            bytes: normalized.len() as u64,
        },
    ) {
        driver.apply(&mut model, reply);
    }
    submit(&mut model, "summarize the paste");
    let commands = drain(&mut driver, &mut model);
    let LiveCommand::Submit {
        text, attachments, ..
    } = commands
        .iter()
        .find(|command| matches!(command, LiveCommand::Submit { .. }))
        .expect("the submit")
        .clone()
    else {
        unreachable!()
    };
    assert_eq!(text, "summarize the paste");
    assert_eq!(
        attachments,
        vec![AttachmentBlock::PastedText {
            artifact: artifact.clone(),
            lines: 5,
        }]
    );
    assert!(
        !model.composer.has_attachments(),
        "the pill cleared on submit"
    );

    // Wire shape of the pasted-text block.
    let encoded = serde_json::to_value(
        serde_json::to_value(&AttachmentBlock::PastedText { artifact, lines: 5 })
            .expect("block encodes"),
    )
    .expect("value");
    assert_eq!(
        encoded.get("kind").and_then(|v| v.as_str()),
        Some("pasted_text")
    );
}

// ---- law 4: clear on submit; survive branch switches; ⌫ removes -------

#[test]
fn pending_attachments_clear_on_submit_and_survive_branch_switch_capture() {
    let mut model = live_model();
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_BRANCH_CREATE_V1.to_owned());
    let mut driver = LiveDriver::new("test");

    // Daemon truth installs the branch.
    assert_eq!(
        model.route_raw(&raw(
            1,
            &EventPayload::NodeCommitted(haider_protocol::history::TreeNode {
                node: NodeId::new("node-1"),
                parent: None,
                kind: haider_protocol::history::NodeKind::UserTurn {
                    text: "committed".to_owned(),
                    attachments: vec![],
                },
            })
        )),
        RawOutcome::Applied
    );
    let mut branch_envelope = raw(2, &EventPayload::IdleDecayed);
    branch_envelope.payload = haider_protocol::branch::BranchCreated {
        branch: haider_protocol::branch::BranchDescriptor {
            branch_id: bid("b-exp"),
            name: "experiment".to_owned(),
            source_branch_id: None,
            fork_node_id: NodeId::new("node-1"),
            fork_seq: 1,
            created_seq: 2,
            created_at_ms: 0,
            head_node_id: NodeId::new("node-1"),
            head_seq: 1,
        },
    }
    .to_payload_value()
    .expect("branch fact serializes");
    assert_eq!(model.route_raw(&branch_envelope), RawOutcome::Applied);

    // A ready chip on main…
    let artifact = ArtifactRef::new(format!("blake3:{:0>64}", "keep"));
    model.begin_attachment_upload(
        vec![1, 2, 3],
        haider_tui::composer::PendingKind::Image {
            mime: "image/png".to_owned(),
        },
        "photo.png · 1 KB".to_owned(),
    );
    model.requests.clear();
    assert!(model.complete_upload(model.surface_key(), 1, artifact.clone()));

    // …SURVIVES the branch switch: the draft is session-scoped, and a
    // `/branch` switch never swaps it.
    assert!(model.switch_branch(Some(&bid("b-exp"))).is_some());
    assert_eq!(model.composer.attachments().len(), 1);

    // ⌫ at the very start removes the NEWEST chip, nothing else.
    model.begin_attachment_upload(
        vec![9],
        haider_tui::composer::PendingKind::PastedText { lines: 9 },
        "[Pasted 9 lines]".to_owned(),
    );
    model.requests.clear();
    model.handle(key(KeyCode::Backspace));
    let labels: Vec<&str> = model
        .composer
        .attachments()
        .iter()
        .map(|chip| chip.label.as_str())
        .collect();
    assert_eq!(labels, vec!["photo.png · 1 KB"], "newest chip removed");

    // The submit issued ON the fork captures branch AND blocks; the take
    // clears the draft's chips.
    submit(&mut model, "carry it");
    let request = model
        .requests
        .iter()
        .find(|request| matches!(request, AppRequest::SubmitText { .. }))
        .expect("the submit request")
        .clone();
    let AppRequest::SubmitText {
        branch,
        attachments,
        ..
    } = request.clone()
    else {
        unreachable!()
    };
    assert_eq!(branch.as_ref().map(BranchId::as_str), Some("b-exp"));
    assert_eq!(
        attachments,
        vec![AttachmentBlock::Image {
            artifact: artifact.clone(),
            mime: "image/png".to_owned(),
            width: None,
            height: None,
        }]
    );
    assert!(
        !model.composer.has_attachments(),
        "pending attachments clear on submit"
    );

    // CAPTURE INTACT: a later switch back to main retargets NOTHING —
    // the queued request drains onto the branch-capable wire form with
    // its blocks riding.
    model.requests.clear();
    assert!(model.switch_branch(None).is_some());
    let commands: Vec<LiveCommand> = driver.handle_request(&mut model, request);
    let submit_command = commands
        .iter()
        .find(|command| matches!(command, LiveCommand::Submit { .. }))
        .expect("the drained submit")
        .clone();
    let LiveCommand::Submit {
        branch: sent_branch,
        attachments: sent_blocks,
        ..
    } = submit_command.clone()
    else {
        unreachable!()
    };
    assert_eq!(sent_branch.as_ref().map(BranchId::as_str), Some("b-exp"));
    assert_eq!(sent_blocks.len(), 1);
    let encoded = serde_json::to_value(request_body(submit_command)).expect("encodes");
    assert!(
        matches!(
            request_body(LiveCommand::Submit {
                command_id: haider_rpc::CommandId::new("cmd-pin"),
                session: sid(),
                worker_generation: 0,
                text: "carry it".to_owned(),
                mode: haider_protocol::DeliveryMode::Steer,
                branch: Some(bid("b-exp")),
                attachments: vec![AttachmentBlock::Image {
                    artifact: artifact.clone(),
                    mime: "image/png".to_owned(),
                    width: None,
                    height: None,
                }],
            }),
            RequestBody::TurnSubmitWithBranch { .. }
        ),
        "a captured branch rides the branch-capable decode form"
    );
    assert_eq!(
        encoded.get("branch_id").and_then(|v| v.as_str()),
        Some("b-exp")
    );
    assert_eq!(
        encoded
            .get("attachments")
            .expect("blocks ride the branch form"),
        &serde_json::json!([{
            "kind": "image",
            "artifact": artifact.as_str(),
            "mime": "image/png",
        }])
    );
}

// ---- law 5: the ungated daemon fabricates nothing ---------------------

#[test]
fn feature_ungated_attach_is_honest() {
    let mut model = live_model();
    model.daemon_features.clear();

    run_slash(&mut model, "/attach photo.png");
    assert_eq!(
        model.flash.as_deref(),
        Some(
            "· attachments needs a newer daemon (running v0.0.50) — restart it to pick up this release"
        )
    );
    assert!(model.requests.is_empty(), "no read, no upload");
    assert!(model.composer.attachments().is_empty(), "no chip");

    // The big paste cannot fabricate a pill either: the text lands
    // LITERALLY (content preserved, nothing claimed) and the notice
    // names the stale daemon.
    model.handle(AppEvent::Paste(Pasted::new("a\nb\nc\nd\ne".to_owned())));
    assert!(model.composer.attachments().is_empty(), "no chip");
    assert_eq!(model.composer, "a\nb\nc\nd\ne");
    assert_eq!(
        model.flash.as_deref(),
        Some(
            "· paste attachments needs a newer daemon (running v0.0.50) — restart it to pick up this release"
        )
    );
    assert!(model.requests.is_empty(), "no upload request");
}

// ---- law 6: demo refuses honestly -------------------------------------

#[test]
fn demo_attach_refuses() {
    let mut model = launcher_model();
    submit(&mut model, "start a session");
    model.requests.clear();
    assert_eq!(model.screen, Screen::Session);

    run_slash(&mut model, "/attach photo.png");
    assert_eq!(
        model.flash.as_deref(),
        Some("· /attach — live only; attachments ride the daemon's store")
    );
    assert!(model.requests.is_empty(), "nothing rides");
    assert!(model.composer.attachments().is_empty(), "no chip");

    // The demo paste pill keeps its sim-verbatim vocabulary — a local
    // token in a world that is local by design — and still chips nothing.
    model.handle(AppEvent::Paste(Pasted::new("a\nb\nc\nd\ne".to_owned())));
    assert_eq!(model.composer, "[Pasted 5 lines] ");
    assert!(model.composer.attachments().is_empty(), "no chip");
    assert!(model.requests.is_empty(), "no upload request");
}
