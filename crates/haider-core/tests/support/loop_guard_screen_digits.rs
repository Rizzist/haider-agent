//! Real-actor loop-guard laws for numeric screen paging and Unicode decimal
//! counters (Astra final review of request-budget, findings B1/B2; adapted
//! from its `probe/tests/edge_cases.rs`).
// Included from runtime_tests to share its fixtures.
use super::*;

/// `mobile` dispatcher: `swipe` advances a list page and acks; `a11y_tree`
/// returns one row (stable id, resource id and bounds) whose text is the page
/// number, as a numeric ID/price list, or `itemN` in the letters control.
/// `stuck` keeps the page fixed (end of the list).
struct NumericList {
    page: AtomicUsize,
    trees: Mutex<Vec<String>>,
    letters: bool,
    stuck: bool,
}

#[async_trait]
impl ToolDispatcher for NumericList {
    async fn execute(
        &self,
        _run_id: &RunId,
        _item_id: &ItemId,
        _call_id: &str,
        name: &str,
        args: serde_json::Value,
        _cancel: &haider_core::CancelToken,
    ) -> Result<ToolDispatchResult, HaiderError> {
        assert_eq!(name, "mobile");
        let output = if args["action"] == "swipe" {
            if !self.stuck {
                self.page.fetch_add(1, Ordering::SeqCst);
            }
            serde_json::json!("Ack")
        } else {
            assert_eq!(args["action"], "a11y_tree");
            let page = self.page.load(Ordering::SeqCst);
            let text = if self.letters {
                format!("item{page}")
            } else {
                (100_000 + page).to_string()
            };
            serde_json::json!({"A11yTree": [{
                "id": "row1", "text": text, "content_desc": null,
                "class": "android.widget.TextView", "resource_id": "example:id/row",
                "bounds": {"left": 0, "top": 100, "right": 100, "bottom": 150}
            }]})
        };
        // The exact typed output envelope the daemon serializes as preview.
        let typed: haider_protocol::mobile::MobileOutput =
            serde_json::from_value(output).expect("typed mobile output");
        let preview = serde_json::to_string(&typed).expect("preview");
        if args["action"] == "a11y_tree" {
            self.trees.lock().expect("trees").push(preview.clone());
        }
        Ok(ToolDispatchResult::Completed(BoundedResult {
            preview,
            truncated: false,
            truncation: None,
            effects: Vec::new(),
            data: None,
            artifact: None,
            images: Vec::new(),
            cursor: None,
            status: haider_protocol::tool::ToolResultStatus::Completed,
            reason: None,
            presentation: None,
            orchestration: None,
        }))
    }
}

struct Run {
    state: RunState,
    requests: usize,
    error: Option<HaiderError>,
    steers: usize,
}

async fn run(script: Vec<FakeStep>, dispatcher: Option<Arc<dyn ToolDispatcher>>) -> Run {
    let provider = Arc::new(FakeProvider::new(script));
    let store = Arc::new(MemoryStore::new());
    let (actor, handle) =
        HarnessActor::new_with_dispatcher(config(), provider.clone(), store.clone(), dispatcher);
    let task = tokio::spawn(actor.run());
    let outcome = handle
        .submit_turn(SubmitTurn::new("Perform the requested work."))
        .await
        .expect("submit")
        .wait()
        .await
        .expect("outcome");
    let steers = store
        .events(&SessionId::new(SESSION))
        .await
        .iter()
        .filter(|event| {
            matches!(typed(event), EventPayload::Item(ItemEvent::Completed { item, .. })
                if LoopSuspectedV1::from_extension_item(&item).is_some())
        })
        .count();
    handle.stop().await.expect("stop");
    task.await.expect("actor task");
    Run {
        state: outcome.state,
        requests: provider.requests().len(),
        error: outcome.error,
        steers,
    }
}

/// 201 alternating `a11y_tree`/`swipe` calls (101 observations, 100 pages).
async fn pages(letters: bool, stuck: bool) -> (Run, Vec<String>) {
    let dispatcher = Arc::new(NumericList {
        page: AtomicUsize::new(0),
        trees: Mutex::new(Vec::new()),
        letters,
        stuck,
    });
    let mut script = Vec::new();
    for n in 0..201 {
        if n > 0 {
            script.push(FakeStep::ExpectToolResult {
                call_id: format!("page-{}", n - 1),
            });
        }
        let args = if n % 2 == 0 {
            serde_json::json!({"action": "a11y_tree"})
        } else {
            serde_json::json!({"action": "swipe", "from": {"x": 540, "y": 1800}, "to": {"x": 540, "y": 600}})
        };
        script.push(FakeStep::EmitToolCall {
            call_id: format!("page-{n}"),
            name: "mobile".into(),
            args,
        });
        script.push(FakeStep::Finish {
            reason: FinishReason::ToolUse,
        });
    }
    script.push(FakeStep::EmitText {
        text: "PAGING_DONE".into(),
    });
    script.push(FakeStep::Finish {
        reason: FinishReason::EndTurn,
    });
    let result = run(script, Some(dispatcher.clone())).await;
    let trees = dispatcher.trees.lock().expect("trees").clone();
    (result, trees)
}

#[tokio::test]
async fn letter_identifier_a11y_pages_complete() {
    let (run, trees) = pages(true, false).await;
    assert!(trees.windows(2).all(|pair| pair[0] != pair[1]));
    assert_eq!(run.state, RunState::Done, "{:?}", run.error);
    assert_eq!(run.steers, 0);
}

/// B1: pages whose rows differ only by numbers are real screen changes.
#[tokio::test]
async fn productive_numeric_a11y_pages_complete() {
    let (run, trees) = pages(false, false).await;
    assert_eq!(trees.len(), 101);
    assert!(
        trees.windows(2).all(|pair| pair[0] != pair[1]),
        "every observed page must really differ"
    );
    assert_eq!(run.state, RunState::Done, "{:?}", run.error);
    assert_eq!(run.requests, 202);
    assert_eq!(run.steers, 0);
}

/// A truly identical numeric tree (stuck at the list end) is still stopped
/// by the result-level guard with a typed `loop_limit`.
#[tokio::test]
async fn identical_numeric_a11y_tree_still_stops() {
    let (run, trees) = pages(false, true).await;
    assert!(trees.windows(2).all(|pair| pair[0] == pair[1]));
    assert_eq!(run.state, RunState::Errored);
    let error = run.error.expect("loop error");
    assert_eq!(error.code, ErrorCode::LoopLimit);
    assert_eq!(
        error.details.expect("details")["loop"],
        "repeated_tool_calls"
    );
    assert_eq!(run.steers, 1);
    assert!(run.requests < 202, "{}", run.requests);
}

/// 100 `pause_turn` responses whose only change is an attempt counter
/// written with the decimal digits starting at `digit_zero`.
async fn repeated_counter_text(digit_zero: u32) -> Run {
    let mut script = Vec::new();
    for n in 1..=100_u32 {
        let digits: String = n
            .to_string()
            .chars()
            .map(|ch| {
                char::from_u32(digit_zero + ch.to_digit(10).expect("ascii digit")).expect("digit")
            })
            .collect();
        script.push(FakeStep::EmitText {
            text: format!("Retrying the same operation (attempt {digits})."),
        });
        script.push(FakeStep::Finish {
            reason: FinishReason::PauseTurn,
        });
    }
    script.push(FakeStep::EmitText {
        text: "UNREACHABLE_AFTER_LOOP".into(),
    });
    script.push(FakeStep::Finish {
        reason: FinishReason::EndTurn,
    });
    run(script, None).await
}

async fn assert_counter_loop_stops(digit_zero: u32) {
    let run = repeated_counter_text(digit_zero).await;
    assert_eq!(run.state, RunState::Errored, "U+{digit_zero:04X}");
    assert!(run.requests < 20, "U+{digit_zero:04X}: {}", run.requests);
}

#[tokio::test]
async fn ascii_counter_continuations_stop() {
    assert_counter_loop_stops(0x30).await;
}

/// B2: Arabic-Indic digits have no ASCII NFKC form; still a counter.
#[tokio::test]
async fn arabic_indic_counter_continuations_stop() {
    assert_counter_loop_stops(0x660).await;
}

/// B2: Extended Arabic-Indic (Persian) digits.
#[tokio::test]
async fn persian_counter_continuations_stop() {
    assert_counter_loop_stops(0x6f0).await;
}
