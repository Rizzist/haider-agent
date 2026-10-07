//! Test-only ordering barrier after a durable completion attempt.
#![allow(clippy::expect_used)]
use haider_protocol::ids::SessionId;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use tokio::sync::oneshot;
type Barrier = (oneshot::Sender<()>, oneshot::Receiver<()>);
fn barriers() -> &'static Mutex<HashMap<SessionId, Barrier>> {
    static BARRIERS: OnceLock<Mutex<HashMap<SessionId, Barrier>>> = OnceLock::new();
    BARRIERS.get_or_init(|| Mutex::new(HashMap::new()))
}
#[cfg(unix)]
pub(crate) fn install(session: &SessionId) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
    let (arrived_tx, arrived_rx) = oneshot::channel();
    let (resume_tx, resume_rx) = oneshot::channel();
    assert!(
        barriers()
            .lock()
            .expect("barrier lock")
            .insert(session.clone(), (arrived_tx, resume_rx))
            .is_none()
    );
    (arrived_rx, resume_tx)
}
pub(crate) async fn after_attempt(session: &SessionId) {
    let barrier = barriers().lock().expect("barrier lock").remove(session);
    if let Some((arrived, resume)) = barrier {
        let _ = arrived.send(());
        let _ = resume.await;
    }
}
