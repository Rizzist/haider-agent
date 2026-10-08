//! A settings save belongs to its awaiting run, even when its blocking task
//! continues after that future is dropped. Generations prevent ABA when the
//! same run is selected again. The serial covers durable activation AND memory
//! publication, so a successor or deletion cannot be followed by an old save.
use super::{RunId, SessionHubError, SessionId};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

type Registry = HashMap<(String, SessionId), Weak<SaveSlot>>;
static SLOTS: OnceLock<Mutex<Registry>> = OnceLock::new();
static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
struct Current {
    generation: u64,
    owner: u64,
    run: Option<RunId>,
}

#[derive(Default)]
struct SaveSlot {
    current: Mutex<Current>,
    serial: Mutex<()>,
}

pub(crate) struct SaveOwners {
    owner: u64,
    closed: AtomicBool,
    slots: Mutex<HashMap<SessionId, Arc<SaveSlot>>>,
}

impl Default for SaveOwners {
    fn default() -> Self {
        Self {
            owner: NEXT_OWNER.fetch_add(1, Ordering::Relaxed),
            closed: AtomicBool::new(false),
            slots: Mutex::default(),
        }
    }
}

impl SaveOwners {
    pub(crate) fn begin(
        &self,
        profile: &str,
        session: &SessionId,
        run: &RunId,
    ) -> Result<SaveGuard, SessionHubError> {
        let mut local = super::lock(&self.slots)?;
        if self.closed.load(Ordering::Acquire) {
            return Err(SessionHubError::Closed);
        }
        let slot = if let Some(slot) = local.get(session) {
            Arc::clone(slot)
        } else {
            let mut registry = super::lock(SLOTS.get_or_init(Mutex::default))?;
            registry.retain(|_, slot| slot.strong_count() > 0);
            let key = (profile.to_owned(), session.clone());
            let slot = registry
                .get(&key)
                .and_then(Weak::upgrade)
                .unwrap_or_default();
            registry.insert(key, Arc::downgrade(&slot));
            local.insert(session.clone(), Arc::clone(&slot));
            slot
        };
        let mut current = super::lock(&slot.current)?;
        current.generation = current
            .generation
            .checked_add(1)
            .ok_or_else(|| SessionHubError::Task("settings generation exhausted".to_owned()))?;
        current.owner = self.owner;
        current.run = Some(run.clone());
        let generation = current.generation;
        drop(current);
        Ok(SaveGuard(SavePermit {
            slot,
            session: session.clone(),
            run: run.clone(),
            generation,
            cancelled: Arc::default(),
        }))
    }

    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.fence(None, None);
    }

    pub(crate) fn fence(&self, session: Option<&SessionId>, run: Option<&RunId>) {
        let local = self
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (_, slot) in local
            .iter()
            .filter(|(id, _)| session.is_none_or(|session| session == *id))
        {
            let mut current = slot
                .current
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if current.owner == self.owner
                && run.is_none_or(|run| current.run.as_ref() == Some(run))
            {
                current.run = None;
            }
        }
    }
}

pub(crate) struct SaveGuard(SavePermit);
impl SaveGuard {
    pub(crate) fn permit(&self) -> SavePermit {
        self.0.clone()
    }
}
impl Drop for SaveGuard {
    fn drop(&mut self) {
        // Cancellation and replacement have one ordering point. This lock is
        // held across rename and memory publication, never across Full sync.
        let _current = self
            .0
            .slot
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.0.cancelled.store(true, Ordering::Release);
    }
}

#[derive(Clone)]
pub(crate) struct SavePermit {
    slot: Arc<SaveSlot>,
    session: SessionId,
    run: RunId,
    generation: u64,
    cancelled: Arc<AtomicBool>,
}

impl SavePermit {
    pub(crate) fn ensure_current(&self) -> Result<(), SessionHubError> {
        let current = super::lock(&self.slot.current)?;
        if !self.owns(&current) {
            return Err(self.superseded());
        }
        Ok(())
    }

    pub(crate) fn ensure_for(
        &self,
        session: &SessionId,
        run: &RunId,
    ) -> Result<(), SessionHubError> {
        if self.session != *session || self.run != *run {
            return Err(self.superseded());
        }
        self.ensure_current()
    }

    fn owns(&self, current: &Current) -> bool {
        !self.cancelled.load(Ordering::Acquire)
            && current.generation == self.generation
            && current.run.as_ref() == Some(&self.run)
    }

    fn superseded(&self) -> SessionHubError {
        SessionHubError::SettingsSaveSuperseded {
            session_id: self.session.clone(),
            run_id: self.run.clone(),
            generation: self.generation,
        }
    }

    pub(crate) fn publish<T>(
        &self,
        operation: impl FnOnce() -> Result<T, SessionHubError>,
    ) -> Result<T, SessionHubError> {
        let current = super::lock(&self.slot.current)?;
        if !self.owns(&current) {
            return Err(self.superseded());
        }
        operation()
    }

    pub(crate) fn commit<T>(
        &self,
        operation: impl FnOnce() -> Result<T, SessionHubError>,
    ) -> Result<T, SessionHubError> {
        let _serial = super::lock(&self.slot.serial)?;
        self.ensure_current()?;
        let result = operation();
        self.ensure_current()?;
        result
    }
}

impl crate::lockdown::TurnSaveFence for &SavePermit {
    fn is_current(&self) -> bool {
        self.ensure_current().is_ok()
    }
    fn replace(
        &self,
        operation: impl FnOnce() -> Result<(), crate::lockdown::LockdownError>,
    ) -> Result<(), crate::lockdown::LockdownError> {
        let current =
            self.slot
                .current
                .lock()
                .map_err(|error| crate::lockdown::LockdownError::Task {
                    message: error.to_string(),
                })?;
        if !self.owns(&current) {
            return Err(crate::lockdown::LockdownError::SaveSuperseded);
        }
        operation()
    }
}
