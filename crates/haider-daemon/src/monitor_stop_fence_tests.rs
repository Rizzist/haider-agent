use super::*;

fn ids() -> (SessionId, RunId, RunId) {
    (
        SessionId::new("session-fenced"),
        RunId::new("run-stopped"),
        RunId::new("run-later"),
    )
}

/// A fence refuses registrations from every run of the session while held
/// and lowers only when the last concurrent stop releases it.
#[test]
fn fence_is_reference_counted_per_session() {
    let fences = MonitorStopFences::default();
    let (session, run, later) = ids();
    let other = SessionId::new("session-other");
    assert!(!fences.registration_fenced(&session, &run));
    let first = fences.fence(&session);
    let second = fences.fence(&session);
    assert!(fences.session_fenced(&session));
    assert!(fences.registration_fenced(&session, &later));
    assert!(!fences.session_fenced(&other));
    assert!(!fences.registration_fenced(&other, &run));
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
    let fence = fences.fence(&session);
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
    fences.forget_deleted_session(&session);
    assert!(!fences.registration_fenced(&session, &run));
}
