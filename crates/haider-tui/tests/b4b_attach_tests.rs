//! B4b — attachments in the live TUI (research §seam-plan item 6).
//!
//! `/attach` chips the draft with a magic-sniffed image and uploads it
//! through the receipt-free `artifact.put`; the chips ride both
//! `turn.submit` wire forms at their captured coordinates; and every
//! refusal — demo, feature-ungated daemon, oversized, non-image — is an
//! honest notice with NO upload behind it. The big paste left this
//! pipeline with the QoL wave: it is a LOCAL draft pill
//! (`qol_paste_pill_tests.rs`) whose content rides the submit as text.
#![allow(clippy::expect_used)]

use base64::Engine as _;
use haider_protocol::EventPayload;
use haider_protocol::envelope::{EventEnvelope, PromptRender, RawEnvelope, RenderTargets};
use haider_protocol::error::{ErrorAction, ErrorPresentation, ErrorScope};
use haider_protocol::ids::{ArtifactRef, BranchId, DeviceId, EventId, NodeId, SessionId};
use haider_protocol::tool::AttachmentBlock;
use haider_rpc::{ErrorData, RequestBody, ResponseBody};
use haider_tui::app::{AppEvent, AppModel, AppRequest, Pasted, RuntimeMode, Screen};
use haider_tui::link::{CommandContext, map_response, request_body};
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply};
use haider_tui::projection::RawOutcome;
use haider_tui::render::render;
use haider_tui::runtime::{ShellRequest, attach_read_effects, live_pass};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
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

fn pdf_bytes(pages: u32) -> Vec<u8> {
    let kids = (0..pages)
        .map(|index| format!("{} 0 R", index + 3))
        .collect::<Vec<_>>()
        .join(" ");
    let mut pdf = format!(
        "%PDF-1.4\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
         2 0 obj\n<< /Type /Pages /Count {pages} /Kids [{kids}] >>\nendobj\n"
    );
    for index in 0..pages {
        pdf.push_str(&format!(
            "{} 0 obj\n<< /Type /Page /Parent 2 0 R >>\nendobj\n",
            index + 3
        ));
    }
    pdf.push_str("trailer\n<< /Root 1 0 R >>\n%%EOF\n");
    pdf.into_bytes()
}

fn draw_text(model: &AppModel, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            render(model, frame);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
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
    let plain = haider_tui::plain::render_plain(&model.projection, 0, None);
    assert!(
        plain.contains("what is in this image? [+1 attachment(s)]"),
        "the transcript names the real attachment: {plain}"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn pdf_chip_uses_pdf_glyph_basename_pages_and_transcript_plain_parity() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    let bytes = pdf_bytes(12);
    let dir = std::env::temp_dir().join(format!(
        "haider-b4b-pdf-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("report.pdf");
    std::fs::write(&path, &bytes).expect("temp PDF writes");

    attach_read_effects(&mut model, &path.display().to_string());
    let chip = &model.composer.attachments()[0];
    assert_eq!(chip.kind.glyph(), "▤");
    assert_eq!(chip.label, "report.pdf · 12p");
    let commands = drain(&mut driver, &mut model);
    let put = commands
        .iter()
        .find(|command| matches!(command, LiveCommand::ArtifactPut { .. }))
        .expect("PDF upload")
        .clone();
    let upload = match &put {
        LiveCommand::ArtifactPut { upload, .. } => *upload,
        _ => unreachable!(),
    };
    let artifact = ArtifactRef::new(format!("blake3:{:0>64}", "pdf"));
    for reply in map_response(
        &CommandContext::of(&put),
        ResponseBody::ArtifactPut {
            artifact: artifact.clone(),
            bytes: bytes.len() as u64,
        },
    ) {
        driver.apply(&mut model, reply);
    }
    assert_eq!(model.composer.attachments()[0].upload, upload);

    let styled = draw_text(&model, 100, 28);
    assert!(
        styled.contains("[▤ report.pdf · 12p]"),
        "styled composer must show the PDF chip: {styled}"
    );

    assert_eq!(
        model.route_raw(&raw(
            1,
            &EventPayload::UserMessage {
                text: "summarize the report".into(),
                attachments: vec![AttachmentBlock::Pdf {
                    artifact,
                    name: "report.pdf".into(),
                    pages: 12,
                    delivery: haider_protocol::tool::PdfDeliveryMode::NativeDocument,
                }],
                mode: haider_protocol::DeliveryMode::Queue,
            }
        )),
        RawOutcome::Applied
    );
    let plain = haider_tui::plain::render_plain(&model.projection, 0, None);
    assert!(
        plain.contains("summarize the report [+1 attachment(s)]"),
        "plain transcript keeps attachment-count parity: {plain}"
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn daemon_pdf_admission_rejection_keeps_its_typed_card_through_the_link() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    submit(&mut model, "read the report");
    let commands = drain(&mut driver, &mut model);
    let submit = commands
        .iter()
        .find(|command| matches!(command, LiveCommand::Submit { .. }))
        .expect("submit command");
    let presentation = ErrorPresentation::new(
        "pdf-too-many-pages",
        "PDF has too many pages",
        "report.pdf has 201 pages; the limit is 100",
        ErrorScope::Turn,
        [ErrorAction::None],
    );
    let artifact = ArtifactRef::new(format!("blake3:{:0>64}", "pdf-pages"));

    let replies = map_response(
        &CommandContext::of(submit),
        ResponseBody::Error {
            code: "pdf_too_many_pages".into(),
            message: presentation.detail.clone(),
            retryable: false,
            data: Some(ErrorData::PdfTooManyPages {
                index: 0,
                artifact,
                actual_pages: 201,
                max_pages: 100,
                presentation: presentation.clone(),
            }),
        },
    );
    assert!(matches!(
        replies.as_slice(),
        [LiveReply::Failed {
            presentation: Some(mapped),
            ..
        }] if mapped == &presentation
    ));
    for reply in replies {
        driver.apply(&mut model, reply);
    }

    assert_eq!(
        model
            .command_diagnostic
            .as_ref()
            .map(|card| card.subcode.as_str()),
        Some("pdf-too-many-pages")
    );
    let plain = haider_tui::plain::render_plain(&model.projection, 0, None);
    assert!(
        plain.contains(
            "PDF has too many pages — report.pdf has 201 pages; the limit is 100 [pdf-too-many-pages]"
        ),
        "typed E2-E4 card grammar survives in plain rendering: {plain}"
    );
}

// ---- law 2: oversized / non-image / over-count are honest, no upload --

#[test]
fn oversized_and_non_image_attach_are_honest_notices_with_no_upload() {
    let mut model = live_model();

    // Not an image AND not UTF-8 (G2): the image sniff falls through to
    // the text lane, which refuses binary with the DISTINCT encoding
    // message — never a silent chip.
    let binary_file = temp_file("blob.png", &[0xff, 0xfe, 0x00, 0x80, 0x81]);
    attach_read_effects(&mut model, &binary_file.display().to_string());
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|flash| flash.contains("not UTF-8 text")),
        "flash: {:?}",
        model.flash
    );
    assert!(model.composer.attachments().is_empty(), "no chip");
    assert!(model.requests.is_empty(), "no upload request");

    // The PDF-specific page cap uses the typed E2-E4 card grammar even
    // when the client can reject before uploading.
    let pdf_file = temp_file("over-pages.pdf", &pdf_bytes(101));
    attach_read_effects(&mut model, &pdf_file.display().to_string());
    assert_eq!(
        model
            .command_diagnostic
            .as_ref()
            .map(|card| card.subcode.as_str()),
        Some("pdf-too-many-pages")
    );
    assert!(
        haider_tui::plain::render_plain(&model.projection, 0, None)
            .contains("[pdf-too-many-pages]"),
        "the local PDF refusal is a typed transcript card"
    );
    assert!(model.composer.attachments().is_empty(), "no PDF chip");
    assert!(model.requests.is_empty(), "no PDF upload request");
    let _ = std::fs::remove_file(&binary_file);

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
                bytes: Some(0),
                artifact: Some(ArtifactRef::new(format!("blake3:{lines:0>64}"))),
                carried: None,
            });
    }
    run_slash(&mut model, "/attach sixth.png");
    assert_eq!(
        model.flash.as_deref(),
        Some("· 5 attachments a turn — ⌫ at the start removes one")
    );
    assert!(model.requests.is_empty(), "no read request for the sixth");

    let _ = std::fs::remove_file(&big_file);
    let _ = std::fs::remove_file(&pdf_file);
}

// ---- law 2b (G2): the text fallback chips a File and rides the wire ----

/// LAW (G2 TUI half): a readable NON-image `/attach` falls back to the
/// UTF-8 text lane — the chip is `name · N lines`, the upload carries the
/// exact file bytes, and the completed submit rides an
/// `AttachmentBlock::File` with the sanitized basename on the wire.
///
/// MUTATION CHECK: drop the fallback from `attach_read_effects` or mint an
/// Image/PastedText block from `PendingKind::File` in `ready_block`.
/// Expected RUNTIME failure: no chip appears, or the submit's wire JSON
/// loses `"kind": "file"` / the name.
#[test]
fn attach_text_fallback_chips_a_file_and_submit_carries_the_block() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    let bytes = b"alpha line\nbeta line\ngamma line";
    // A CLEAN basename (the wire name is the basename, so the unique part
    // must live in the directory, not the file name).
    let dir = std::env::temp_dir().join(format!(
        "haider-b4b-g2-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("notes.md");
    std::fs::write(&path, bytes).expect("temp file writes");

    attach_read_effects(&mut model, &path.display().to_string());
    let chips = model.composer.attachments();
    assert_eq!(chips.len(), 1);
    assert!(chips[0].artifact.is_none(), "the chip is still uploading");
    assert!(
        chips[0].label.contains("notes.md") && chips[0].label.contains("3 lines"),
        "label names the file and its line count: {}",
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
    assert_eq!(sent.as_slice(), bytes, "the exact file bytes travel");

    let artifact = ArtifactRef::new(format!("blake3:{:0>64}", "g2f"));
    for reply in map_response(
        &CommandContext::of(&put),
        ResponseBody::ArtifactPut {
            artifact: artifact.clone(),
            bytes: bytes.len() as u64,
        },
    ) {
        driver.apply(&mut model, reply);
    }
    let _ = (upload, surface);
    assert!(
        !model.composer.has_uploading_attachment(),
        "the chip is ready"
    );

    submit(&mut model, "summarize the attached notes");
    let commands = drain(&mut driver, &mut model);
    let submit_command = commands
        .iter()
        .find(|command| matches!(command, LiveCommand::Submit { .. }))
        .expect("the submit")
        .clone();
    let LiveCommand::Submit { attachments, .. } = submit_command.clone() else {
        unreachable!()
    };
    assert_eq!(
        attachments,
        vec![AttachmentBlock::File {
            artifact: artifact.clone(),
            name: "notes.md".to_owned(),
            lines: 3,
        }]
    );
    // EXACT WIRE: the tagged file block rides `turn.submit`.
    let encoded = serde_json::to_value(request_body(submit_command)).expect("encodes");
    assert_eq!(
        encoded.get("attachments").expect("attachments ride"),
        &serde_json::json!([{
            "kind": "file",
            "artifact": artifact.as_str(),
            "name": "notes.md",
            "lines": 3,
        }])
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- law 3: the paste pill is LOCAL (QoL wave) -------------------------

/// MUTATION CHECK: route `big_paste` back through
/// `begin_attachment_upload`. Expected runtime failure: an `ArtifactPut`
/// command appears, the composer gains a chip instead of the atomic
/// placeholder, and the submit's TEXT no longer carries the pasted bytes.
#[test]
fn paste_is_a_local_pill_never_an_upload() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");

    model.handle(AppEvent::Paste(Pasted::new(
        "line one\r\nline two\nline three\nline four\nline five".to_owned(),
    )));
    // The pill is an atomic DRAFT placeholder — no chip, no artifact, no
    // daemon feature consulted.
    assert_eq!(model.composer, "[Pasted text #1 +5 lines]");
    assert!(model.composer.attachments().is_empty(), "no chip");
    let commands = drain(&mut driver, &mut model);
    assert!(
        !commands
            .iter()
            .any(|command| matches!(command, LiveCommand::ArtifactPut { .. })),
        "nothing uploads"
    );

    // Submit expands the placeholder byte-exact at its position — the
    // wire carries the full NORMALIZED paste as text, attachments empty.
    submit(&mut model, " summarize");
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
    assert_eq!(
        text,
        "line one\nline two\nline three\nline four\nline five summarize"
    );
    assert!(attachments.is_empty(), "no blocks ride a paste");

    // Wire shape of the pasted-text block — the vocabulary stays wire-
    // legal for daemons that store text by ref, even though the TUI's
    // paste path no longer mints it.
    let artifact = ArtifactRef::new(format!("blake3:{:0>64}", "paste"));
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

    // ⌫ at the very start removes the NEWEST chip — and ONLY at the
    // start: mid-text it stays a text edit and the chips stay put.
    model.begin_attachment_upload(
        vec![9],
        haider_tui::composer::PendingKind::PastedText { lines: 9 },
        "[Pasted 9 lines]".to_owned(),
    );
    model.requests.clear();
    model.handle(key(KeyCode::Char('x')));
    model.handle(key(KeyCode::Backspace));
    assert_eq!(model.composer, "", "mid-text ⌫ deleted the grapheme");
    assert_eq!(
        model.composer.attachments().len(),
        2,
        "mid-text ⌫ ate no chip"
    );
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

// ---- capture: the reply completes the ISSUING draft -------------------

/// MUTATION-DRIVEN (B4b executed-mutation round, survivor finding): a
/// `complete_upload` that ignored the captured surface and wrote to the
/// on-screen composer passed the entire suite — the reply always landed
/// while the issuing surface was still displayed. This observer parks the
/// issuing draft first: the reply must complete the chip on the draft
/// that ISSUED the upload, not vanish against whatever is on screen.
#[test]
fn upload_reply_completes_the_issuing_draft_not_the_displayed_surface() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");

    // An /attach text upload (pastes are draft-local since the QoL wave
    // and issue no upload to park).
    let path = temp_file("parked.md", b"p1\np2\np3\np4\np5");
    attach_read_effects(&mut model, &path.display().to_string());
    let issuing_surface = model.surface_key();
    let commands = drain(&mut driver, &mut model);
    let LiveCommand::ArtifactPut {
        upload, surface, ..
    } = commands
        .iter()
        .find(|command| matches!(command, LiveCommand::ArtifactPut { .. }))
        .expect("the upload")
        .clone()
    else {
        unreachable!()
    };
    assert_eq!(surface, issuing_surface);

    // ⌃C walks back to the launcher — the issuing draft PARKS with its
    // uploading chip; the launcher composer (no chips) is now on screen.
    model.handle(AppEvent::Key(ratatui::crossterm::event::KeyEvent::new(
        KeyCode::Char('c'),
        ratatui::crossterm::event::KeyModifiers::CONTROL,
    )));
    assert_eq!(model.screen, Screen::Launcher);
    assert!(model.composer.attachments().is_empty());

    // The verified address lands while the launcher is displayed…
    let artifact = ArtifactRef::new(format!("blake3:{:0>64}", "parked"));
    driver.apply(
        &mut model,
        LiveReply::ArtifactUploaded {
            upload,
            surface,
            artifact: artifact.clone(),
        },
    );
    assert!(
        model.composer.attachments().is_empty(),
        "the displayed launcher composer gained nothing"
    );

    // …and the ISSUING draft comes back with its chip READY.
    model.open_session(&sid());
    let chips = model.composer.attachments();
    assert_eq!(chips.len(), 1);
    assert_eq!(
        chips[0].artifact.as_ref(),
        Some(&artifact),
        "the reply completed the issuing draft's chip"
    );
    let _ = std::fs::remove_file(&path);
}

// ---- disconnect: in-flight uploads die honestly -----------------------

/// `artifact.put` is receipt-free: no reply survives a socket and nothing
/// resends the bytes, so a disconnect must kill the uploading chip WITH a
/// notice — a chip left "uploading" forever is a promise no reconnect can
/// keep.
#[test]
fn disconnect_drops_uploading_chips_with_an_honest_notice() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");

    // An /attach text upload holds the in-flight chip (pastes issue no
    // upload since the QoL wave).
    let path = temp_file("dropped.md", b"d1\nd2\nd3\nd4\nd5");
    attach_read_effects(&mut model, &path.display().to_string());
    drain(&mut driver, &mut model);
    assert!(model.composer.has_uploading_attachment());

    driver.apply(
        &mut model,
        LiveReply::Disconnected {
            reason: "socket closed".to_owned(),
        },
    );
    assert!(
        model.composer.attachments().is_empty(),
        "the in-flight chip died with the socket"
    );
    assert_eq!(
        model.flash.as_deref(),
        Some(
            "· reconnecting — socket closed (1 in-flight attachment upload(s) dropped — /attach again)"
        )
    );
    let _ = std::fs::remove_file(&path);
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

    // The big paste needs NO daemon feature (QoL wave): the pill is
    // draft-local and its content rides the submit as text — nothing is
    // claimed of the stale daemon, nothing uploads.
    model.handle(AppEvent::Paste(Pasted::new("a\nb\nc\nd\ne".to_owned())));
    assert!(model.composer.attachments().is_empty(), "no chip");
    assert_eq!(model.composer, "[Pasted text #1 +5 lines]");
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

    // The demo paste is the SAME local pill (the sim's literal token
    // retired with the QoL wave) — still no chip, still no request.
    model.handle(AppEvent::Paste(Pasted::new("a\nb\nc\nd\ne".to_owned())));
    assert_eq!(model.composer, "[Pasted text #1 +5 lines]");
    assert!(model.composer.attachments().is_empty(), "no chip");
    assert!(model.requests.is_empty(), "no upload request");
}
