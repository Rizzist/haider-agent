//! TUI4c — item 13b: across-restart persistence for the demo (the sim's
//! localStorage contract, ported to `demo-tui-state.json`). The five load
//! guards in order, the deliberately-not-restored set, the stale-card note,
//! and `/reset`'s purge — driven through production dispatch (the same
//! `DemoDriver` seams the interactive loop uses) wherever a turn is needed.
#![allow(clippy::expect_used)]

use haider_tui::app::{AppModel, AppRequest, ChipModel, DemoRequest, Screen};
use haider_tui::demo_store::{
    ChipDto, DEMO_STORE_VERSION, DemoStore, HeadDto, ProjectionDto, SessionDto, SessionIdDto,
    StateDto, hydrate, snapshot,
};
use haider_tui::identity::{UiGeneration, demo_session_id};
use haider_tui::render::render;
use haider_tui::script::{ChipDisplayState, DemoEvent, ROSTER_FIRST_CLAIM, roster_at};
use haider_tui::theme::ThemeKey;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::KeyCode;

mod common;
use common::{drain, driver_for, key, launcher_model, pump_until, submit};

fn rows(model: &AppModel, width: u16, height: u16) -> Vec<String> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            render(model, frame);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect()
}

/// A session slot BY GENERATION. W3c3 split identity in two (report R11
/// cut 1): the demo's stable numbering `1..n` is now the row's `ui_gen`
/// and its protocol id is `demo-session-{ui_gen}`, so every call site here
/// keeps naming exactly the row it always did.
fn slot(model: &AppModel, generation: u64) -> &haider_tui::session::SessionState {
    let generation = UiGeneration::new(generation);
    let entry = model
        .sessions
        .iter()
        .find(|entry| entry.ui_gen == generation)
        .expect("session slot");
    assert_eq!(
        entry.id,
        demo_session_id(generation),
        "the demo's protocol id is derived from its generation, always"
    );
    entry
}

/// Deep chip equality on everything the store persists (the runtime types
/// carry non-persisted extras like `removing`, compared via `closed`).
fn assert_chips_equal(original: &[ChipModel], hydrated: &[ChipModel], context: &str) {
    assert_eq!(original.len(), hydrated.len(), "chip count · {context}");
    for (a, b) in original.iter().zip(hydrated) {
        assert_eq!(a.agent, b.agent, "{context}");
        assert_eq!(a.ros, b.ros, "chip ros · {context}");
        assert_eq!(a.callsign, b.callsign, "{context}");
        assert_eq!(a.hon, b.hon, "{context}");
        assert_eq!(a.full, b.full, "{context}");
        assert_eq!(a.name, b.name, "{context}");
        assert_eq!(a.model, b.model, "{context}");
        assert_eq!(a.device, b.device, "{context}");
        assert_eq!(a.state, b.state, "chip state · {context}");
        assert_eq!(a.tokens, b.tokens, "{context}");
        assert_eq!(a.question, b.question, "chip question · {context}");
        assert_eq!(a.closed, b.closed, "{context}");
        assert_eq!(
            a.transcript.entries(),
            b.transcript.entries(),
            "chip transcript · {context}"
        );
        assert_chips_equal(&a.children, &b.children, context);
    }
}

fn all_callsigns(model: &AppModel) -> Vec<String> {
    fn walk(chips: &[ChipModel], out: &mut Vec<String>) {
        for chip in chips {
            out.push(chip.callsign.clone());
            walk(&chip.children, out);
        }
    }
    let mut names = Vec::new();
    for session in &model.sessions {
        names.push(session.head.0.clone());
        walk(&session.chips, &mut names);
    }
    names
}

fn chip_state_by_name(model: &AppModel, name: &str) -> Option<ChipDisplayState> {
    model
        .chips
        .iter()
        .find(|chip| chip.name == name)
        .map(|chip| chip.state)
}

/// A minimal hand-built session DTO (the corrupt/backfill fixtures).
fn bare_session(id: u64, name: &str, head: Option<HeadDto>) -> SessionDto {
    SessionDto {
        id: SessionIdDto::Current(demo_session_id(UiGeneration::new(id)).as_str().to_owned()),
        ui_gen: Some(id),
        name: Some(name.to_owned()),
        title: None,
        head,
        dir: "~/dev".to_owned(),
        model_short: "fable-5".to_owned(),
        device: "this-mac".to_owned(),
        ago: "now".to_owned(),
        branches: 1,
        turns_offset: 0,
        projection: ProjectionDto::default(),
        chips: Vec::new(),
    }
}

fn bare_chip(agent: &str, callsign: &str, ros: Option<u64>) -> ChipDto {
    ChipDto {
        agent: agent.to_owned(),
        ros,
        callsign: callsign.to_owned(),
        hon: if callsign.is_empty() {
            String::new()
        } else {
            "(r)".to_owned()
        },
        full: callsign.to_owned(),
        name: "task".to_owned(),
        model: "gpt-5.6".to_owned(),
        device: "local".to_owned(),
        state: "RUNNING".to_owned(),
        tokens: 100,
        question: None,
        closed: false,
        children: Vec::new(),
        transcript: ProjectionDto::default(),
    }
}

// ---- The flagship round trip: two user sessions + the seeds, through ----
// ---- production dispatch, serialized to the REAL file and back.       ----

#[tokio::test(start_paused = true)]
async fn two_user_sessions_and_the_seeds_round_trip_through_the_file() {
    // MUTATION CHECK (guard 3): skip the ros walk (or drop the
    // `model.roster.store(next)` in hydrate) and the hydrated roster stays
    // at 3 — the fresh claim below re-issues Hasan, which the persisted
    // session 4 already wears, and the counter/uniqueness asserts fail.
    let mut model = launcher_model();
    // Production wiring (run_demo does exactly this): ONE honour roll.
    let (mut driver, mut rx) = driver_for(&model);

    // Session A (id 4, head Hasan @ ros 3): a generic turn to completion,
    // auto-title included.
    submit(&mut model, "wire the stripe retries");
    pump_until(&mut driver, &mut rx, &mut model, "turn A done", |m| {
        !m.turn_active
            && m.session_title.is_some()
            && !m.projection.entries().is_empty()
            && m.projection.badge() == "IDLE"
    })
    .await;
    submit(&mut model, "/clear");

    // Session B (id 5, head Husayn @ ros 4): the two-subagent turn — its
    // chips claim ros 5+6 from the SHARED counter; settle at the parked
    // question cards (tests amber ? · docs recovery ⌁).
    submit(&mut model, "use two subagents to split this work");
    pump_until(&mut driver, &mut rx, &mut model, "turn B settled", |m| {
        !m.turn_active
            && m.session_title.is_some()
            && chip_state_by_name(m, "tests") == Some(ChipDisplayState::InputRequired)
            && chip_state_by_name(m, "docs") == Some(ChipDisplayState::Error)
    })
    .await;
    submit(&mut model, "/clear");
    assert_eq!(model.screen, Screen::Launcher);

    // Serialize → the real file → load → hydrate a fresh model.
    let dir = tempfile::tempdir().expect("tempdir");
    let mut store = DemoStore::at(dir.path().join("demo-tui-state.json"));
    store.save(&model);
    assert!(store.path().exists(), "the save actually wrote");
    let reload = DemoStore::at(store.path().to_path_buf());
    let dto = reload.load().expect("a saved state loads");
    let mut hydrated = launcher_model();
    hydrate(&mut hydrated, dto);

    // Same rendered launcher — the whole frame, byte for byte.
    assert_eq!(
        rows(&model, 118, 34),
        rows(&hydrated, 118, 34),
        "the hydrated launcher must render exactly the saved one"
    );

    // Same re-entered transcripts, sessions and chips (seeds included).
    assert_eq!(model.sessions.len(), 5);
    assert_eq!(hydrated.sessions.len(), 5);
    for id in [5u64, 4, 1, 2, 3] {
        assert_eq!(
            slot(&model, id).projection.entries(),
            slot(&hydrated, id).projection.entries(),
            "transcript of session {id}"
        );
        assert!(
            !slot(&hydrated, id).turn_active,
            "run states are NOT restored — every session loads IDLE"
        );
        assert_chips_equal(
            &slot(&model, id).chips,
            &slot(&hydrated, id).chips,
            &format!("session {id}"),
        );
    }

    // No duplicate callsigns anywhere, and the honour-roll continues where
    // it left off: heads 0-2 (seeds) + 3,4 (users), seed chip 15, turn
    // chips 5,6 → next claim is 7.
    let names = all_callsigns(&hydrated);
    let unique: std::collections::HashSet<&String> = names.iter().collect();
    assert_eq!(unique.len(), names.len(), "duplicate callsign: {names:?}");
    assert_eq!(
        model.roster.load(std::sync::atomic::Ordering::SeqCst),
        7,
        "claims 3-6 burned live"
    );
    // Guard 3 across the reload: `next = max(3, every ros + 1)` — the L1
    // seed chip's recorded ros 15 dominates, so the counter lands at 16
    // (the sim's exact arithmetic: its seed literal spreads `rosterAt(15)`,
    // tui.js:559, and its load walks every persisted ros, tui.js:711-721).
    // Skipping 7-14 is safe by construction; re-issuing 3-6 or 15 is the
    // bug this guard exists to prevent.
    let next = hydrated.roster.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(next, 16, "guard 3: the counter resumes past EVERY ros");
    submit(&mut hydrated, "a brand new task");
    assert_eq!(
        hydrated.session_head.0,
        roster_at(next).callsign,
        "the claim after reload continues the roll — never a re-issue"
    );
    assert!(
        !names.contains(&hydrated.session_head.0),
        "…and it duplicates no persisted callsign"
    );
}

// ---- Guard 1: corrupt / missing / empty → seeds, never a crash ----

#[test]
fn corrupt_and_missing_files_keep_the_seeds() {
    // MUTATION CHECK: accept an empty `sessions` array in `load` (drop the
    // non-empty guard) and the last case yields Some — hydrating it would
    // wipe every seed row, exactly the sim bug guard 1 fixed.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("demo-tui-state.json");
    let store = DemoStore::at(path.clone());
    assert!(store.load().is_none(), "missing file → seeds");

    std::fs::write(&path, "{\"sessions\":[{\"id\":").expect("write");
    assert!(store.load().is_none(), "truncated JSON → seeds");

    std::fs::write(&path, "{\"sessions\": 7}").expect("write");
    assert!(store.load().is_none(), "wrong types → seeds");

    std::fs::write(&path, "[1,2,3]").expect("write");
    assert!(store.load().is_none(), "wrong root shape → seeds");

    std::fs::write(&path, "{\"sessions\": []}").expect("write");
    assert!(store.load().is_none(), "EMPTY session array → seeds");
}

// ---- /reset: purge the file, reseed, reset the counters ----

#[test]
fn reset_purges_the_state_file_and_restores_seeds() {
    // MUTATION CHECK: drop the reducer's PurgeDemoStore push (or the
    // remove_file in DemoStore::purge) and the request/file asserts fail —
    // a /reset that leaves the file behind resurrects the purged sessions
    // on the next boot.
    let dir = tempfile::tempdir().expect("tempdir");
    let mut store = DemoStore::at(dir.path().join("demo-tui-state.json"));
    let mut model = launcher_model();
    submit(&mut model, "some user work");
    model.requests.clear();
    store.save(&model);
    assert!(store.path().exists());

    submit(&mut model, "/reset");
    // W3c3 (report R11 cut 3): the purge left the COMMON `AppRequest`
    // vocabulary for a demo-only queue, so `run_live` cannot even name it
    // — a live reset can never delete demo persistence. Same law, same
    // /reset trigger, different (narrower) channel.
    assert!(
        model
            .demo_requests
            .iter()
            .any(|request| matches!(request, DemoRequest::PurgeStore)),
        "/reset must request the file purge"
    );
    // …and the SEMANTIC requests still ride the common queue. (The earlier
    // form of this assertion looked for `AppRequest::Reattach` — LIVE
    // vocabulary that `/reset` could never emit — so no mutation could
    // make it fail. What is actually assertable at runtime is that the
    // split routed each effect to the right queue.)
    assert!(
        model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::ResetAllSessions)),
        "/reset's teardown is semantic and stays on the common queue"
    );
    assert!(
        model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::ResetAura)),
        "…as does the aura reseed"
    );
    assert_eq!(
        model.demo_requests.len(),
        1,
        "and the demo-only queue carries exactly the purge, nothing else"
    );
    assert_eq!(model.sessions.len(), 3, "seeds restored");
    assert_eq!(
        model.roster.load(std::sync::atomic::Ordering::SeqCst),
        ROSTER_FIRST_CLAIM,
        "honour-roll reset"
    );
    // Review TUI4.1 P1-2: the allocator is MONOTONIC — /reset must NOT
    // rewind it, or a replacement session reuses a dead id and the
    // surviving control-tagged auto-title callback retitles it.
    assert!(
        model.next_ui_generation >= 5,
        "/reset never rewinds the identity allocator (monotonic identity law)"
    );

    // The runtime intercepts the request and purges (run_demo's arm).
    store.purge();
    assert!(!store.path().exists(), "the state file is gone");
}

// ---- Guard 4: sweep closed chips + callsign backfill ----

#[test]
fn closed_chips_are_swept_and_callsigns_backfilled_on_load() {
    // MUTATION CHECK (guard 4): drop the `sweep_closed_chips` call in
    // hydrate and the ⊘ chip below returns from the grave; drop the
    // backfill and two chips render nameless. Either revert fails here.
    let chips = vec![
        {
            let mut chip = bare_chip("gone", "Maytham", Some(19));
            chip.closed = true;
            chip
        },
        {
            // Persisted before the naming feature, ros recorded: the SAME
            // name re-derives without burning a claim.
            let mut chip = bare_chip("named-by-ros", "", Some(15));
            chip.children.push(bare_chip("nested", "Ammar", Some(17)));
            chip
        },
        // No callsign, no ros: burns `next++`.
        bare_chip("nameless", "", None),
    ];
    let mut session = bare_session(
        9,
        "old-work",
        Some(HeadDto {
            callsign: "Baqir".to_owned(),
            hon: "(a)".to_owned(),
            ros: Some(6),
        }),
    );
    session.chips = chips;
    let dto = StateDto {
        version: DEMO_STORE_VERSION,
        sessions: vec![session],
        theme: String::new(),
        vfs: std::collections::BTreeMap::new(),
        launcher_dir: String::new(),
        voice: None,
        card_seq: 0,
    };
    let mut model = launcher_model();
    hydrate(&mut model, dto);

    let entry = slot(&model, 9);
    assert_eq!(entry.chips.len(), 2, "the closed chip was swept on load");
    assert!(
        !entry.chips.iter().any(|chip| chip.agent == "gone"),
        "⊘ gone"
    );
    let by_ros = &entry.chips[0];
    assert_eq!(by_ros.callsign, "Salman", "ros 15 re-derives Salman");
    assert_eq!(by_ros.hon, "(r)");
    assert_eq!(by_ros.full, "Salman al-Farsi");
    assert_eq!(by_ros.ros, Some(15));
    // Walk: head 6→7 · gone 19→20 (the SWEPT chip's ros still counts — its
    // callsign was burned before it closed) · ros 15→16 stays under 20 ·
    // nested 17→18 under 20. Backfill for the nameless chip claims 20.
    let nameless = &entry.chips[1];
    assert_eq!(nameless.callsign, roster_at(20).callsign);
    assert_eq!(nameless.ros, Some(20));
    assert_eq!(
        model.roster.load(std::sync::atomic::Ordering::SeqCst),
        21,
        "rosterRef = next AFTER the walk (guard 5's first half)"
    );
}

// ---- Guard 2: the id-collision bump ----

#[test]
fn id_collision_bump_restores_card_seq_and_session_id_allocator() {
    // MUTATION CHECK (guard 2): drop the `card_seq` restore in hydrate and
    // the post-reload /voice card mints `voice-card-1` — the SAME id as the
    // persisted card, so a stale answer could reconfigure the new card
    // (review r2 P1-1's bug, resurrected across restarts). Drop the
    // `next_session_id` bump instead and the next session collides with a
    // persisted id.
    let mut model = launcher_model();
    submit(&mut model, "task one");
    submit(&mut model, "/voice");
    let persisted_card = model
        .projection
        .open_menu()
        .expect("voice card open")
        .id
        .clone();
    assert_eq!(persisted_card.as_str(), "voice-card-1");

    let dto_json = serde_json::to_string(&snapshot(&model)).expect("serialize");
    let dto: StateDto = serde_json::from_str(&dto_json).expect("parse");
    let mut hydrated = launcher_model();
    hydrate(&mut hydrated, dto);
    assert_eq!(hydrated.card_seq, 1, "card counter restored");
    assert_eq!(
        hydrated.next_ui_generation, 5,
        "identities resume past the persisted max"
    );

    hydrated.open_session(&demo_session_id(UiGeneration::new(4)));
    // The open card itself round-tripped; dismiss it (non-blocking) so the
    // composer is back for a fresh /voice.
    assert_eq!(
        hydrated.projection.open_menu().map(|menu| menu.id.clone()),
        Some(persisted_card.clone()),
        "the persisted open card is restored"
    );
    hydrated.handle(key(KeyCode::Esc));
    assert!(hydrated.projection.open_menu().is_none());
    submit(&mut hydrated, "/voice");
    let fresh_card = hydrated
        .projection
        .open_menu()
        .expect("fresh voice card")
        .id
        .clone();
    assert_eq!(fresh_card.as_str(), "voice-card-2");
    assert_ne!(fresh_card, persisted_card, "no id collision after reload");
}

// ---- Not-restored set: run states load IDLE, but idle(i) is verbatim ----

#[test]
fn a_persisted_idle_i_survives_hydration_verbatim() {
    // MUTATION CHECK: drop `interrupted` from ProjectionDto (or pass
    // `false` in projection_from_dto) and the reopened badge shows plain
    // IDLE — the interrupt marker lost across a restart. No arm survives a
    // restart, so nothing can overwrite the marker post-restore either.
    let mut model = launcher_model();
    submit(&mut model, "interrupt me please");
    model.requests.clear();
    model.handle(key(KeyCode::Esc)); // mid-turn esc = interrupt → idle (i)
    assert!(model.projection.interrupted());
    model.handle(key(KeyCode::Esc)); // idle esc = detach

    let dto_json = serde_json::to_string(&snapshot(&model)).expect("serialize");
    let dto: StateDto = serde_json::from_str(&dto_json).expect("parse");
    let mut hydrated = launcher_model();
    hydrate(&mut hydrated, dto);
    common::hit_session_named(&mut hydrated, "interrupt-me-please");
    assert_eq!(hydrated.screen, Screen::Session);
    assert!(
        !hydrated.turn_active,
        "loads IDLE — run states not restored"
    );
    assert!(
        hydrated.projection.interrupted(),
        "⏸ IDLE (i) survives hydration verbatim"
    );
    assert_eq!(hydrated.projection.badge(), "⏸ IDLE (i)");
}

// ---- The stale-menu note: a hydrated card has no live run attached ----

#[tokio::test(start_paused = true)]
async fn answering_a_hydrated_card_lands_the_stale_menu_note() {
    // MUTATION CHECK: drop the `has_session_arms` gate in the driver's
    // Answer arm (always-false stale) and the note never lands; make it
    // always-true instead and the live /voice answer below grows a bogus
    // note. Both directions fail here.
    use haider_protocol::ids::MenuId;
    use haider_protocol::menu::{Menu, MenuKind, MenuOption, MenuScope};
    let card = Menu {
        id: MenuId::new("t1-gate"),
        kind: MenuKind::Permission {
            effect_summary: "patch src/lib.rs".to_owned(),
        },
        title: "Allow fs_patch — lib.rs?".to_owned(),
        body: vec!["persisted across a restart".to_owned()],
        options: vec![
            MenuOption {
                key: "allow".to_owned(),
                label: "Allow once".to_owned(),
                detail: None,
                decision: None,
            },
            MenuOption {
                key: "deny".to_owned(),
                label: "Deny".to_owned(),
                detail: None,
                decision: None,
            },
        ],
        blocking: true,
        scope: MenuScope::Session,
        origin: "fs_patch".to_owned(),
        ttl_ms: None,
        timeout_option: None,
    };
    let mut session = bare_session(
        7,
        "parked-on-a-card",
        Some(HeadDto {
            callsign: "Hasan".to_owned(),
            hon: "(a)".to_owned(),
            ros: Some(3),
        }),
    );
    session.projection.menu = Some(card);
    let dto = StateDto {
        version: DEMO_STORE_VERSION,
        sessions: vec![session],
        theme: String::new(),
        vfs: std::collections::BTreeMap::new(),
        launcher_dir: String::new(),
        voice: None,
        card_seq: 0,
    };
    let mut model = launcher_model();
    let (mut driver, _rx) = driver_for(&model);
    hydrate(&mut model, dto);
    model.open_session(&demo_session_id(UiGeneration::new(7)));
    assert!(model.projection.open_menu().is_some(), "the card came back");

    // Answer it — no live arms exist for session 7 (nothing ran since
    // boot): the resolver died with the restart, so the sim's note lands.
    model.handle(key(KeyCode::Enter));
    let pending = model.outbox.remove(0);
    driver.consume(
        &mut model,
        driver.control_tag(),
        DemoEvent::Answer {
            origin: pending.origin,
            answer: pending.answer,
        },
    );
    assert!(model.projection.open_menu().is_none(), "card dismissed");
    let stale_note = "· stale menu dismissed — no live run attached (answered after reload)";
    let notes = |m: &AppModel| {
        m.projection
            .entries()
            .iter()
            .filter(|entry| format!("{entry:?}").contains(stale_note))
            .count()
    };
    assert_eq!(notes(&model), 1, "the sim's note, verbatim");

    // Counter-case: once a run is live in this session (its arm registers
    // at dispatch, synchronously), an ordinary card answer is NOT stale.
    submit(&mut model, "now really run something");
    drain(&mut driver, &mut model);
    submit(&mut model, "/voice");
    model.handle(key(KeyCode::Enter));
    let pending = model.outbox.remove(0);
    driver.consume(
        &mut model,
        driver.control_tag(),
        DemoEvent::Answer {
            origin: pending.origin,
            answer: pending.answer,
        },
    );
    assert_eq!(
        notes(&model),
        1,
        "a live session's card answer lands no stale note"
    );
}

// ---- Guard 5's singles: theme/vfs/dir/voice, each guarded ----

#[test]
fn guarded_singles_restore_theme_vfs_dir_and_voice() {
    // MUTATION CHECK: restore the theme unguarded (`model.theme` from any
    // string) and the unknown-theme case below would need a panic or a
    // default-clobber to satisfy — either way this test fails.
    let mut model = launcher_model();
    submit(&mut model, "some session");
    submit(&mut model, "/clear");
    model.theme = ThemeKey::Dark;
    model.launcher_dir = "~/dev/elsewhere".to_owned();
    model.voice.enabled = false;
    model
        .vfs
        .insert("~/dev/extra".to_owned(), vec!["notes.md".to_owned()]);

    let mut dto: StateDto =
        serde_json::from_str(&serde_json::to_string(&snapshot(&model)).expect("serialize"))
            .expect("parse");
    // The persisted vfs lost a seed key (an older save): the merge heals it.
    dto.vfs.remove("~/dev");
    let mut hydrated = launcher_model();
    let outcome = hydrate(&mut hydrated, dto);
    assert!(outcome.theme_restored);
    assert_eq!(hydrated.theme, ThemeKey::Dark);
    assert_eq!(hydrated.launcher_dir, "~/dev/elsewhere");
    assert!(!hydrated.voice.enabled);
    assert!(
        hydrated.vfs.contains_key("~/dev/extra"),
        "persisted vfs entries survive"
    );
    assert!(
        hydrated.vfs.contains_key("~/dev"),
        "vfs is merged OVER the seed — missing seed keys heal"
    );

    // Unknown theme name → the single stays guarded (seed default kept).
    let mut dto: StateDto =
        serde_json::from_str(&serde_json::to_string(&snapshot(&model)).expect("serialize"))
            .expect("parse");
    dto.theme = "neon".to_owned();
    let mut hydrated = launcher_model();
    let outcome = hydrate(&mut hydrated, dto);
    assert!(!outcome.theme_restored);
    assert_eq!(hydrated.theme, ThemeKey::Dark, "unknown theme → default");
}

// ---- The dump_screens-equivalence check: seeds in, seeds out ----

#[test]
fn seed_snapshot_hydrates_to_a_byte_identical_launcher() {
    // Hydration must be invisible when the state IS the seeds: the
    // launcher renders byte-identically at the review sizes (the owed
    // "does the launcher render differently with hydrated sessions"
    // check — it must not).
    let fresh = launcher_model();
    let dto: StateDto =
        serde_json::from_str(&serde_json::to_string(&snapshot(&fresh)).expect("serialize"))
            .expect("parse");
    let mut hydrated = launcher_model();
    hydrate(&mut hydrated, dto);
    assert_eq!(rows(&fresh, 118, 34), rows(&hydrated, 118, 34));
    assert_eq!(rows(&fresh, 90, 10), rows(&hydrated, 90, 10));
    for id in [1u64, 2, 3] {
        assert_eq!(
            slot(&fresh, id).projection.entries(),
            slot(&hydrated, id).projection.entries(),
            "seed transcript {id}"
        );
    }
}
