//! ⚠ DEMO-ONLY persistence — the sim's `localStorage` ported to a file.
//!
//! This is **NOT the real session store**. It is the across-restart state of
//! `haider tui --demo` and nothing else: one JSON file, deliberately named
//! `demo-tui-state.json` so nobody mistakes it for harness data. The REAL
//! daemon-backed store replaces this wholesale at W3c; when that lands, this
//! module is deleted, not evolved.
//!
//! Sim parity source (`next-diffforge/src/pages/tui.js`):
//! - **Save** (tui.js:765-772, key `haider-tui-v1`): `{ sessions, themeName,
//!   vfs, launcherDir, voice, … }` on every change. The Rust port piggybacks
//!   the run loop's coalesced frame cadence (≤30 writes/s while streaming,
//!   zero when idle) plus a final write on quit: a synchronous small-JSON
//!   write at the sim's save points, hash-skipped when nothing persisted
//!   changed, so the event loop never blocks noticeably and no timer or arm
//!   is introduced. Write failures are swallowed exactly as the sim's
//!   `catch {}` — storage full/blocked, the demo keeps running in memory.
//!   The sim's `accounts`/`nodes`/`voiceSession` singles have no persisted
//!   port surface yet (accounts/peers are W3 stubs; the aura reseeds every
//!   boot) and `haider-tui-startup-v1` waits on startup gates that do not
//!   exist here — both deliberately skipped, per the 13b brief.
//! - **Load** (tui.js:699-754), guards in order — each one was a sim bug
//!   fix, see [`hydrate`]:
//!   1. proceed only on a parseable file with a NON-EMPTY session array;
//!      anything else → seeds, never a crash ([`DemoStore::load`]);
//!   2. id-collision bump (the sim scans `e(\d+)` and bumps `nid` +1000;
//!      the port's minted ids are `card_seq` menu ids and session ids);
//!   3. roster-counter restore from every recorded `ros`;
//!   4. `sweepClosedChips` + callsign backfill;
//!   5. `rosterRef = next` after the walk, then the guarded singles.
//! - **Not restored** (deliberately, sim §6): run states (every session
//!   loads IDLE), `activeId`/screen (always boot → launcher), msg queues,
//!   menu RESOLVERS (a persisted card answered after reload lands the sim's
//!   stale-menu note — see the driver's `Answer` arm), timers.
//! - **/reset** (tui.js:1913-1943): `removeItem` + reseed — ported as
//!   [`crate::app::AppRequest::PurgeDemoStore`] + the reducer's reseed; the
//!   next save refills the file with seeds exactly as the sim's save effect
//!   refills `localStorage`.
//!
//! Serde rides mirror DTOs, not derives on the runtime types: `hon` is
//! `&'static str` on [`ChipModel`] (it cannot `Deserialize`), and a versioned
//! on-disk shape must not be an accidental projection of in-memory structs.
//! Protocol types ([`Menu`], [`TurnItem`], [`Usage`], [`TodoItem`]) are the
//! wire format and serialize as themselves.

use crate::app::{AppModel, ChipModel, ChipQuestion, VoiceState};
use crate::projection::{ItemBlock, SessionProjection, TodoPanel, TranscriptEntry};
use crate::script::{ChipDisplayState, roster_at};
use crate::session::SessionState;
use crate::theme::ThemeKey;
use base64::Engine as _;
use haider_protocol::history::TodoItem;
use haider_protocol::ids::ItemId;
use haider_protocol::item::TurnItem;
use haider_protocol::menu::Menu;
use haider_protocol::provider::Usage;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The state file's name — `demo-` prefixed on purpose (see module docs).
pub const DEMO_STORE_FILE: &str = "demo-tui-state.json";

/// The on-disk format version. Checked as part of guard 1: any other
/// value (or its absence) rejects the whole file back to seeds — the
/// demo store never guesses at a foreign format (review TUI4.1 P1-1).
pub const DEMO_STORE_VERSION: u32 = 1;

/// Handle on the demo state file: knows the path and the hash of the last
/// write, so unchanged frames skip the disk entirely.
#[derive(Debug)]
pub struct DemoStore {
    path: PathBuf,
    last_hash: Option<u64>,
}

impl DemoStore {
    /// A store at an explicit path (tests point this into a temp dir).
    #[must_use]
    pub fn at(path: PathBuf) -> Self {
        Self {
            path,
            last_hash: None,
        }
    }

    /// The default location: `$HAIDER_PROFILE_DIR/demo-tui-state.json`,
    /// falling back to `~/.haider/dev-profile/` — the same resolution the
    /// CLI's profile dir uses, so the demo file lives beside the profile it
    /// belongs to. `None` (no HOME either) simply disables persistence.
    #[must_use]
    pub fn default_path() -> Option<PathBuf> {
        let profile = std::env::var_os("HAIDER_PROFILE_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(|home| PathBuf::from(home).join(".haider").join("dev-profile"))
            })?;
        Some(profile.join(DEMO_STORE_FILE))
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Guard 1 (sim tui.js:700-704 + its whole-body `catch {}`), STRICT
    /// (review TUI4.1 P1-1 — the sim throws before `setSessions`, so any
    /// structural surprise preserves the seeds whole; per-field defaulting
    /// on the structural core was quietly accepting partial state):
    /// - missing file, unreadable bytes, ANY parse error → seeds;
    /// - a `version` other than [`DEMO_STORE_VERSION`] (or absent) →
    ///   seeds — serde ignores unknown fields, so without the check a
    ///   future format hydrated as if it were ours AND was rewritten
    ///   without its version;
    /// - `deny_unknown_fields` on every DTO: within a version, an
    ///   unknown key is tampering, not tolerance;
    /// - a session missing its required shape fails the WHOLE file
    ///   (all-or-nothing, the sim's throw), never a blank-field session;
    /// - an EMPTY session array → seeds;
    /// - a session id 0 → seeds: 0 is the scratch-lineage sentinel
    ///   (Fable D3-3 — the driver drops `Session(0)` events while a
    ///   session is attached, so an id-0 session's turns silently die).
    ///
    /// The strict tier is the SESSION CORE — the shape the sim's
    /// `s.branches.map(…)` throws on. ONE LEVEL BELOW it (`ProjectionDto`
    /// and `ChipDto` interiors) fields deliberately keep their defaults:
    /// those are the sim's per-item `b.menus || []` tolerances, and an
    /// absent transcript is an empty transcript, not a damaged session.
    #[must_use]
    pub fn load(&self) -> Option<StateDto> {
        let raw = std::fs::read_to_string(&self.path).ok()?;
        let dto: StateDto = serde_json::from_str(&raw).ok()?;
        if dto.version != DEMO_STORE_VERSION {
            return None;
        }
        if dto.sessions.is_empty() || dto.sessions.iter().any(|session| session.id == 0) {
            return None;
        }
        Some(dto)
    }

    /// Serialize the model's persisted slice and write it if it changed
    /// since the last write (see the module docs for the timing contract).
    /// The write is atomic (temp file + rename) so a crash mid-write leaves
    /// the previous state, not a truncated file.
    pub fn save(&mut self, model: &AppModel) {
        let Ok(json) = serde_json::to_string(&snapshot(model)) else {
            return;
        };
        let hash = {
            use std::hash::{DefaultHasher, Hash, Hasher};
            let mut hasher = DefaultHasher::new();
            json.hash(&mut hasher);
            hasher.finish()
        };
        if self.last_hash == Some(hash) {
            return;
        }
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = self.path.with_extension("json.tmp");
        let written =
            std::fs::write(&tmp, json.as_bytes()).and_then(|()| std::fs::rename(&tmp, &self.path));
        if written.is_ok() {
            self.last_hash = Some(hash);
        }
    }

    /// `/reset`'s file purge (sim tui.js:1918 `removeItem`). The hash
    /// resets with it so the very next save rewrites the seeds.
    pub fn purge(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        self.last_hash = None;
    }
}

// ---------------------------------------------------------------- DTOs ----

/// The on-disk root — the sim's `haider-tui-v1` payload, minus the singles
/// whose surfaces are not persisted yet (module docs). Strict on the
/// structural core (guard 1); the SINGLES keep `#[serde(default)]`
/// because the sim guards each individually (`if (data.themeName …)`,
/// guard 5) — their absence is tolerance, not damage.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateDto {
    /// [`DEMO_STORE_VERSION`] — required, checked first.
    pub version: u32,
    pub sessions: Vec<SessionDto>,
    #[serde(default)]
    pub theme: String,
    #[serde(default)]
    pub vfs: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub launcher_dir: String,
    #[serde(default)]
    pub voice: Option<VoiceDto>,
    /// Guard 2's port half: `/voice`·`/tools` menu ids are `voice-card-N`;
    /// restoring N keeps a post-reload card from minting an id a persisted
    /// (possibly still-open) card already used.
    #[serde(default)]
    pub card_seq: u64,
}

/// STRICT structural core (review TUI4.1 P1-1): every field is required —
/// a session that fails to carry its full shape rejects the whole file
/// back to seeds, exactly as the sim's `s.branches.map(…)` throw does.
/// The ONE tolerated absence is `head`, because guard 4's backfill
/// (`s.head || rosterAt(next++)`, tui.js:725) is itself sim law.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionDto {
    pub id: u64,
    pub name: Option<String>,
    pub title: Option<String>,
    /// Absent for sessions stored before heads were named (sim guard 4
    /// backfills `rosterAt(next++)`).
    #[serde(default)]
    pub head: Option<HeadDto>,
    pub dir: String,
    pub model_short: String,
    pub device: String,
    pub ago: String,
    pub branches: u32,
    pub turns_offset: u32,
    pub projection: ProjectionDto,
    pub chips: Vec<ChipDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeadDto {
    pub callsign: String,
    #[serde(default)]
    pub hon: String,
    /// The roster index the head was claimed at (guard 3 reads it).
    #[serde(default)]
    pub ros: Option<u64>,
}

/// The persisted slice of a [`SessionProjection`] — display rows, the open
/// menu, the todo panel, the usage meter, the idle(i) marker. Stream-scoped
/// state (run state, seq accounting, idempotency sets) deliberately absent:
/// every session loads IDLE (sim §6).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectionDto {
    #[serde(default)]
    pub entries: Vec<EntryDto>,
    #[serde(default)]
    pub menu: Option<Menu>,
    #[serde(default)]
    pub todos: Option<TodosDto>,
    #[serde(default)]
    pub usage: Option<Usage>,
    #[serde(default)]
    pub interrupted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TodosDto {
    pub item_id: ItemId,
    pub items: Vec<TodoItem>,
    pub pinned: bool,
}

/// One transcript row. Item rows persist their FINAL display state — a
/// stream cannot survive a restart, so `streaming`/`args_fragments` are
/// dropped at save and every restored block is complete (the sim's entries
/// hold whatever text had accumulated, rendered as a finished row).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EntryDto {
    User {
        text: String,
        #[serde(default)]
        attachments: usize,
        #[serde(default)]
        voice: bool,
    },
    Item {
        item_id: ItemId,
        item: TurnItem,
        /// Bounded command-output tail, base64 (it is bytes by law).
        #[serde(default)]
        output_tail_b64: String,
        #[serde(default)]
        output_truncated: bool,
        #[serde(default)]
        output_decode_error: bool,
        #[serde(default)]
        spoken: bool,
    },
    Note {
        text: String,
    },
    Shell {
        cmd: String,
        out: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChipDto {
    pub agent: String,
    #[serde(default)]
    pub ros: Option<u64>,
    /// Empty for chips stored before the naming feature — guard 4 backfills
    /// `rosterAt(ros ?? next++)` exactly as the sim does (tui.js:730).
    #[serde(default)]
    pub callsign: String,
    #[serde(default)]
    pub hon: String,
    #[serde(default)]
    pub full: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub device: String,
    /// [`ChipDisplayState::label`] string; unknown labels degrade to IDLE
    /// (graceful for hand-edited files — never a crash).
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub tokens: u64,
    #[serde(default)]
    pub question: Option<QuestionDto>,
    /// Persisted so guard 4's sweep can drop chips closed before quit whose
    /// 5 s removal never fired (sim `sweepClosedChips` at load).
    #[serde(default)]
    pub closed: bool,
    #[serde(default)]
    pub children: Vec<ChipDto>,
    #[serde(default)]
    pub transcript: ProjectionDto,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionDto {
    pub recovery: bool,
    pub text: String,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub resolved: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceDto {
    pub enabled: bool,
    #[serde(default)]
    pub stt: String,
    #[serde(default)]
    pub tts: String,
    #[serde(default)]
    pub duplex: bool,
}

// ---------------------------------------------------------- snapshotting ----

/// The persisted slice of the model — the sim's save payload. The ATTACHED
/// session is virtually checked in: its dynamics read from the model's live
/// fields, its statics from the slot (checkout leaves them there).
#[must_use]
pub fn snapshot(model: &AppModel) -> StateDto {
    let sessions = model
        .sessions
        .iter()
        .map(|slot| {
            if model.active_session == Some(slot.id) {
                SessionDto {
                    id: slot.id,
                    name: model.session_name.clone(),
                    title: model.session_title.clone(),
                    head: Some(HeadDto {
                        callsign: model.session_head.0.clone(),
                        hon: model.session_head.1.clone(),
                        ros: slot.head_ros,
                    }),
                    dir: model.session_dir.clone(),
                    model_short: slot.model_short.clone(),
                    device: slot.device.clone(),
                    ago: slot.ago.clone(),
                    branches: slot.branches,
                    turns_offset: slot.turns_offset,
                    projection: projection_to_dto(&model.projection),
                    chips: model.chips.iter().map(chip_to_dto).collect(),
                }
            } else {
                SessionDto {
                    id: slot.id,
                    name: slot.name.clone(),
                    title: slot.title.clone(),
                    head: Some(HeadDto {
                        callsign: slot.head.0.clone(),
                        hon: slot.head.1.clone(),
                        ros: slot.head_ros,
                    }),
                    dir: slot.dir.clone(),
                    model_short: slot.model_short.clone(),
                    device: slot.device.clone(),
                    ago: slot.ago.clone(),
                    branches: slot.branches,
                    turns_offset: slot.turns_offset,
                    projection: projection_to_dto(&slot.projection),
                    chips: slot.chips.iter().map(chip_to_dto).collect(),
                }
            }
        })
        .collect();
    StateDto {
        version: DEMO_STORE_VERSION,
        sessions,
        theme: model.theme.name().to_owned(),
        vfs: model.vfs.clone(),
        launcher_dir: model.launcher_dir.clone(),
        voice: Some(VoiceDto {
            enabled: model.voice.enabled,
            stt: model.voice.stt.clone(),
            tts: model.voice.tts.clone(),
            duplex: model.voice.duplex,
        }),
        card_seq: model.card_seq,
    }
}

fn projection_to_dto(projection: &SessionProjection) -> ProjectionDto {
    ProjectionDto {
        entries: projection.entries().iter().map(entry_to_dto).collect(),
        menu: projection.open_menu().cloned(),
        todos: projection.todos().map(|panel| TodosDto {
            item_id: panel.item_id.clone(),
            items: panel.items.clone(),
            pinned: panel.pinned,
        }),
        usage: projection.usage().cloned(),
        interrupted: projection.interrupted(),
    }
}

fn entry_to_dto(entry: &TranscriptEntry) -> EntryDto {
    match entry {
        TranscriptEntry::User {
            text,
            attachments,
            voice,
        } => EntryDto::User {
            text: text.clone(),
            attachments: *attachments,
            voice: *voice,
        },
        TranscriptEntry::Item(block) => EntryDto::Item {
            item_id: block.item_id.clone(),
            item: block.item.clone(),
            output_tail_b64: base64::engine::general_purpose::STANDARD.encode(&block.output_tail),
            output_truncated: block.output_truncated,
            output_decode_error: block.output_decode_error,
            spoken: block.spoken,
        },
        TranscriptEntry::Note { text } => EntryDto::Note { text: text.clone() },
        TranscriptEntry::Shell { cmd, out } => EntryDto::Shell {
            cmd: cmd.clone(),
            out: out.clone(),
        },
    }
}

fn chip_to_dto(chip: &ChipModel) -> ChipDto {
    ChipDto {
        agent: chip.agent.clone(),
        ros: chip.ros,
        callsign: chip.callsign.clone(),
        hon: chip.hon.to_owned(),
        full: chip.full.clone(),
        name: chip.name.clone(),
        model: chip.model.clone(),
        device: chip.device.clone(),
        state: chip.state.label().to_owned(),
        tokens: chip.tokens,
        question: chip.question.as_ref().map(|q| QuestionDto {
            recovery: q.recovery,
            text: q.text.clone(),
            options: q.options.clone(),
            resolved: q.resolved,
        }),
        closed: chip.closed,
        children: chip.children.iter().map(chip_to_dto).collect(),
        transcript: projection_to_dto(&chip.transcript),
    }
}

// ------------------------------------------------------------- hydration ----

/// What [`hydrate`] restored, for the caller's precedence decisions (an
/// explicit `--theme` flag beats a persisted theme; a persisted theme beats
/// system detection).
#[derive(Debug, Clone, Copy)]
pub struct HydrateOutcome {
    pub theme_restored: bool,
}

/// Guards 2-5 of the sim's load (tui.js:706-745), in order, against a
/// freshly seeded model. Guard 1 lives in [`DemoStore::load`] — version,
/// strict shape, non-empty, no id-0 — so reaching here means the payload
/// is structurally sound and every session id is a real (non-sentinel)
/// identity.
pub fn hydrate(model: &mut AppModel, dto: StateDto) -> HydrateOutcome {
    // Guard 2 — id-collision bump (sim: scan `e(\d+)`, `bumpEid(max+1000)`,
    // tui.js:706-710). The port's minted ids are session ids and the
    // `voice-card-N` menu counter; both resume PAST everything persisted.
    let max_id = dto.sessions.iter().map(|s| s.id).max().unwrap_or(0);
    model.next_session_id = model.next_session_id.max(max_id.saturating_add(1));
    model.card_seq = model.card_seq.max(dto.card_seq);

    // Guard 3 — resume the honour-roll where prior claims left off
    // (tui.js:711-721): `next = max(3, every head.ros + 1, every chip.ros
    // + 1)`. The walk is recursive over chip trees — the sim reads only the
    // top level, but nested chips carry claims the counter must clear too
    // (same law, superset coverage).
    let mut next = crate::script::ROSTER_FIRST_CLAIM;
    for session in &dto.sessions {
        if let Some(ros) = session.head.as_ref().and_then(|head| head.ros) {
            next = next.max(ros.saturating_add(1));
        }
        walk_ros(&session.chips, &mut next);
    }

    // Guard 4 — materialize sessions with the sim's per-session backfills
    // (tui.js:722-731): head backfill, sweep closed chips, then callsign
    // backfill consuming `next++` only for chips with no recorded ros.
    let mut sessions = Vec::with_capacity(dto.sessions.len());
    for s in dto.sessions {
        let mut entry = SessionState::neutral(s.id);
        entry.name = s.name;
        entry.title = s.title;
        match s.head {
            Some(head) if !head.callsign.is_empty() => {
                entry.head = (head.callsign, head.hon);
                entry.head_ros = head.ros;
            }
            _ => {
                let claimed = roster_at(next);
                next = next.saturating_add(1);
                entry.head = (claimed.callsign, claimed.hon.to_owned());
                entry.head_ros = Some(claimed.ros);
            }
        }
        entry.dir = s.dir;
        entry.model_short = s.model_short;
        entry.device = s.device;
        entry.ago = s.ago;
        entry.branches = s.branches;
        entry.turns_offset = s.turns_offset;
        entry.projection = projection_from_dto(s.projection);
        entry.chips = s.chips.into_iter().map(chip_from_dto).collect();
        crate::session::sweep_closed_chips(&mut entry.chips);
        backfill_callsigns(&mut entry.chips, &mut next);
        sessions.push(entry);
    }
    model.sessions = sessions;
    model.active_session = None;
    model.last_detached = None;

    // Guard 5 — `rosterRef = next` AFTER the walk (tui.js:735), then the
    // guarded singles: theme only if the name is known, vfs merged over the
    // seed, launcherDir, voice (tui.js:736-740).
    model
        .roster
        .store(next, std::sync::atomic::Ordering::SeqCst);
    let theme_restored = match ThemeKey::parse(&dto.theme) {
        Some(key) => {
            model.theme = key;
            true
        }
        None => false,
    };
    let mut vfs = crate::app::vfs_seed();
    vfs.extend(dto.vfs);
    model.vfs = vfs;
    if !dto.launcher_dir.is_empty() {
        model.launcher_dir = dto.launcher_dir.clone();
        // The no-session scratch dir mirrors the launcher's (fresh_session
        // law) — per-session dirs rode in with their sessions above.
        model.session_dir = dto.launcher_dir;
    }
    if let Some(voice) = dto.voice {
        model.voice = VoiceState {
            enabled: voice.enabled,
            stt: voice.stt,
            tts: voice.tts,
            duplex: voice.duplex,
        };
    }
    model.dirty = true;
    HydrateOutcome { theme_restored }
}

fn walk_ros(chips: &[ChipDto], next: &mut u64) {
    for chip in chips {
        if let Some(ros) = chip.ros {
            *next = (*next).max(ros.saturating_add(1));
        }
        walk_ros(&chip.children, next);
    }
}

/// Sim tui.js:730: `c.callsign ? c : { ...c, ...rosterAt(c.ros ?? next++) }`
/// — a recorded ros re-derives the SAME name without consuming a claim; only
/// truly nameless chips burn `next++`.
fn backfill_callsigns(chips: &mut [ChipModel], next: &mut u64) {
    for chip in chips {
        if chip.callsign.is_empty() {
            let index = match chip.ros {
                Some(ros) => ros,
                None => {
                    let claimed = *next;
                    *next = next.saturating_add(1);
                    claimed
                }
            };
            let name = roster_at(index);
            chip.callsign = name.callsign;
            chip.hon = name.hon;
            chip.full = name.full;
            chip.ros = Some(index);
        }
        backfill_callsigns(&mut chip.children, next);
    }
}

fn projection_from_dto(dto: ProjectionDto) -> SessionProjection {
    SessionProjection::hydrate(
        dto.entries.into_iter().map(entry_from_dto).collect(),
        dto.menu,
        dto.todos.map(|todos| TodoPanel {
            item_id: todos.item_id,
            items: todos.items,
            pinned: todos.pinned,
        }),
        dto.usage,
        dto.interrupted,
    )
}

fn entry_from_dto(dto: EntryDto) -> TranscriptEntry {
    match dto {
        EntryDto::User {
            text,
            attachments,
            voice,
        } => TranscriptEntry::User {
            text,
            attachments,
            voice,
        },
        EntryDto::Item {
            item_id,
            item,
            output_tail_b64,
            output_truncated,
            output_decode_error,
            spoken,
        } => {
            // A restored block is FINAL: no stream survives a restart, so
            // `streaming` is false and the fragment accumulator is empty
            // (the sim's reloaded entries render as finished rows).
            let output_tail = base64::engine::general_purpose::STANDARD
                .decode(&output_tail_b64)
                .unwrap_or_default();
            TranscriptEntry::Item(ItemBlock {
                item_id,
                item,
                streaming: false,
                args_fragments: String::new(),
                output_tail,
                output_truncated,
                output_decode_error,
                spoken,
            })
        }
        EntryDto::Note { text } => TranscriptEntry::Note { text },
        EntryDto::Shell { cmd, out } => TranscriptEntry::Shell { cmd, out },
    }
}

fn chip_from_dto(dto: ChipDto) -> ChipModel {
    ChipModel {
        agent: dto.agent,
        ros: dto.ros,
        callsign: dto.callsign,
        hon: hon_static(&dto.hon),
        full: dto.full,
        name: dto.name,
        model: dto.model,
        device: dto.device,
        state: chip_state_from_label(&dto.state),
        tokens: dto.tokens,
        question: dto.question.map(|q| ChipQuestion {
            recovery: q.recovery,
            text: q.text,
            options: q.options,
            resolved: q.resolved,
        }),
        closed: dto.closed,
        // A persisted closed chip is mid-removal by definition; guard 4's
        // sweep drops it before it ever renders.
        removing: dto.closed,
        children: dto.children.into_iter().map(chip_from_dto).collect(),
        transcript: projection_from_dto(dto.transcript),
    }
}

/// Map a persisted honorific back to the `&'static str` the runtime types
/// carry. Every hon this port ever WRITES comes from [`crate::script::ROSTER`],
/// so the lookup is total in practice; an unknown string (hand-edited file)
/// falls back to a `Box::leak` — bounded: hydration runs once per process
/// boot, demo-scoped, and the whole module dies at W3c.
fn hon_static(hon: &str) -> &'static str {
    for (_, known, _) in &crate::script::ROSTER {
        if *known == hon {
            return known;
        }
    }
    Box::leak(hon.to_owned().into_boxed_str())
}

/// Inverse of [`ChipDisplayState::label`]; unknown labels degrade to IDLE
/// (corrupt-tolerant — a wrong STATE string must not cost the whole file).
fn chip_state_from_label(label: &str) -> ChipDisplayState {
    match label {
        "THINKING" => ChipDisplayState::Thinking,
        "STREAMING" => ChipDisplayState::Streaming,
        "RUNNING" => ChipDisplayState::Running,
        "TOOL" => ChipDisplayState::Tool,
        "INPUT_REQUIRED" => ChipDisplayState::InputRequired,
        "WAITING" => ChipDisplayState::Waiting,
        "DONE" => ChipDisplayState::Done,
        "ERROR" => ChipDisplayState::Error,
        _ => ChipDisplayState::Idle,
    }
}
