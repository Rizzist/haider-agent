//! CLI-owned coordinator for live-TUI update checks and transactions.

use super::check_policy::{
    BackgroundCheckOutcome, CheckReservation, automatic_checks_disabled, background_check,
    reserve_check, unix_timestamp_now,
};
use super::{
    UpdateAvailability, UpdateError, UpdateOptions, UpdateRunOutcome,
    check_update_availability_cancellable, run_update,
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
        let _worker = spawn_explicit_check(
            check_profile.clone(),
            check_tx.clone(),
            check_update_availability_cancellable,
        );
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
            // rev933c finding 6: this failure belongs to the INSTALL flow —
            // an App-classed event would leave update_in_progress latched
            // forever.
            let _ = tx.send(LiveUpdateEvent::Install(AppEvent::UpdateFailed {
                message: format!("could not start update worker: {error}"),
            }));
        }
    };

    LiveUpdateBridge::new(events_rx, check_now, run_now)
}

fn spawn_explicit_check(
    profile_dir: PathBuf,
    events: mpsc::UnboundedSender<LiveUpdateEvent>,
    check: impl FnOnce(
        super::discovery::DiscoveryCancellation,
    ) -> Result<UpdateAvailability, UpdateError>
    + Send
    + 'static,
) -> tokio::task::JoinHandle<()> {
    let cancellation_events = events.clone();
    let cancellation = std::sync::Arc::new(move || cancellation_events.is_closed());
    // Retain runtime ownership until discovery has killed/reaped its curl and
    // joined its cancellation watcher. Receiver closure cancels the request.
    tokio::task::spawn_blocking(move || {
        if events.is_closed() {
            return;
        }
        if let Err(error) = reserve_check(&profile_dir, unix_timestamp_now(), true) {
            send_app(
                &events,
                AppEvent::UpdateFailed {
                    message: format!("could not record update check: {error}"),
                },
            );
            return;
        }
        let event = match check(cancellation) {
            Ok(UpdateAvailability::Current { version }) => AppEvent::UpdateCurrent { version },
            Ok(UpdateAvailability::Available { latest, .. }) => {
                AppEvent::UpdateAvailable { version: latest }
            }
            Err(error) => AppEvent::UpdateFailed {
                message: error.to_string(),
            },
        };
        send_app(&events, event);
    })
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

#[cfg(test)]
#[path = "tui_tests.rs"]
mod tests;
