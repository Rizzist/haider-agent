//! Native children remain session-owned across cancellation and result timeouts.
use std::{future::Future, io, process::ExitStatus, sync::Arc, time::Duration};

use haider_platform::{ProcessGroup, ProcessId, ProcessSignal};
use haider_protocol::ids::SessionId;
use tokio::{
    process::Child,
    sync::{OwnedRwLockReadGuard, oneshot},
};

use crate::session_hub::SessionHub;

tokio::task_local! {
    static OWNER: Option<NativeOwner>;
}

#[derive(Clone)]
pub(crate) struct NativeOwner {
    hub: SessionHub,
    session: SessionId,
    activity: Arc<OwnedRwLockReadGuard<()>>,
}

impl NativeOwner {
    pub(crate) fn new(
        hub: &SessionHub,
        session: &SessionId,
        activity: Arc<OwnedRwLockReadGuard<()>>,
    ) -> Self {
        Self {
            hub: hub.clone(),
            session: session.clone(),
            activity,
        }
    }

    pub(crate) async fn scope<T>(owner: Option<Self>, work: impl Future<Output = T>) -> T {
        OWNER.scope(owner, work).await
    }

    fn current() -> Option<Self> {
        OWNER.try_with(Clone::clone).ok().flatten()
    }
}

/// The registry owns the actual join. Dropping a result receiver never drops work.
fn spawn_retained<T: Send + 'static>(
    owner: Option<NativeOwner>,
    work: impl Future<Output = T> + Send + 'static,
) -> (oneshot::Receiver<T>, futures_util::future::AbortHandle) {
    let (send, receive) = oneshot::channel();
    let activity = owner.as_ref().map(|owner| Arc::clone(&owner.activity));
    let task_owner = owner.clone();
    let (abort, cancellation) = futures_util::future::AbortHandle::new_pair();
    let task = tokio::spawn(async move {
        let _activity = activity;
        if let Ok(result) =
            futures_util::future::Abortable::new(NativeOwner::scope(task_owner, work), cancellation)
                .await
        {
            let _ = send.send(result);
        }
    });
    if let Some(owner) = owner {
        owner
            .hub
            .task_registry()
            .track_pipeline(&owner.session, task);
    }
    (receive, abort)
}

/// A cancellable native task has an abort-on-drop result and a retained real join.
pub(crate) struct NativeTask<T> {
    result: Option<oneshot::Receiver<T>>,
    abort: futures_util::future::AbortHandle,
}
impl<T: Send + 'static> NativeTask<T> {
    pub(crate) fn spawn(work: impl Future<Output = T> + Send + 'static) -> Self {
        Self::spawn_owned(NativeOwner::current(), work)
    }
    pub(crate) fn spawn_owned(
        owner: Option<NativeOwner>,
        work: impl Future<Output = T> + Send + 'static,
    ) -> Self {
        let (result, abort) = spawn_retained(owner, work);
        Self {
            result: Some(result),
            abort,
        }
    }
    pub(crate) async fn wait(&mut self) -> io::Result<T> {
        let receive = self
            .result
            .as_mut()
            .ok_or_else(|| io::Error::other("native task result already consumed"))?;
        let result = receive.await.map_err(io::Error::other);
        self.result = None;
        result
    }
    pub(crate) fn abort(&self) {
        self.abort.abort();
    }
}
impl<T> Drop for NativeTask<T> {
    fn drop(&mut self) {
        self.abort.abort();
    }
}

pub(crate) struct NativeProcess {
    child: Option<Child>,
    group: Option<ProcessGroup>,
    pid: ProcessId,
    owner: Option<NativeOwner>,
    cleanup: Option<oneshot::Receiver<io::Result<ExitStatus>>>,
    status: Option<ExitStatus>,
    #[cfg(test)]
    cleanup_gate: Option<oneshot::Receiver<()>>,
}

impl NativeProcess {
    pub(crate) fn register(mut child: Child) -> io::Result<Self> {
        let admission = haider_platform::process_id(child.id())
            .ok_or_else(|| io::Error::other("spawned child has no PID"))
            .and_then(|pid| {
                haider_platform::register_process_group(pid.id()).map(|group| (pid, group))
            });
        match admission {
            Ok((pid, group)) => Ok(Self {
                child: Some(child),
                group: Some(group),
                pid,
                owner: NativeOwner::current(),
                cleanup: None,
                status: None,
                #[cfg(test)]
                cleanup_gate: None,
            }),
            Err(error) => {
                // Even failed native admission keeps leader reap owned.
                let _ = child.start_kill();
                let (_result, _abort) =
                    spawn_retained(NativeOwner::current(), clean_unregistered_child(child));
                Err(error)
            }
        }
    }
    #[cfg(test)]
    pub(crate) fn delay_cleanup(&mut self, gate: oneshot::Receiver<()>) {
        self.cleanup_gate = Some(gate);
    }
    #[cfg(test)]
    pub(crate) fn id(&self) -> u32 {
        self.pid.id()
    }
    pub(crate) fn take_stdin(&mut self) -> Option<tokio::process::ChildStdin> {
        self.child.as_mut().and_then(|child| child.stdin.take())
    }
    pub(crate) fn take_stdout(&mut self) -> Option<tokio::process::ChildStdout> {
        self.child.as_mut().and_then(|child| child.stdout.take())
    }
    pub(crate) fn take_stderr(&mut self) -> Option<tokio::process::ChildStderr> {
        self.child.as_mut().and_then(|child| child.stderr.take())
    }
    pub(crate) fn signal_kill(&mut self) {
        // group is removed before reaping; numeric authority can never be used afterward.
        if let Some(group) = self.group {
            let _ = haider_platform::signal_process_group(group, ProcessSignal::Kill);
        }
    }
    pub(crate) fn leader_exited(&mut self) -> io::Result<bool> {
        #[cfg(unix)]
        {
            haider_platform::process_leader_exited(self.pid)
        }
        #[cfg(windows)]
        {
            self.child
                .as_mut()
                .ok_or_else(|| io::Error::other("native cleanup already started"))?
                .try_wait()
                .map(|status| status.is_some())
        }
    }
    pub(crate) async fn observe_exit(&mut self) -> io::Result<()> {
        #[cfg(unix)]
        {
            observe_native_leader_exit(self.pid).await
        }
        #[cfg(windows)]
        {
            self.child
                .as_mut()
                .ok_or_else(|| io::Error::other("native cleanup already started"))?
                .wait()
                .await
                .map(|_| ())
        }
    }
    fn start_cleanup(&mut self) {
        // Preserve immediate best-effort termination even if forced runtime
        // shutdown never polls the transferred cleanup task. The unreaped
        // child still pins Unix authority here; acknowledgement awaits the join.
        self.signal_kill();
        let Some(child) = self.child.take() else {
            return;
        };
        let group = self.group.take();
        let pid = self.pid;
        #[cfg(test)]
        let cleanup_gate = self.cleanup_gate.take();
        let (result, _abort) = spawn_retained(self.owner.take(), async move {
            #[cfg(test)]
            if let Some(gate) = cleanup_gate {
                let _ = gate.await;
            }
            if let Some(group) = group {
                clean_child(child, group, pid).await
            } else {
                clean_unregistered_child(child).await;
                Err(io::Error::other(
                    "native child lost its process group authority",
                ))
            }
        });
        self.cleanup = Some(result);
    }
    pub(crate) async fn finish(&mut self) -> io::Result<ExitStatus> {
        if let Some(status) = self.status {
            return Ok(status);
        }
        self.start_cleanup();
        let status = self
            .cleanup
            .as_mut()
            .ok_or_else(|| io::Error::other("native cleanup result unavailable"))?
            .await
            .map_err(|error| io::Error::other(format!("native cleanup stopped: {error}")))??;
        self.status = Some(status);
        Ok(status)
    }
}

impl Drop for NativeProcess {
    fn drop(&mut self) {
        self.start_cleanup();
    }
}

// Unix admission is purely numeric validation. Windows admission starts from a
// suspended child; failed assignment/resume drops the kill-on-close Job. Keep
// even this leader-only failure path owned until the Child actually reaps.
async fn clean_unregistered_child(mut child: Child) {
    loop {
        match child.kill().await {
            Ok(()) => return,
            Err(error) => tracing::warn!(%error, "unregistered native child reap will retry"),
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// The platform's macOS convenience observer starts an unjoined native thread.
// Keep this session-owned wait on its existing async task instead. Backoff has
// the same one-millisecond cap as the platform's non-reaping polling fallback;
// cancellation drops only a timer and never an OS observer thread/descriptor.
#[cfg(unix)]
async fn observe_native_leader_exit(pid: ProcessId) -> io::Result<()> {
    let mut delay = Duration::from_micros(50);
    while !haider_platform::process_leader_exited(pid)? {
        tokio::time::sleep(delay).await;
        delay = delay.saturating_mul(2).min(Duration::from_millis(1));
    }
    Ok(())
}

async fn clean_child(
    mut child: Child,
    group: ProcessGroup,
    pid: ProcessId,
) -> io::Result<ExitStatus> {
    // On Unix, do not poll Child::wait/try_wait before the sweep: an unreaped
    // leader pins the numeric PGID even when every descendant has exited.
    let _ = haider_platform::signal_process_group(group, ProcessSignal::Kill);
    #[cfg(unix)]
    loop {
        match observe_native_leader_exit(pid).await {
            Ok(()) => break,
            Err(error) => tracing::warn!(%error, "native leader observation will retry"),
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    #[cfg(windows)]
    let _ = pid;
    loop {
        #[cfg(target_os = "macos")]
        let complete = haider_platform::process_group_contains_only_leader(group, pid);
        #[cfg(not(target_os = "macos"))]
        let complete = haider_platform::process_group_exists(group).map(|exists| !exists);
        match complete {
            Ok(true) => break,
            Ok(false) => {
                // A signal is a termination request, never the native exit
                // witness. In particular Darwin EPERM can race file teardown.
                let _ = haider_platform::signal_process_group(group, ProcessSignal::Kill);
            }
            Err(error) => tracing::warn!(%error, "native group observation will retry"),
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // No more numeric probes or signals are permitted beyond this point.
    haider_platform::release_process_group(group);
    loop {
        match child.wait().await {
            Ok(status) => return Ok(status),
            Err(error) => tracing::warn!(%error, "native child reap will retry"),
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(all(test, unix))]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    #[tokio::test]
    async fn leader_observation_keeps_identity_pinned_until_sweep() {
        let mut command = tokio::process::Command::new("/bin/sh");
        command.args(["-c", "exit 7"]).kill_on_drop(true);
        haider_platform::configure_process_group(&mut command);
        let mut process =
            NativeProcess::register(command.spawn().expect("owned child")).expect("group");
        let pid = process.pid;
        process.observe_exit().await.expect("observe without reap");
        assert!(
            haider_platform::process_leader_exited(pid)
                .expect("leader is still our waitable child")
        );
        assert!(
            process.child.as_ref().and_then(Child::id).is_some(),
            "Tokio child has not been reaped"
        );
        let status = process.finish().await.expect("safe sweep then reap");
        assert_eq!(status.code(), Some(7));
        assert!(
            process.group.is_none(),
            "no numeric authority survives reap"
        );
        process.signal_kill(); // Inert after reap, including a hypothetically recycled PGID.
        assert_eq!(process.finish().await.expect("cached witness"), status);
    }
}
