//! Session identity must be shared by every compact path renderer.
#![allow(clippy::expect_used)]
use haider_protocol::{
    EventPayload,
    ids::ItemId,
    item::{ItemEvent, TurnItem},
    workspace::{WorkspaceEventPayload, WorkspaceSelected},
};
use haider_tui::{
    app::AppModel,
    demo_store,
    live::{LiveDriver, LiveReply},
    plain::render_plain_with_context,
};
use serde_json::json;
mod tuivirt_common;
use tuivirt_common::{draw, session_model};

#[test]
fn workspace_notices_follow_the_render_context_in_plain_tui_and_demo_replay() {
    let mut model = session_model();
    model.cwd = "/opt/project".into();
    model.session_workspace_cwd = Some(model.cwd.clone());
    model
        .projection
        .apply_workspace_event(&WorkspaceEventPayload::WorkspaceSelected(
            WorkspaceSelected {
                path: "C:/Users/alice/next".into(),
                previous_path: None,
            },
        ));
    model
        .projection
        .apply(&EventPayload::Item(ItemEvent::Completed {
            item_id: ItemId::new("file-change"),
            item: TurnItem::FileChange {
                path: "/opt/project/src/lib.rs".into(),
                added: 1,
                removed: 0,
            },
        }));
    for (home, expected) in [
        ("C:/Users/alice", "~/next"),
        ("C:/Users/carol", "C:/Users/<user>/next"),
    ] {
        model.display_homes = vec![home.into()];
        let plain =
            render_plain_with_context(&model.projection, 0, None, &model.tool_path_context());
        assert!(
            plain.contains(&format!("⇄ workspace → {expected}")),
            "{plain}"
        );
        assert!(plain.contains("± src/lib.rs +1 -0"), "{plain}");
        for (width, height) in [(118, 36), (80, 24)] {
            let text = draw(&model, width, height).rows.join("\n");
            assert!(
                text.contains(&format!("⇄ workspace → {expected}")),
                "{text}"
            );
            assert!(text.contains("Edit(src/lib.rs)"), "{text}");
            assert!(!text.contains("C:/Users/alice/next"), "{text}");
        }
    }
    let id = model.active_session.clone().expect("session");
    let wire = serde_json::to_value(demo_store::snapshot(&model)).expect("snapshot JSON");
    let entries = wire["sessions"]
        .as_array()
        .expect("sessions")
        .iter()
        .find(|session| session["id"] == id.as_str())
        .expect("active session")["projection"]["entries"]
        .as_array()
        .expect("entries");
    // Released v2 readers strictly decode EntryDto. A workspace receipt
    // must use the existing item/extension carrier, never a new row kind.
    let receipt = entries
        .iter()
        .find(|entry| entry["item"]["data"]["type"] == "workspace_selected")
        .expect("workspace receipt");
    assert_eq!(receipt["kind"], "item");
    assert_eq!(receipt["item"]["item"], "extension");
    assert_eq!(receipt["item"]["data"]["path"], "C:/Users/alice/next");
    let dto: demo_store::StateDto = serde_json::from_value(wire).expect("decode snapshot");
    let mut restored = AppModel::new();
    restored.display_homes = vec!["C:/Users/alice".into()];
    demo_store::hydrate(&mut restored, dto);
    restored.open_session(&id);
    let plain =
        render_plain_with_context(&restored.projection, 0, None, &restored.tool_path_context());
    assert!(plain.contains("⇄ workspace → ~/next"), "{plain}");
}

#[test]
fn session_list_summaries_and_workspace_receipts_share_the_model_homes() {
    let mut model = session_model();
    model.display_homes = vec!["C:/Users/alice".into()];
    let session = model.active_session.clone().expect("session");
    let summary:haider_rpc::SessionSummary=serde_json::from_value(json!({
        "session_id":session, "head_seq":1, "worker_generation":1, "workspace_cwd":"C:/Users/alice/project"
    })).expect("summary");
    let mut driver = LiveDriver::new("test");
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![summary.clone()],
            next_cursor: None,
        },
    );
    assert!(
        model
            .session_browser_rows()
            .iter()
            .any(|row| row.id == session && row.dir == "~/project")
    );
    let mut other_summary = summary.clone();
    other_summary.session_id = haider_protocol::ids::SessionId::new("context-other");
    other_summary.workspace_cwd = Some("C:/Users/carol/project".into());
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![other_summary.clone()],
            next_cursor: None,
        },
    );
    assert!(
        model.session_browser_rows().iter().any(|row| {
            row.id == other_summary.session_id && row.dir == "C:/Users/<user>/project"
        })
    );
    model.note_summary_counts(&summary);
    assert_eq!(model.session_dir, "~/project");
    assert_eq!(
        model.display_path("C:/Users/carol/project"),
        "C:/Users/<user>/project"
    );
    driver.apply(
        &mut model,
        LiveReply::WorkspaceSelected {
            command_id: haider_rpc::CommandId::new("context-test"),
            session,
            path: "C:/Users/alice/next".into(),
            worker_generation: 1,
        },
    );
    assert_eq!(model.session_dir, "~/next");
    assert_eq!(
        model.flash.as_deref(),
        Some("· workspace re-rooted → ~/next")
    );
}

#[test]
fn shell_and_file_monitor_overlays_use_the_session_home_snapshot() {
    let mut model = session_model();
    model.display_homes = vec!["C:/Users/alice".into()];
    model.shells.push(haider_rpc::ShellWire {
        id: "context-shell".into(),
        kind: haider_rpc::ShellKindWire::Local,
        status: haider_rpc::ShellStatusWire::Running,
        setup_error: None,
        title: "tests".into(),
        cwd_or_host: "C:/Users/alice/project".into(),
        created_at_ms: 1,
        last_activity_ms: 2,
        bytes_out: 3,
    });
    model.monitors.push(haider_rpc::MonitorRegistrationWire {
        monitor_id: "context-monitor".into(),
        session_id: model.active_session.clone().expect("session"),
        branch_id: None,
        agent_id: None,
        source: haider_rpc::MonitorSourceWire::File {
            path: "C:/Users/alice/watch.rs".into(),
        },
        filter: None,
        action: haider_rpc::MonitorActionWire {
            report: true,
            follow_up: None,
        },
        occurrence: haider_rpc::MonitorOccurrenceWire::Every,
        created_at_ms: 1,
        start_source_sequence: 0,
        expires_at_ms: None,
        state: haider_rpc::MonitorStateWire::Armed,
        last_event: None,
        fire_count: 0,
        next_fire_at_ms: None,
        source_summary: "stale raw summary".into(),
    });
    for (width, height) in [(118, 36), (80, 24)] {
        model.shells_open = true;
        let text = draw(&model, width, height).rows.join("\n");
        assert!(text.contains("~/project"), "{text}");
        assert!(!text.contains("C:/Users/alice"), "{text}");
        model.shells_open = false;
        model.monitors_open = true;
        let text = draw(&model, width, height).rows.join("\n");
        assert!(text.contains("file ~/watch.rs"), "{text}");
        assert!(
            !text.contains("C:/Users/alice") && !text.contains("stale raw summary"),
            "{text}"
        );
        model.monitors_open = false;
    }
    model.cwd = "C:/Users/alice/project".into();
    model.session_workspace_cwd = Some(model.cwd.clone());
    model.monitors[0].source = haider_rpc::MonitorSourceWire::File {
        path: "C:/Users/alice/project/src/watch.rs".into(),
    };
    model.monitors_open = true;
    for (width, height) in [(118, 36), (80, 24)] {
        let text = draw(&model, width, height).rows.join("\n");
        assert!(text.contains("file src/watch.rs"), "{text}");
    }
}

#[test]
fn file_review_headers_share_session_context_without_changing_the_review_path() {
    use haider_protocol::{menu::MenuKind, state::RunState};
    for (path, expected) in [
        ("C:/Users/alice/project/src/lib.rs", "src/lib.rs"),
        ("C:/Users/alice/notes.md", "~/notes.md"),
        ("C:/Users/carol/notes.md", "C:/Users/<user>/notes.md"),
    ] {
        let mut model = session_model();
        model.cwd = "C:/Users/alice/project".into();
        model.session_workspace_cwd = Some(model.cwd.clone());
        model.display_homes = vec!["C:/Users/alice".into()];
        let mut menu = tuivirt_common::file_review_menu();
        if let MenuKind::Permission {
            file_review: Some(review),
            ..
        } = &mut menu.kind
        {
            review.path = path.into();
        }
        let id = menu.id.clone();
        model.projection.apply(&EventPayload::MenuOpened(menu));
        model
            .projection
            .apply(&EventPayload::RunState(RunState::PermissionRequired {
                menu: id,
            }));
        let plain =
            render_plain_with_context(&model.projection, 0, None, &model.tool_path_context());
        assert!(
            plain.contains(&format!("file review: {expected}")),
            "{plain}"
        );
        for (width, height) in [(118, 36), (80, 24)] {
            let text = draw(&model, width, height).rows.join("\n");
            assert!(text.contains(&format!("REVIEW · {expected}")), "{text}");
            assert!(
                !text.contains("C:/Users/carol") && !text.contains("C:/Users/alice"),
                "{text}"
            );
        }
        assert!(
            matches!(&model.projection.open_menu().expect("menu").kind,MenuKind::Permission{file_review:Some(review),..} if review.path==path)
        );
    }
}

#[test]
fn image_rows_keep_workspace_labels_and_broad_root_account_masks() {
    use haider_protocol::image::{IMAGE_CREATED_EXTENSION_KIND, ImageCreatedV1};
    for (workspace, path, reported, expected) in [
        (
            "C:/Users/alice/project",
            "C:/Users/alice/project/artifacts/x.png",
            "artifacts/x.png",
            "artifacts/x.png",
        ),
        (
            "C:/Users",
            "C:/Users/carol/x.png",
            "carol/x.png",
            "C:/Users/<user>/x.png",
        ),
        (
            "C:/Users/alice/project",
            "C:/Users/alice/notes.png",
            "C:/Users/alice/notes.png",
            "~/notes.png",
        ),
    ] {
        let mut model = session_model();
        model.cwd = workspace.into();
        model.session_workspace_cwd = Some(workspace.into());
        model.display_homes = vec!["C:/Users/alice".into()];
        let image = ImageCreatedV1 {
            path: path.into(),
            display_path: reported.into(),
            media_type: "image/png".into(),
            byte_len: 1024,
            width: None,
            height: None,
            call_id: "context-image".into(),
            tool: "process_exec".into(),
        };
        model
            .projection
            .apply(&EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("context-image"),
                item: TurnItem::Extension {
                    kind: IMAGE_CREATED_EXTENSION_KIND.into(),
                    data: serde_json::to_value(&image).expect("image JSON"),
                },
            }));
        let plain =
            render_plain_with_context(&model.projection, 0, None, &model.tool_path_context());
        assert!(
            plain.contains(&format!("image · {expected} · 1 KB")),
            "{plain}"
        );
        for (width, height) in [(118, 36), (80, 24)] {
            let frame = draw(&model, width, height);
            let text = frame.rows.join("\n");
            assert!(
                text.contains(&format!("image · {expected} · 1 KB")),
                "{text}"
            );
            assert!(
                frame
                    .hits
                    .iter()
                    .any(|(_, hit)| hit == &haider_tui::app::Hit::RevealPath(path.into())),
                "the action must keep its actual path"
            );
        }
    }
}

#[test]
fn local_runtime_status_uses_the_same_home_snapshot_as_the_session_rows() {
    use haider_tui::talk::{RuntimeRowState, SetupStage, TalkSetupCard};
    let mut model = session_model();
    model.display_homes = vec!["C:/Users/alice".into()];
    let mut card = TalkSetupCard::new(Default::default(), false);
    card.stage = SetupStage::Local;
    card.loaded = true;
    card.runtime = RuntimeRowState::Found("C:/Users/alice/bin/whisper-cli".into());
    model.talk_setup = Some(card);
    for (width, height) in [(118, 36), (80, 24)] {
        let text = draw(&model, width, height).rows.join("\n");
        assert!(text.contains("~/bin/whisper-cli"), "{text}");
        assert!(!text.contains("C:/Users/alice"), "{text}");
    }
}
