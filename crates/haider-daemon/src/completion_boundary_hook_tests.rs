//! Test-only ordering barriers for completion and stop admission.
#![allow(clippy::expect_used)]
use haider_protocol::ids::SessionId;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use tokio::sync::oneshot;
type Barrier = (oneshot::Sender<()>, oneshot::Receiver<()>);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum Boundary {
    CompletionAttempt,
    StopOperation,
    StopAdmission,
}

type Barriers = Mutex<HashMap<(Boundary, SessionId), Barrier>>;

fn barriers() -> &'static Barriers {
    static BARRIERS: OnceLock<Barriers> = OnceLock::new();
    BARRIERS.get_or_init(|| Mutex::new(HashMap::new()))
}
#[cfg(unix)]
pub(crate) fn install(session: &SessionId) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
    install_at(Boundary::CompletionAttempt, session)
}

#[cfg(unix)]
pub(crate) fn install_at(
    boundary: Boundary,
    session: &SessionId,
) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
    let (arrived_tx, arrived_rx) = oneshot::channel();
    let (resume_tx, resume_rx) = oneshot::channel();
    assert!(
        barriers()
            .lock()
            .expect("barrier lock")
            .insert((boundary, session.clone()), (arrived_tx, resume_rx))
            .is_none()
    );
    (arrived_rx, resume_tx)
}
pub(crate) async fn after_attempt(session: &SessionId) {
    after_at(Boundary::CompletionAttempt, session).await;
}

pub(crate) async fn after_at(boundary: Boundary, session: &SessionId) {
    let barrier = barriers()
        .lock()
        .expect("barrier lock")
        .remove(&(boundary, session.clone()));
    if let Some((arrived, resume)) = barrier {
        let _ = arrived.send(());
        let _ = resume.await;
    }
}
