# Dated workspace v1 — contract addendum

Status: ratified contract (owner decisions 2026-09-14) for the dated default
workspace and the session launch-origin event. Statements here are testable
requirements; the historical design draft is background, this addendum wins
inside the repository.

## 1. Calendar

- C1. The organizing date label uses the deterministic tabular
  `islamic-civil` calendar (civil epoch, Friday; 1 Muharram 1 AH =
  Gregorian 0622-07-19 = JDN 1948440). No network, locale, or
  observation input.
- C2. Year start offset in days is `354*(y-1) + floor((3 + 11*y)/30)`.
  Odd months have 30 days, even months 29, month 12 has 30 in leap
  years; leap years are cycle years {2,5,7,10,13,16,18,21,24,26,29}
  (year % 30, with 0 treated as cycle year 30). Checked integer
  arithmetic only; no floating point.
- C3. Pinned conversions: `2026-09-13 -> 1448-03-30`,
  `2026-09-12 -> 1448-03-29`, `2026-06-17 -> 1448-01-01`,
  `0622-07-19 -> 0001-01-01`.
- C4. Labels are ASCII `YYYY-MM-DD` (zero-padded, year at least four
  digits). Supported AH years are 1–9999; anything outside is an
  explicit error, never a wrapped or clamped date.
- C5. "Today" is the client host's civil date sampled at allocation
  time using the OS local offset at that instant; the date changes at
  local civil midnight. `local` (default) and `UTC` are the only
  timezone policies in v1. A session never re-rolls its directory on
  midnight, DST, travel, or clock correction; only a new independent
  session samples again.

## 2. Workspace selection

- W1. Headless `haider run` keeps launch cwd. No auto-organisation for
  scripts/CI, ever, unless explicitly opted in by flag/env/config.
- W2. New interactive desktop sessions (bare `haider`, `haider tui`)
  resolve mode `auto` by default: preserve launch cwd when it is inside
  a recognised project or an existing Haider-allocated workspace,
  otherwise use the dated workspace.
- W3. Precedence (first match wins): explicit workspace path >
  explicit mode flag > `HAIDER_WORKSPACE` (exact existing absolute
  path) / `HAIDER_WORKSPACE_MODE` (both set together is an error) >
  profile `config.json` `workspace.mode` > invocation default
  (`auto` interactive, `cwd` headless). Under any set `CI` value the
  implicit interactive default becomes `cwd`; explicit choices still win.
  Empty selector values are errors, not silent defaults.
- W4. Project detection walks canonical launch cwd and its ancestors
  checking name/type only (never executing or parsing content):
  `.git` (dir or regular indirection file), `.hg`, `.svn`,
  `.haider-project`, or a regular `Cargo.toml`, `package.json`,
  `pyproject.toml`, `go.mod`, `CMakeLists.txt`, `build.gradle`,
  `build.gradle.kts`, or `Makefile`; a bare Git repository is a
  directory with `objects/`, `refs/` and a regular `HEAD` together.
  On a match the resolver preserves LAUNCH CWD (not the boundary
  root). An inspection error preserves readable cwd (conservative).
- W5. The dated workspace is `<base>/Haider/<hijri-date>/s-<32 lowercase
  hex>` where the 128-bit allocation id is cryptographically random and
  never derived from title/prompt/model/username. The session tool
  workspace is the LEAF; the date directory is a shared organizing
  container only.
- W6. Base resolution: `HAIDER_WORKSPACE_BASE`, else config
  `workspace.base` (both must be nonempty absolute paths), else with an
  explicit `HAIDER_PROFILE_DIR` the profile store's `workspaces` child,
  else the platform Documents convention (macOS/Windows user Documents,
  Linux `XDG_DOCUMENTS_DIR` from `user-dirs.dirs` parsed as data with
  the `$HOME/` prefix form only), falling back to home with a recorded
  reason. Once a base is selected, failure at that base is a visible
  error; no silent spill to another location.
- W7. Android keeps the app-private workspace and immutable ceiling;
  desktop base/env/config resolution never runs there. Peer, fleet,
  SSH-remote and observer paths never auto-allocate. Existing sessions
  keep their stored workspace on resume; no file is ever relocated by
  this feature (migration = none).
- W8. Relative command-line attachments resolve against launch cwd,
  captured before allocation. Relative `--workspace` also resolves
  against launch cwd.

## 3. Lazy materialisation (anti-littering)

- L1. The dated workspace path is RESOLVED without being CREATED.
  Resolution performs no filesystem writes.
- L2. The only directory that may be created eagerly (at allocation
  commit, before first write) is the organizing root chain
  `<base>/Haider/<hijri-date>/`. The session leaf `s-<id>/` and
  everything under it materialises on first write only — never at
  session start, attach, resume, preview, `--help`, status, or replay.
- L3. A session that writes nothing leaves NOTHING on disk below the
  daily root: no leaf, no marker, no empty directory. Chat-only /
  read-only sessions are disk-invisible.
- L4. Leaf creation uses atomic create-new with a bounded retry (8
  fresh ids) on genuine collision; new directories are owner-private
  (0700) on Unix. Request retries reuse the same allocation id.
- L5. If leaf creation succeeds but the same materialisation call then
  fails validation, that call removes only the still-empty leaf it
  created. Once a materialisation call has returned success, an empty
  leaf (e.g. every subsequent write failed or was reverted) REMAINS on
  disk with no automatic cleanup — deliberate, not accidental.
- L6. Recording, displaying, or transmitting the resolved path (origin
  event, TUI preview, metadata) must not create it, and the record
  distinguishes resolved-but-unmaterialised from materialised. The TUI
  must not imply the directory exists before it does.

## 4. Launch-origin event

- O1. Origin rides the EXISTING `session.attach`: an optional
  `launch_origin` registration field on the request and an optional
  current-origin snapshot on the response. Zero new RPC methods; the
  exhaustive method pin stays 136. Feature token:
  `session_launch_origin_v1`; when the daemon lacks it the client
  attaches normally, shows a local-only note, and sends no unsupported
  field.
- O2. Registration is receipt-backed (command id + request digest):
  an identical retry replays the original result; same command id with
  different bytes is rejected; replay never resurrects a superseded
  origin.
- O3. One origin per open: emitted ONCE per session per client open,
  never per turn. Repeated turns, repaint, reconnect, status refresh
  do not mint a new open. A foreground reopen elsewhere (new process,
  or explicit close-and-reopen in one process) mints a fresh open and
  REPLACES the current origin via revision CAS (`expected_revision`
  equals current, 0 = absence). A stale CAS is a conflict, never
  latest-timestamp-wins.
- O4. The transaction atomically: updates the typed
  `SessionMetadataV1.launch_origin` projection (when typed metadata
  exists), appends the additive raw config event
  `session_launch_origin_selected`, and finalises the receipt. It is
  session config only: no conversation node, run, cache-epoch or
  seen/activity movement. Legacy `{}` metadata stays `{}`; the event
  alone carries the origin for such rows.
- O5. Raw history is immutable: every accepted registration remains in
  the journal; "replaced" refers to the current UI/model context slot
  only. The snapshot carries `subject_session_id`, `open_id`,
  `revision`, sanitised `path`, `recorded_at_ms`, `selected_seq`; a
  forked child never inherits a claim that the parent's TUI opened it.
- O6. Sanitisation happens client-side BEFORE transmission and is
  revalidated on daemon ingress: own home and descendants become
  `~`/`~/...` (exact component prefix — `/Users/alice2` is not inside
  `/Users/alice`); other-user home forms mask the user component as
  `<user>` with kind `redacted`; control characters, escapes, bidi
  controls and newlines are escaped; serialized origin data is bounded
  (4096 UTF-8 bytes) and over-limit registration is rejected without
  corrupting the session. Path kinds: `home_relative`, `absolute`,
  `redacted`, `unavailable` (display optional only for `unavailable`).
  No unredacted original in events, receipts, or exports.
- O7. The model sees only the LATEST sanitised origin as one line of
  volatile session context (after the stable shared prefix), recomputed
  at the next logical turn boundary; it is never appended to durable
  conversation history and never changes recorded history.
  `session workspace set` changes workspace authority only and does not
  touch origin. The origin may record a resolved-but-unmaterialised
  workspace path without creating it (L6), including a leaf that was
  materialised and then left empty (L5) — the record's materialisation
  state reflects what actually happened on disk at registration time.
- O8. Only a local interactive CONTROL attach may register; view-mode,
  observer, headless, fleet, peer-remote and Android registration is
  refused without mutation. Client-name strings are not authorization;
  the daemon validates the authenticated local connection and Control
  capability.

## 5. Wire compatibility

- X1. All new request/response/metadata fields are optional-additive
  with absent-field bytes identical to pre-feature frames; existing
  golden fixtures must pass unmodified. New fixtures cover the
  negotiated-present shapes.
- X2. `SessionConfigEventPayload` gains variant
  `SessionLaunchOriginSelected` (tag
  `session_launch_origin_selected`); unknown-variant tolerance for
  older readers follows the existing session-config event contract.
- X3. `SessionMetadataV1` gains optional `launch_origin` and
  `workspace_allocation`; absent fields serialize to the exact
  pre-feature bytes.
