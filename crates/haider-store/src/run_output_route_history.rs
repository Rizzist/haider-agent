//! Field knowledge for old run routes. Unknown selectors are never default selectors.
use haider_protocol::{
    error::HaiderError,
    session::{
        ModelSelected, RunOutputProviderRouteV1, RunOutputRouteSnapshotV1, SessionMetadataV1,
        SessionProviderRebound, run_route_history_unavailable,
    },
};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Unknown {
    account: bool,
    base: bool,
    rebind: bool,
    observation: bool,
    epoch: bool,
    reset: bool,
}

/// A transient reconstruction, never a persisted partial snapshot. Only a
/// complete value can be restored; later historical witnesses may resolve it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutputRouteHistory {
    route: RunOutputRouteSnapshotV1,
    unknown: Unknown,
}

impl RunOutputRouteHistory {
    #[must_use]
    pub fn known(route: RunOutputRouteSnapshotV1) -> Self {
        Self {
            route,
            unknown: Unknown::default(),
        }
    }

    #[must_use]
    pub fn from_creation(metadata: &SessionMetadataV1) -> Self {
        let mut history = Self::known(RunOutputRouteSnapshotV1::capture(metadata));
        // An old creation record without epoch fields cannot prove the epoch
        // a later worker stamped before dispatch without journaling it.
        history.unknown.epoch = metadata.selection_epoch.is_none();
        history.unknown.reset = metadata.route_reset_epoch.is_none();
        history
    }

    #[must_use]
    pub fn legacy_pin(route: RunOutputProviderRouteV1) -> Self {
        let observations_recorded = (route.selection_epoch.is_some()
            && route.reset_epoch.is_some())
            || route.resolved_seen
            || route.resolved_alias.is_some();
        let mut history = Self::known(route.into());
        history.unknown.epoch = history.route.selection_epoch.is_none();
        history.unknown.reset = history.route.reset_epoch.is_none();
        history.unknown.observation = !observations_recorded;
        history
    }

    #[must_use]
    pub fn missing() -> Self {
        Self {
            route: RunOutputRouteSnapshotV1 {
                rebind_id: None,
                base_url: None,
                account: None,
                resolved_alias: None,
                resolved_seen: false,
                selection_epoch: None,
                reset_epoch: None,
            },
            unknown: Unknown {
                account: true,
                base: true,
                rebind: true,
                observation: true,
                epoch: true,
                reset: true,
            },
        }
    }

    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.unknown == Unknown::default()
    }

    pub fn require_snapshot(&self) -> Result<&RunOutputRouteSnapshotV1, HaiderError> {
        if self.is_complete() {
            Ok(&self.route)
        } else {
            Err(run_route_history_unavailable(
                "historical account, endpoint, observation or epoch has no complete witness",
            ))
        }
    }

    pub fn restore(&self, metadata: &mut SessionMetadataV1) -> Result<(), HaiderError> {
        self.require_snapshot()?.restore(metadata);
        Ok(())
    }

    /// Legacy child-create receipts digest selectors, not parent observations.
    pub fn selectors(&self) -> Result<[Option<String>; 3], HaiderError> {
        if self.unknown.account || self.unknown.base || self.unknown.rebind {
            return Err(run_route_history_unavailable(
                "legacy inherited selectors have no historical witness",
            ));
        }
        Ok([
            self.route.account.clone(),
            self.route.base_url.clone(),
            self.route.rebind_id.clone(),
        ])
    }

    #[must_use]
    pub fn matches_rebind(&self, rebind: Option<&str>) -> bool {
        !self.unknown.rebind && self.route.rebind_id.as_deref() == rebind
    }

    #[must_use]
    pub fn rebind_unknown(&self) -> bool {
        self.unknown.rebind
    }

    /// Merge only fields actually recorded by a run's older pin. The history
    /// underneath has already been cut at that pin's boundary.
    pub fn overlay(&mut self, pin: Self) {
        if !pin.unknown.account {
            self.route.account = pin.route.account;
            self.unknown.account = false;
        }
        if !pin.unknown.base {
            self.route.base_url = pin.route.base_url;
            self.unknown.base = false;
        }
        if !pin.unknown.rebind {
            self.route.rebind_id = pin.route.rebind_id;
            self.unknown.rebind = false;
        }
        if !pin.unknown.observation {
            self.route.resolved_alias = pin.route.resolved_alias;
            self.route.resolved_seen = pin.route.resolved_seen;
            self.unknown.observation = false;
        }
        if !pin.unknown.epoch {
            self.route.selection_epoch = pin.route.selection_epoch;
            self.unknown.epoch = false;
        }
        if !pin.unknown.reset {
            self.route.reset_epoch = pin.route.reset_epoch;
            self.unknown.reset = false;
        }
    }

    pub fn record(&mut self, route: RunOutputRouteSnapshotV1) {
        *self = Self::known(route);
    }

    #[must_use]
    pub fn epoch_matches(&self, epoch: Option<u64>) -> bool {
        !self.unknown.epoch && self.route.selection_epoch.unwrap_or(0) == epoch.unwrap_or(0)
    }

    /// Resolution can observe an account or advance its served epoch; it
    /// cannot change selectors or supply a missing boundary/reset epoch.
    pub fn record_resolution(
        &mut self,
        route: RunOutputRouteSnapshotV1,
        tainted_legacy_checkpoint: bool,
    ) {
        if self.unknown.account
            || self.unknown.base
            || self.unknown.rebind
            || self.unknown.epoch
            || self.unknown.reset
            || !route.resolved_seen
            || route.selection_epoch.is_none()
            || self.route.rebind_id != route.rebind_id
            || self.route.base_url != route.base_url
            || self.route.account != route.account
            || self.route.reset_epoch != route.reset_epoch
        {
            return;
        }
        if tainted_legacy_checkpoint
            && let (Some(current), Some(next)) = (self.route.selection_epoch, route.selection_epoch)
        {
            // An older recovery can mistake the next turn's seen account for
            // its own, then mint an epoch while resolving its unchanged alias.
            // A scoped fixed-writer fact remains authoritative (including H1
            // coalescence); only a contaminated legacy frame needs this proof.
            let rotating = self.route.resolved_seen
                && !self.unknown.observation
                && self.route.resolved_alias != route.resolved_alias;
            if (rotating && next <= current) || (!rotating && next != current) {
                return;
            }
        }
        self.record(route);
    }

    pub fn record_switch_observation(&mut self, alias: Option<String>) {
        // A switch target was already resolved. An old fact that omits its
        // account cannot prove that the run had never observed an account.
        self.unknown.observation = alias.is_none();
        self.route.resolved_seen = alias.is_some();
        self.route.resolved_alias = alias;
    }

    pub fn clear_selectors(&mut self) {
        self.route.clear_selectors();
        self.unknown.account = false;
        self.unknown.base = false;
        self.unknown.rebind = false;
        self.unknown.observation = false;
    }

    pub fn select_legacy(&mut self, provider_changed: bool, selected: &ModelSelected) {
        if provider_changed {
            self.clear_selectors();
            self.route.reset_epoch = selected.selection_epoch;
            self.unknown.reset = selected.selection_epoch.is_none();
        }
        self.route.selection_epoch = selected.selection_epoch;
        self.unknown.epoch = selected.selection_epoch.is_none();
        // Old select receipts/facts never recorded the optional account setter.
        // A model key in opaque request_json is not evidence it was absent.
        self.unknown.account = true;
    }

    /// Coordinate-only events from the pre-epoch child admission format
    /// preserve a creation account unless an explicit rebind is released.
    /// This proves only a legacy digest's selectors, never a full run route.
    pub(crate) fn select_pre_epoch_coordinates(&mut self, provider_changed: bool) {
        if provider_changed {
            if self.unknown.rebind {
                self.unknown.account = true;
            } else if self.route.rebind_id.is_some() {
                self.route.account = None;
                self.unknown.account = false;
            }
            self.route.base_url = None;
            self.route.rebind_id = None;
            self.unknown.base = false;
            self.unknown.rebind = false;
        }
        self.unknown.observation = true;
        self.unknown.epoch = true;
        self.unknown.reset = true;
    }

    pub fn resolve_legacy(&mut self, selected: &ModelSelected) {
        // An already-seen, known alias cannot change without an epoch advance.
        // A same-epoch publication may merely carry an outstanding clamp notice.
        let unchanged = self.route.resolved_seen
            && !self.unknown.observation
            && !self.unknown.epoch
            && self.route.selection_epoch == selected.selection_epoch
            && selected.selection_epoch.is_some();
        if !unchanged {
            self.route.resolved_alias = None;
            self.unknown.observation = true;
        }
        self.route.resolved_seen = true;
        self.route.selection_epoch = selected.selection_epoch;
        self.unknown.epoch = selected.selection_epoch.is_none();
    }

    pub fn rebind_legacy(&mut self, provider_changed: bool, rebound: &SessionProviderRebound) {
        let reset = provider_changed
            || (!self.unknown.base && self.route.base_url != rebound.base_url)
            || (!self.unknown.account && self.route.account != rebound.account);
        let reset_uncertain = !reset && (self.unknown.base || self.unknown.account);
        self.route.rebind_id = Some(rebound.rebind_id.clone());
        self.route.base_url.clone_from(&rebound.base_url);
        self.route.account.clone_from(&rebound.account);
        self.unknown.account = false;
        self.unknown.base = false;
        self.unknown.rebind = false;
        if let Some(epoch) = rebound.selection_epoch {
            self.route.selection_epoch = Some(epoch);
            self.unknown.epoch = false;
        }
        if reset {
            self.route.resolved_alias = None;
            self.route.resolved_seen = false;
            self.route.reset_epoch = rebound.selection_epoch;
            self.unknown.observation = false;
            self.unknown.reset = rebound.selection_epoch.is_none();
        } else if reset_uncertain {
            self.unknown.observation = true;
            self.unknown.reset = true;
        }
    }
}
