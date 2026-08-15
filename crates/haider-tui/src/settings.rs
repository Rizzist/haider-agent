//! TUI-LOCAL display settings (owner spec §3) — a small file in the
//! profile dir. This is DISPLAY preference, never daemon truth: nothing
//! here rides the wire, and a missing/corrupt file simply means defaults.
//!
//! One setting today: the theme CHOICE (`system` or a fixed key). The
//! resolved theme is NOT persisted — `system` re-evaluates the terminal's
//! appearance on every boot, which is the whole point of the choice layer.

use crate::theme::ThemeChoice;
use std::path::{Path, PathBuf};

/// The settings file's name, beside the demo state in the profile dir.
pub const SETTINGS_FILE: &str = "tui-settings.json";

/// The on-disk shape: `{"version":1,"theme":"system","notifications":true}`.
/// Strict version gate (an unknown future format keeps defaults rather than
/// half-loads); an unknown theme NAME is guarded at parse (defaults, never a
/// clobber). `notifications` is additive — pre-W-C files omit it and load as
/// `true` (the default), so old settings stay valid.
const SETTINGS_VERSION: u32 = 1;

#[derive(serde::Serialize, serde::Deserialize)]
struct SettingsDto {
    version: u32,
    theme: String,
    #[serde(default = "default_notifications")]
    notifications: bool,
    /// Owner 2026-08-15 (model retention): the last COMMITTED model pick, so
    /// the harness OPENS on the model the user last selected. Additive —
    /// older files omit the pair and the boot seed simply defers to the
    /// existing resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_model: Option<String>,
}

fn default_notifications() -> bool {
    true
}

/// The store: load-once, save-if-changed, atomic writes (temp + rename) —
/// the demo store's timing contract, minus the hashing (one tiny record).
#[derive(Debug)]
pub struct SettingsStore {
    path: PathBuf,
    last_saved: Option<ThemeChoice>,
    /// W-C M2: the desktop-notification toggle mirrored into every write so a
    /// theme save never drops it. Seeded from the file at boot.
    notifications: bool,
    last_saved_notifications: Option<bool>,
    /// Model retention: the `(provider, model)` pair mirrored into every
    /// write so a theme/notification save never drops it. Seeded at boot.
    last_model: Option<(String, String)>,
}

impl SettingsStore {
    /// A store at an explicit path (tests point this into a temp dir).
    #[must_use]
    pub fn at(path: PathBuf) -> Self {
        Self {
            path,
            last_saved: None,
            notifications: true,
            last_saved_notifications: None,
            last_model: None,
        }
    }

    /// The default location: `$HAIDER_PROFILE_DIR/tui-settings.json`,
    /// falling back to `~/.haider/dev-profile/` — the same resolution the
    /// demo store and the CLI profile dir use. `None` (no HOME either)
    /// simply disables persistence.
    #[must_use]
    pub fn default_path() -> Option<PathBuf> {
        let profile = std::env::var_os("HAIDER_PROFILE_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(|home| PathBuf::from(home).join(".haider").join("dev-profile"))
            })?;
        Some(profile.join(SETTINGS_FILE))
    }

    /// The store at the default location, when one resolves.
    #[must_use]
    pub fn open_default() -> Option<Self> {
        Self::default_path().map(Self::at)
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load the persisted choice: missing file, unreadable bytes, a parse
    /// error, a foreign version, or an unknown theme name all mean `None`
    /// — defaults, never damage.
    #[must_use]
    pub fn load(&self) -> Option<ThemeChoice> {
        ThemeChoice::parse(&self.load_dto()?.theme)
    }

    /// W-C M2: load the persisted notification toggle. Any missing/corrupt/
    /// foreign-version file defaults to `true` (notifications on).
    #[must_use]
    pub fn load_notifications(&self) -> bool {
        self.load_dto().is_none_or(|dto| dto.notifications)
    }

    fn load_dto(&self) -> Option<SettingsDto> {
        let raw = std::fs::read_to_string(&self.path).ok()?;
        let dto: SettingsDto = serde_json::from_str(&raw).ok()?;
        (dto.version == SETTINGS_VERSION).then_some(dto)
    }

    /// W-C M2: seed the tracked notification value (from a boot-time load) so
    /// a subsequent theme save preserves it rather than defaulting it back on.
    pub fn set_notifications(&mut self, enabled: bool) {
        self.notifications = enabled;
        self.last_saved_notifications = Some(enabled);
    }

    /// Model retention: the persisted last-committed `(provider, model)`
    /// pick, or `None` on a missing/corrupt/foreign file or a pre-retention
    /// file (either half absent).
    #[must_use]
    pub fn load_last_model(&self) -> Option<(String, String)> {
        let dto = self.load_dto()?;
        Some((dto.last_provider?, dto.last_model?))
    }

    /// Seed the tracked pair (from a boot-time load) so a later theme or
    /// notification save preserves it.
    pub fn set_last_model(&mut self, pair: Option<(String, String)>) {
        self.last_model = pair;
    }

    /// Persist a committed model pick, carrying the current theme +
    /// notification toggle. A no-op when the pair is unchanged.
    pub fn save_last_model_if_changed(&mut self, theme: ThemeChoice, provider: &str, model: &str) {
        let pair = (provider.to_owned(), model.to_owned());
        if self.last_model.as_ref() == Some(&pair) {
            return;
        }
        self.last_model = Some(pair);
        if self.write_dto(theme, self.notifications) {
            self.last_saved = Some(theme);
            self.last_saved_notifications = Some(self.notifications);
        }
    }

    /// Persist the theme choice if it differs from the last write this store
    /// made. Atomic (temp file + rename): a crash mid-write leaves the
    /// previous settings, never a truncated file. The current notification
    /// toggle rides along so a theme save never drops it.
    pub fn save_if_changed(&mut self, choice: ThemeChoice) {
        if self.last_saved == Some(choice) {
            return;
        }
        if self.write_dto(choice, self.notifications) {
            self.last_saved = Some(choice);
            self.last_saved_notifications = Some(self.notifications);
        }
    }

    /// W-C M2: persist a notification-toggle change, carrying the current
    /// theme so it is never dropped. A no-op when the value is unchanged.
    pub fn save_notifications_if_changed(&mut self, theme: ThemeChoice, enabled: bool) {
        if self.last_saved_notifications == Some(enabled) {
            self.notifications = enabled;
            return;
        }
        self.notifications = enabled;
        if self.write_dto(theme, enabled) {
            self.last_saved = Some(theme);
            self.last_saved_notifications = Some(enabled);
        }
    }

    fn write_dto(&self, theme: ThemeChoice, notifications: bool) -> bool {
        let dto = SettingsDto {
            version: SETTINGS_VERSION,
            theme: theme.name().to_owned(),
            notifications,
            last_provider: self
                .last_model
                .as_ref()
                .map(|(provider, _)| provider.clone()),
            last_model: self.last_model.as_ref().map(|(_, model)| model.clone()),
        };
        let Ok(json) = serde_json::to_string(&dto) else {
            return false;
        };
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json.as_bytes())
            .and_then(|()| std::fs::rename(&tmp, &self.path))
            .is_ok()
    }
}
