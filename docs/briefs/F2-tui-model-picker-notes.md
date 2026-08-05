# F2 — TUI model picker wave (F2a-F2e) — implementation notes

Implementer: Fable 5. Branch `f2-tui-model-picker`, built on `main` @
`a274574` + F1 `8b46f87` (the `session.select_model` wire surface is
CONSUMED, never redefined — every shape referenced from `haider-rpc`).

## F2a — full-screen `/model` picker

- `ModelPicker` + `ModelPickerRow` (`app.rs`): a MODEL-LOCAL overlay
  (never a projection card), owning every key while open. Rows derive
  from `provider.list` truth: one per model × provider pair across ALL
  enabled providers; an enabled provider with an empty inventory
  contributes one honest placeholder row wearing its
  `availability_reason`. Auth flavor truth-order: provider key `-oauth`
  encoding → selected account's method → single declared method.
- `/model` moved out of `has_arg_slots` exactly like `/theme`
  (`commands.rs`) — the exact-match lead jump can never hijack ⏎ again;
  Tab keeps completions via `offers_arg_completions`. `/model` is no
  longer session-only: at the launcher a selection sets the default pair
  `session.create` uses (`identity` + pin — deliberately NOT
  `account.set_default_model`, which stays the /providers chip's job).
- Live path: `AppRequest::SelectModel` → `LiveCommand::SelectModel`
  (durable outbox, `command_session`-gated resume, minted command id +
  tracked worker generation) → `RequestBody::SessionSelectModel` with
  `provider: Some(...)` → reply renders the RESOLVED pair via
  `apply_model_selected` — never an echo. Typed refusals correlate
  through `pending_model_select` and land inline
  (`model_select_failed`); after the picker closes they reach the
  session view (`record_local_error`). Feature-gated on
  `session_model_select_v1` with an honest stale-daemon note.
- Renderer: `render_model_picker` covers the body ahead of the screen
  match; search band, pair count / inline error line, selection-follow
  window with `⋮` edge marks, current-pair `●` + `current` tag,
  provider-default `*`, dimmed unavailable rows with reasons, pulsing
  pending mark, value-carrying `Hit::ModelPickerRow` rects.

## F2b — providers scroll + pinned add-login footer

- `ProvidersState` gains `scroll`/`scroll_max` Cells + a `follow_cursor`
  latch — the transcript's render-is-the-single-scroll-authority law.
- `render_providers` splits roster (scrolling, `⋮` edge marks, hit rects
  shifted+clipped by the offset) from a PINNED footer carrying the
  UNCHANGED `push_account_add_buttons` flows + hint; frames under 12
  rows keep the flowed layout (still reachable by scrolling).
- Keys: ↑/↓ cursor (follow), PageUp/PageDown ±8, Home/End, wheel ±3
  (`handle_wheel` gained a Providers arm).

## F2c — composer-top-rule identity

- The band's top border carries `model · oauth|api · reasoning [· fast]`
  right-aligned (NO alias) — `AppModel::composer_identity(budget)` +
  `identity_auth_label()`; `IdentityLine` gains `reasoning:
  Option<String>` / `fast: bool` (rendered only when daemon truth
  arrives; nothing feeds them yet — no guessing).
- WIDTH-DEGRADATION LAW: whole segments drop in order — reasoning
  (+fast) → auth → the entire line; never mid-word truncation.
- Status bar: token meter DIRECTLY right of the state badge; the meter
  yields WHOLE when the bar is too narrow (badge always survives);
  branch `·` q:turn stay; the old `model · provider` block is retired.

## F2d — terminal markdown (assistant text)

- New `md.rs`: `render_markdown` (one `MdLine` per source line — the
  LINE-STABILITY LAW; fence state across lines, delimiter + language
  lines preserved verbatim), inline parser (`**`/`*`/`_`/backtick;
  matched pairs consume ONLY their markers; unterminated spans render
  literally — the streaming case; nesting best-effort, characters never
  dropped), and `wrap_spans` — the transcript's pre-wrap walk over
  kind-tagged cells so styling can never move a break.
- Seam: `item_lines`'s `AgentMessage` arm (render.rs) — markdown spans
  replace the monolithic text span; the streaming cursor rides the last
  line as a `Cursor` span (same budget accounting as before). The plain
  `wrap_body` stays for user/aura/shell text.
- Styles are THEME SLOTS via `Theme::md_style` (style.rs): bold →
  bright+BOLD, italic → ITALIC, inline code → gold on gold_soft (the
  paste-token pair), block → text on bar_bg, fence → dim on bar_bg,
  heading → bright+BOLD with gold marks, list marks → gold. The WCAG
  law (`every_theme_clears_the_contrast_floors`) gained the three pairs.

## F2e — error-visibility sweep

Wire kinds and their now-pinned visibility (one law each,
`f2_error_visibility_tests.rs`):

| kind | surface |
|---|---|
| `RunFailed` | `{code} — {message}` error line (W5g-6, pinned) |
| `RunState::Errored` unpaired | synthesized line, per-turn armed, never doubles a real reason |
| `EffectOutcome::Failed` | `effect failed — {error}` |
| `EffectOutcome::CancelledEscalated` | `effect cancel escalated — {note}` |
| `EffectOutcome::Unknown` | crash-window line |
| red `GateReport` verdicts | `verify …` line per verdict; green/waived stay quiet |
| `Rotation` | visible note naming target + cause (§4.4 "like a model change") |
| rejected `turn.submit` | session-view line via `record_session_error` (attached projection or parked slot), plus the existing turn release |
| `ToolStatus::Failed` | ✗ glyph (pinned) |
| `session.select_model` refusals | inline picker error / session line (pinned in f2_model_picker_tests) |

Per coordinator: the ⊟ COMPACTING "queued — compacting" composer
affordance is deliberately NOT built here (a later pass owns it).

## Re-homed laws (owner-directed contract changes, intent preserved)

- `w5e3::selecting_a_model_requires_it_to_be_discovered` — now proves
  only discovered pairs are selectable through the picker.
- `w5f2::a_pinned_choice_survives_every_later_snapshot`,
  `w5g1::a_late_catalog_updates_even_a_pinned_identity`,
  `w5g1::the_model_picker_adopts_the_picked_models_window` — same laws,
  selection now travels through the picker's ⏎.
- `app_render`/`submit_preprocess` status-bar pins — moved to the new
  placement contract (identity on the rule, tokens right of state).

## Ledger

1649 (post-F1/F3 main) → 1704: +55 (17 markdown, 7 composer identity,
7 providers scroll, 14 model picker, 10 error visibility).
