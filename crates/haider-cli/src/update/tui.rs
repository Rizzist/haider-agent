//! CLI-owned coordinator for live-TUI update checks and transactions.

use super::check_policy::{
    BackgroundCheckOutcome, CheckReservation, automatic_checks_disabled, background_check,
    reserve_check, unix_timestamp_now,
};
use super::{
    UpdateAvailability, UpdateOptions, UpdateRunOutcome, check_update_availability, run_update,
};
use haider_tui::app::AppEvent;
use haider_tui::runtime::{LiveUpdateBridge, LiveUpdateEvent};
use std::path::PathBuf;
use tokio::sync::mpsc;

/// Build the live host bridge and immediately launch the quiet on-open check.
/// All filesystem, curl, transaction, and daemon work stays outside the TUI
/// reducer and outside its input/render task.
pub(crate) fn live_update_bridge(profile_dir: PathBuf, no_update_check: bool) -> LiveUpdateBridge {
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    spawn_startup_check(profile_dir.clone(), no_update_check, events_tx.clone());

    let check_tx = events_tx.clone();
    let check_profile = profile_dir;
    let check_now = move || {
        let tx = check_tx.clone();
        let profile = check_profile.clone();
        let _task = tokio::task::spawn_blocking(move || {
            if let Err(error) = reserve_check(&profile, unix_timestamp_now(), true) {
                send_app(
                    &tx,
                    AppEvent::UpdateFailed {
                        message: format!("could not record update check: {error}"),
                    },
                );
                return;
            }
            let event = match check_update_availability() {
                Ok(UpdateAvailability::Current { version }) => AppEvent::UpdateCurrent { version },
                Ok(UpdateAvailability::Available { latest, .. }) => {
                    AppEvent::UpdateAvailable { version: latest }
                }
                Err(error) => AppEvent::UpdateFailed {
                    message: error.to_string(),
                },
            };
            send_app(&tx, event);
        });
    };

    let update_tx = events_tx;
    let runtime = tokio::runtime::Handle::current();
    let run_now = move || {
        let tx = update_tx.clone();
        let work_tx = tx.clone();
        let runtime = runtime.clone();
        let spawned = std::thread::Builder::new()
            .name("haider-tui-update".into())
            .spawn(move || {
                // rev933b finding 4: install outcomes carry their own
                // event class so an unrelated CHECK can never clear the
                // install latch or trip the dead-link exit.
                let event = match runtime.block_on(run_update(UpdateOptions { check: false })) {
                    Ok(UpdateRunOutcome::Updated { .. }) => LiveUpdateEvent::Installed,
                    Ok(UpdateRunOutcome::Current { version }) => {
                        LiveUpdateEvent::Install(AppEvent::UpdateCurrent { version })
                    }
                    Ok(UpdateRunOutcome::Available { latest, .. }) => {
                        LiveUpdateEvent::Install(AppEvent::UpdateAvailable { version: latest })
                    }
                    Err(error) => LiveUpdateEvent::Install(AppEvent::UpdateFailed {
                        message: error.to_string(),
                    }),
                };
                let _ = work_tx.send(event);
            });
        if let Err(error) = spawned {
            send_app(
                &tx,
                AppEvent::UpdateFailed {
                    message: format!("could not start update worker: {error}"),
                },
            );
        }
    };

    LiveUpdateBridge::new(events_rx, check_now, run_now)
}

fn spawn_startup_check(
    profile_dir: PathBuf,
    no_update_check: bool,
    events: mpsc::UnboundedSender<LiveUpdateEvent>,
) {
    if automatic_checks_disabled(no_update_check) {
        return;
    }
    let _worker = std::thread::Builder::new()
        .name("haider-update-check".into())
        .spawn(move || {
            let reservation = reserve_check(&profile_dir, unix_timestamp_now(), false);
            if !matches!(reservation, Ok(CheckReservation::Due)) {
                return;
            }
            if let BackgroundCheckOutcome::Available { version } = background_check() {
                send_app(&events, AppEvent::UpdateAvailable { version });
            }
        });
}

fn send_app(events: &mpsc::UnboundedSender<LiveUpdateEvent>, event: AppEvent) {
    let _ = events.send(LiveUpdateEvent::App(event));
}
