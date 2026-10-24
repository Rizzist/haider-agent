use super::*;

fn ids() -> (SessionId, RunId, RunId) {
    (
        SessionId::new("session-fenced"),
        RunId::new("run-stopped"),
        RunId::new("run-later"),
    )
}

/// A fence refuses registrations from captured runs while held
/// and lowers only when the last concurrent stop releases it.
#[test]
fn fence_is_reference_counted_for_owned_runs_and_watches() {
    let fences = MonitorStopFences::default();
    let (session, run, later) = ids();
    let other = SessionId::new("session-other");
    assert!(!fences.registration_fenced(&session, &run));
    let first = fences.fence(
        &session,
        std::slice::from_ref(&run),
        &["owned-watch".into()],
    );
    let second = fences.fence(
        &session,
        std::slice::from_ref(&run),
        &["owned-watch".into()],
    );
    assert!(fences.session_fenced(&session));
    assert!(!fences.registration_fenced(&session, &later));
    assert!(!fences.session_fenced(&other));
    assert!(!fences.registration_fenced(&other, &run));
    assert!(fences.report_fenced(&session, "owned-watch"));
    assert!(!fences.report_fenced(&session, "successor-watch"));
    assert_eq!(fences.epoch(&session, "successor-watch"), 0);
    let owned_epoch = fences.epoch(&session, "owned-watch");
    let independent_stop = fences.fence(
        &session,
        std::slice::from_ref(&later),
        &["later-watch".into()],
    );
    assert!(fences.registration_fenced(&session, &later));
    assert_eq!(
        fences.epoch(&session, "owned-watch"),
        owned_epoch,
        "another stop in the session must not re-epoch this watch"
    );
    drop(independent_stop);
    assert!(!fences.registration_fenced(&session, &later));
    assert!(fences.registration_fenced(&session, &run));
    drop(first);
    assert!(
        fences.session_fenced(&session),
        "second stop still holds it"
    );
    drop(second);
    assert!(!fences.session_fenced(&session));
    assert!(!fences.registration_fenced(&session, &later));
}

/// A run moved to cancelling stays refused after the fence drops, until it
/// is observed terminal; other runs of the session are admitted again.
#[test]
fn stopped_run_outlives_the_fence_until_forgotten() {
    let fences = MonitorStopFences::default();
    let (session, run, later) = ids();
    let fence = fences.fence(
        &session,
        std::slice::from_ref(&run),
        &["owned-watch".into()],
    );
    fences.mark_run_stopped(&session, &run);
    drop(fence);
    assert!(fences.registration_fenced(&session, &run));
    assert!(!fences.registration_fenced(&session, &later));
    assert!(!fences.session_fenced(&session));
    fences.forget_stopped_run(&session, &run);
    assert!(!fences.registration_fenced(&session, &run));
}

#[test]
fn deleted_session_reclaims_unsettled_stopped_runs() {
    let fences = MonitorStopFences::default();
    let (session, run, _) = ids();
    fences.mark_run_stopped(&session, &run);
    assert!(fences.registration_fenced(&session, &run));
    drop(fences.fence(&session, &[], &["deleted-watch".into()]));
    assert_eq!(fences.epoch(&session, "deleted-watch"), 1);
    fences.forget_deleted_session(&session);
    assert!(!fences.registration_fenced(&session, &run));
    assert_eq!(fences.epoch(&session, "deleted-watch"), 0);
}
