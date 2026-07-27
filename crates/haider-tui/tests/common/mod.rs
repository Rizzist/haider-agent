//! Shared paused-time driver harness — the TUI3 review's MUST (extracted
//! TUI4.1, Fable D2-4). Helpers moved verbatim; every driver is built
//! through driver_for so the one-honour-roll law (D2-2) is wired by
//! construction.
#![allow(dead_code)]
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::state::HarnessStatus;
use haider_tui::app::{AppEvent, AppModel, AppRequest};
use haider_tui::runtime::DemoDriver;
use haider_tui::script::DemoEvent;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub fn key(code: KeyCode) -> AppEvent {
    AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

pub fn launcher_model() -> AppModel {
    let mut model = AppModel::new();
    model.handle(AppEvent::Envelope(Box::new(EventPayload::HarnessStatus(
        HarnessStatus::Ready,
    ))));
    model
}

pub fn submit(model: &mut AppModel, text: &str) {
    for c in text.chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
}

pub fn drain(driver: &mut DemoDriver, model: &mut AppModel) {
    let requests: Vec<AppRequest> = model.requests.drain(..).collect();
    for request in requests {
        driver.handle_request(model, request);
    }
}

pub async fn pump_until(
    driver: &mut DemoDriver,
    rx: &mut tokio::sync::mpsc::Receiver<(u64, DemoEvent)>,
    model: &mut AppModel,
    what: &str,
    stop: impl Fn(&AppModel) -> bool,
) {
    drain(driver, model);
    for _ in 0..400_000 {
        if stop(model) {
            return;
        }
        let (generation, event) =
            tokio::time::timeout(std::time::Duration::from_secs(3600), rx.recv())
                .await
                .unwrap_or_else(|_| panic!("pump_until({what}): the driver went silent"))
                .expect("channel open");
        driver.consume(model, generation, event);
        drain(driver, model);
    }
    panic!("pump_until({what}): condition never satisfied");
}

pub fn driver_for(model: &AppModel) -> (DemoDriver, tokio::sync::mpsc::Receiver<(u64, DemoEvent)>) {
    DemoDriver::new(64, std::sync::Arc::clone(&model.roster))
}
