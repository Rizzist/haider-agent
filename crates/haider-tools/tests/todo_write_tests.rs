//! G1 validation laws (L2) for the whole-list-replace `todo_write` surface.
#![allow(clippy::expect_used)]

use haider_protocol::history::TodoState;
use haider_protocol::tool::DispatchMode;
use haider_tools::{MAX_TODO_ITEMS, TodoWrite, todo_write_manifest};

fn item(id: u32, text: &str, state: &str) -> serde_json::Value {
    serde_json::json!({ "id": id, "text": text, "state": state })
}

/// MUTATION CHECK: broker `todo_write` or make it deferred. Expected runtime
/// failure: the manifest stops matching the request_input-pattern contract
/// (no effects, awaited) that keeps planning outside the permission broker.
#[test]
fn todo_write_manifest_is_awaited_and_effect_free() {
    let manifest = todo_write_manifest();
    assert_eq!(manifest.name, "todo_write");
    assert_eq!(manifest.dispatch, DispatchMode::Await);
    assert!(manifest.effects.is_empty());
    assert_eq!(
        manifest.input_schema["required"],
        serde_json::json!(["items"])
    );
    assert_eq!(
        manifest.input_schema["properties"]["items"]["items"]["required"],
        serde_json::json!(["id", "text", "state"])
    );
}

/// L2 law: duplicate ids are rejected.
/// MUTATION CHECK: drop the unique-id insert check. Expected runtime failure:
/// two todos sharing id 1 validate, breaking update-by-id addressing.
#[test]
fn duplicate_ids_are_rejected() {
    let error = TodoWrite::from_tool_args(serde_json::json!({
        "items": [item(1, "first", "listed"), item(1, "second", "listed")]
    }))
    .expect_err("duplicate ids must not validate");
    assert!(error.to_string().contains("duplicate todo id 1"));
}

/// L2 law: cyclic deps (including a self-dep) are rejected.
/// MUTATION CHECK: stop walking the dep chain. Expected runtime failure: the
/// a→b→a cycle below validates and would render as forever-blocked todos.
#[test]
fn cyclic_deps_are_rejected() {
    let error = TodoWrite::from_tool_args(serde_json::json!({
        "items": [
            { "id": 1, "text": "a", "state": "listed", "dep": 2 },
            { "id": 2, "text": "b", "state": "listed", "dep": 1 },
        ]
    }))
    .expect_err("dependency cycle must not validate");
    assert!(error.to_string().contains("dependency cycle"));
    let error = TodoWrite::from_tool_args(serde_json::json!({
        "items": [{ "id": 7, "text": "self", "state": "listed", "dep": 7 }]
    }))
    .expect_err("self-dependency must not validate");
    assert!(error.to_string().contains("dependency cycle"));
}

/// L2 law: more than 50 items are rejected.
/// MUTATION CHECK: drop the item-count bound. Expected runtime failure: a
/// 51-item list validates and floods the pinned panel.
#[test]
fn fifty_one_items_are_rejected() {
    let items: Vec<_> = (0..=MAX_TODO_ITEMS as u32)
        .map(|id| item(id, "todo", "listed"))
        .collect();
    assert_eq!(items.len(), MAX_TODO_ITEMS + 1);
    let error = TodoWrite::from_tool_args(serde_json::json!({ "items": items }))
        .expect_err("51 items must not validate");
    assert!(error.to_string().contains("at most 50"));
    let items: Vec<_> = (0..MAX_TODO_ITEMS as u32)
        .map(|id| item(id, "todo", "listed"))
        .collect();
    TodoWrite::from_tool_args(serde_json::json!({ "items": items })).expect("50 items validate");
}

/// L2 law: the Claude Code `TodoWrite` vocabulary is accepted and normalized.
/// MUTATION CHECK: drop any key-repair branch (todos→items, content→text,
/// status→state, pending/in_progress mapping, positional ids). Expected
/// runtime failure: this canonical CC payload stops parsing or normalizing.
#[test]
fn claude_code_vocabulary_is_accepted_and_normalized() {
    let request = TodoWrite::from_tool_args(serde_json::json!({
        "todos": [
            { "content": "explore the repo", "status": "completed", "activeForm": "Exploring the repo" },
            { "content": "write the fix", "status": "in_progress", "activeForm": "Writing the fix" },
            { "content": "run the tests", "status": "pending", "activeForm": "Running the tests" },
        ]
    }))
    .expect("CC-vocabulary payload is repaired, not rejected");
    assert_eq!(request.items.len(), 3);
    assert_eq!(
        request.items.iter().map(|item| item.id).collect::<Vec<_>>(),
        vec![0, 1, 2],
        "ids are assigned by position when every item omits one"
    );
    assert_eq!(request.items[0].text, "explore the repo");
    assert_eq!(request.items[0].state, TodoState::Completed);
    assert_eq!(request.items[1].state, TodoState::Processing);
    assert_eq!(request.items[2].state, TodoState::Listed);
    assert!(request.items.iter().all(|item| item.dep.is_none()));
}

/// Canonical keys always win over repair aliases, and a MIXED id presence is
/// ambiguous — rejected rather than silently renumbered.
#[test]
fn canonical_keys_win_and_mixed_id_presence_is_rejected() {
    let request = TodoWrite::from_tool_args(serde_json::json!({
        "items": [{ "id": 3, "text": "real", "content": "shadowed", "state": "listed", "status": "completed" }]
    }))
    .expect("canonical keys validate");
    assert_eq!(request.items[0].text, "real");
    assert_eq!(request.items[0].state, TodoState::Listed);
    let error = TodoWrite::from_tool_args(serde_json::json!({
        "items": [
            { "id": 1, "text": "has id", "state": "listed" },
            { "text": "missing id", "state": "listed" },
        ]
    }))
    .expect_err("mixed id presence must not validate");
    assert!(error.to_string().contains("all carry an `id` or all omit"));
}

/// Empty list is VALID (it clears the plan); text bounds and unknown dep
/// targets are enforced; `pending` under the canonical `state` key repairs.
#[test]
fn empty_list_validates_and_text_and_dep_bounds_hold() {
    let request =
        TodoWrite::from_tool_args(serde_json::json!({ "items": [] })).expect("empty list is valid");
    assert!(request.items.is_empty());
    assert!(
        !request.all_completed(),
        "an empty list is a clear, not a completed plan"
    );
    assert!(
        TodoWrite::from_tool_args(serde_json::json!({
            "items": [item(1, "  ", "listed")]
        }))
        .is_err()
    );
    assert!(
        TodoWrite::from_tool_args(serde_json::json!({
            "items": [item(1, &"x".repeat(501), "listed")]
        }))
        .is_err()
    );
    assert!(
        TodoWrite::from_tool_args(serde_json::json!({
            "items": [{ "id": 1, "text": "orphan dep", "state": "listed", "dep": 9 }]
        }))
        .is_err()
    );
    let request = TodoWrite::from_tool_args(serde_json::json!({
        "items": [item(1, "repaired", "pending")]
    }))
    .expect("CC state value under canonical key repairs");
    assert_eq!(request.items[0].state, TodoState::Listed);
}

/// The result echo is the compact `{ok, counts}` shape the model replays.
/// MUTATION CHECK: miscount a state bucket. Expected runtime failure: the
/// counts below stop matching the item states.
#[test]
fn result_echo_counts_states() {
    let request = TodoWrite::from_tool_args(serde_json::json!({
        "items": [
            item(1, "done", "completed"),
            item(2, "doing", "processing"),
            item(3, "later", "listed"),
            item(4, "also done", "completed"),
        ]
    }))
    .expect("valid request");
    assert_eq!(
        request.result_echo(),
        serde_json::json!({
            "ok": true,
            "counts": { "listed": 1, "processing": 1, "completed": 2 }
        })
    );
    assert!(!request.all_completed());
    let request = TodoWrite::from_tool_args(serde_json::json!({
        "items": [item(1, "done", "completed")]
    }))
    .expect("valid request");
    assert!(request.all_completed());
}
