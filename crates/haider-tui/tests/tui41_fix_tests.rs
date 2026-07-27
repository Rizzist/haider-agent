//! TUI4.1 — the TUI4-arc review's fix round, pinned. Two P1 classes, each
//! written as the reviewer's own reproduction:
//!
//! - **P1-1 strict hydration** (`demo_store.rs`): the sim's load throws
//!   before `setSessions`, so ANY structural surprise preserves the seeds
//!   WHOLE. The port used to default per field, so a foreign-version
//!   payload hydrated as if it were ours and `{"sessions":[{"id":99}]}`
//!   replaced all three seeds with one blank session.
//! - **P1-2 monotonic identity** (`app.rs`, `runtime.rs`): `/reset` used to
//!   rewind `next_session_id`, so a replacement session could wear a dead
//!   session's id — and the control-tagged auto-title micro-call, which
//!   SURVIVES `/reset` by design (the sim's bare `setTimeout`), retitled
//!   the replacement. The sim's `s-${Date.now()}` ids never recur;
//!   monotonicity ports that law and makes the whole class unrepresentable.
#![allow(clippy::expect_used)]

use haider_tui::app::AppModel;
use haider_tui::demo_store::{DEMO_STORE_VERSION, DemoStore, hydrate, snapshot};
use haider_tui::projection::TranscriptEntry;
use haider_tui::script::DemoEvent;

mod common;
use common::{drain, driver_for, launcher_model, submit};

/// A model carrying one real user session on top of the three seeds — the
/// shape a live demo actually persists.
fn model_with_user_session() -> AppModel {
    let mut model = launcher_model();
    submit(&mut model, "persisted user work");
    model
}

fn store_at(dir: &std::path::Path) -> DemoStore {
    DemoStore::at(dir.join("demo-tui-state.json"))
}

// ---- P1-1: strict hydration — all-or-nothing, back to seeds ----

#[test]
fn a_foreign_version_rejects_the_whole_file_back_to_seeds() {
    // The reviewer's exact reproduction: a payload that is VALID in every
    // other respect, carrying `"version":999`. Serde ignores unknown
    // fields, so without the discriminator check the file hydrated as if
    // it were ours AND was rewritten without its version — silent,
    // lossy adoption of a foreign format.
    //
    // MUTATION CHECK: drop the `dto.version != DEMO_STORE_VERSION` guard in
    // `DemoStore::load` and the foreign payload loads (the positive control
    // below proves the payload is otherwise perfectly good, so the guard is
    // the ONLY thing rejecting it).
    let dir = tempfile::tempdir().expect("tempdir");
    let store = store_at(dir.path());
    let model = model_with_user_session();
    let ours = serde_json::to_string(&snapshot(&model)).expect("serialize");

    // Positive control: at OUR version the very same payload loads.
    std::fs::write(store.path(), &ours).expect("write");
    assert!(
        store.load().is_some(),
        "control: the payload is valid at our version"
    );

    let stamp = format!("\"version\":{DEMO_STORE_VERSION}");
    assert!(ours.contains(&stamp), "the version rides on disk");
    let foreign = ours.replace(&stamp, "\"version\":999");
    std::fs::write(store.path(), &foreign).expect("write");
    assert!(
        store.load().is_none(),
        "a foreign version rejects the WHOLE file — never a partial adoption"
    );

    // …and the CONTRAST is what makes the rejection matter, boot-path
    // shaped (main.rs hydrates only on `Some`): the good payload really
    // does replace the model's rows, so a payload that loaded would have
    // taken the seeds with it. The foreign one never reaches hydrate, so
    // the three seeds stand untouched.
    let mut adopted = launcher_model();
    assert_eq!(adopted.sessions.len(), 3, "a fresh model carries 3 seeds");
    std::fs::write(store.path(), &ours).expect("write");
    let good = store.load().expect("the good payload loads");
    hydrate(&mut adopted, good);
    assert_eq!(
        adopted.sessions.len(),
        4,
        "a payload that LOADS rewrites the session list wholesale — which is \
         exactly what the foreign one must never be allowed to do"
    );
}

#[test]
fn a_structurally_partial_session_rejects_the_whole_file() {
    // The reviewer's second reproduction: `{"sessions":[{"id":99}]}` used
    // to become ONE BLANK SESSION replacing all three seeds, because every
    // field but `id` carried `#[serde(default)]`. The sim's
    // `s.branches.map(…)` throws on that shape, so the catch preserves the
    // seeds — all-or-nothing, never per-field defaulting.
    //
    // MUTATION CHECK: restore `#[serde(default)]` on `SessionDto`'s
    // structural fields (dir/model_short/device/ago/branches/turns_offset/
    // projection/chips — `name`/`title` are `Option`, which serde already
    // tolerates, so they are not what holds this line) and the versioned
    // partial below parses into one blank session: `load` returns Some and
    // the seeds die.
    let dir = tempfile::tempdir().expect("tempdir");
    let store = store_at(dir.path());

    // Literal reviewer payload (no version at all).
    std::fs::write(store.path(), r#"{"sessions":[{"id":99}]}"#).expect("write");
    assert!(
        store.load().is_none(),
        "the reviewer's partial payload → seeds, NOT one blank session"
    );

    // The same partial AT OUR VERSION: this is the strict-shape guard
    // proving itself, with the version discriminator satisfied.
    std::fs::write(
        store.path(),
        format!(r#"{{"version":{DEMO_STORE_VERSION},"sessions":[{{"id":99}}]}}"#),
    )
    .expect("write");
    assert!(
        store.load().is_none(),
        "a session missing its required shape rejects the whole file, at any version"
    );

    // Within a version an unknown key is tampering, not tolerance.
    let model = model_with_user_session();
    let ours = serde_json::to_string(&snapshot(&model)).expect("serialize");
    let tampered = ours.replacen('{', r#"{"smuggled":true,"#, 1);
    std::fs::write(store.path(), &tampered).expect("write");
    assert!(
        store.load().is_none(),
        "deny_unknown_fields: an unknown root key rejects the file"
    );
}

#[test]
fn a_persisted_session_id_0_rejects_the_file() {
    // Fable D3-3: 0 is the scratch-lineage sentinel — the driver drops
    // `Session(0)`-owned events while a session is attached (runtime.rs),
    // so a hydrated id-0 session is a session whose turns silently vanish.
    // A corrupt or hand-edited file must never mint one.
    //
    // MUTATION CHECK: drop the `session.id == 0` clause in `DemoStore::load`
    // and the id-0 payload loads (the positive control proves it is
    // otherwise identical to a good file).
    let dir = tempfile::tempdir().expect("tempdir");
    let store = store_at(dir.path());
    let model = model_with_user_session();
    let mut dto = snapshot(&model);

    std::fs::write(
        store.path(),
        serde_json::to_string(&dto).expect("serialize"),
    )
    .expect("write");
    assert!(store.load().is_some(), "control: the good file loads");

    dto.sessions[0].id = 0;
    std::fs::write(
        store.path(),
        serde_json::to_string(&dto).expect("serialize"),
    )
    .expect("write");
    assert!(
        store.load().is_none(),
        "a persisted id 0 collides with the scratch sentinel → seeds"
    );
}

// ---- P1-2: monotonic identity — a dead id never comes back ----

#[tokio::test(start_paused = true)]
async fn a_dead_sessions_auto_title_never_lands_on_its_replacement() {
    // The reviewer's PTY reproduction, headless and exact:
    //   `zzz old epoch leak` → immediate `/reset` → `fresh replacement`
    // used to print `· session titled — “Zzz old epoch leak”` INSIDE the
    // replacement, because `/reset` rewound `next_session_id` and the
    // control-tagged micro-call (keyed by session id, uncancelled by
    // design) found a live session wearing the dead id.
    //
    // MUTATION CHECK: restore `self.next_session_id = 4;` in the reducer's
    // `"reset"` arm (app.rs) and the replacement is minted with the DEAD id
    // — the identity assert below fires immediately, and the stale title
    // then lands on the live surface (the launcher-row assert is the
    // background-row half of the law and does not participate in this
    // flow, since an attached session's title lives on the model until
    // checkin).
    let mut model = launcher_model();
    let (mut driver, mut rx) = driver_for(&model);

    submit(&mut model, "zzz old epoch leak");
    let dead = model.active_session.expect("the first session attached");
    // Dispatch spawns the 1.5 s auto-title micro-call keyed `origin: dead`.
    drain(&mut driver, &mut model);

    submit(&mut model, "/reset");
    drain(&mut driver, &mut model);
    submit(&mut model, "fresh replacement");
    let replacement = model.active_session.expect("the replacement attached");
    drain(&mut driver, &mut model);
    assert_ne!(
        replacement, dead,
        "monotonic identity: a replacement session NEVER wears a dead id"
    );

    // Pump the REAL channel until the dead session's micro-call arrives.
    // Nothing here is hand-built: this is what PROVES the callback survived
    // `/reset`'s ResetAllSessions cancel and still carries the dead id.
    // MUTATION CHECK: arm the auto-title under `ArmOwner::Session(active)`
    // instead of the control arm (runtime.rs `handle_request`) and
    // ResetAllSessions cancels it — this loop starves and panics.
    let control = driver.control_tag();
    let mut stale = None;
    for _ in 0..10_000 {
        let (tag, event) = tokio::time::timeout(std::time::Duration::from_secs(3600), rx.recv())
            .await
            .expect("the driver went silent before the micro-call returned")
            .expect("channel open");
        if matches!(&event, DemoEvent::AutoTitle { origin, .. } if *origin == dead) {
            assert_eq!(
                tag, control,
                "the micro-call rides the never-cancelled control arm"
            );
            stale = Some(event);
            break;
        }
        driver.consume(&mut model, tag, event);
        drain(&mut driver, &mut model);
    }
    let stale = stale.expect("the dead session's auto-title micro-call must still fire");
    driver.consume(&mut model, control, stale);

    let ghost = haider_tui::app::auto_blurb("zzz old epoch leak");
    assert_ne!(
        model.session_title.as_deref(),
        Some(ghost.as_str()),
        "the dead session's blurb must not title the replacement"
    );
    assert!(
        !model.projection.entries().iter().any(|entry| matches!(
            entry,
            TranscriptEntry::Note { text } if text.contains(&ghost)
        )),
        "…and its `· session titled` note must not enter the replacement's transcript"
    );
    assert!(
        model
            .sessions
            .iter()
            .all(|slot| slot.title.as_deref() != Some(ghost.as_str())),
        "…nor any launcher row: the dead id resolves to no session at all"
    );
}

#[test]
fn the_session_id_allocator_never_rewinds() {
    // The law behind the repro above, stated directly: `next_session_id` is
    // monotonic for the PROCESS lifetime — neither `/reset` nor a hydrate
    // carrying older ids may move it backwards.
    //
    // MUTATION CHECK: re-add `self.next_session_id = 4;` to the `"reset"`
    // arm and the post-reset assert fails; weaken hydrate's
    // `next_session_id.max(max_id + 1)` to a plain assignment and the
    // hydrate assert fails.
    let mut model = launcher_model();
    submit(&mut model, "first user session");
    let first = model.active_session.expect("attached");

    submit(&mut model, "/reset");
    assert!(
        model.next_session_id > first,
        "/reset reseeds the rows but NEVER rewinds the allocator"
    );

    submit(&mut model, "second user session");
    let second = model.active_session.expect("attached");
    assert!(
        second > first,
        "the session after a reset takes a brand-new identity"
    );

    // A hydrate whose ids are all BELOW the live counter may only push it
    // forward (guard 2 is a `max`, not an assignment).
    let before = model.next_session_id;
    let mut dto = snapshot(&launcher_model());
    for (offset, session) in dto.sessions.iter_mut().enumerate() {
        session.id = 1 + offset as u64;
    }
    let mut fresh = model;
    hydrate(&mut fresh, dto);
    assert!(
        fresh.next_session_id >= before,
        "hydrating older ids never rewinds the allocator either"
    );
}
