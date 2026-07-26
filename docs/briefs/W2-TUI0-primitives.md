# Lane brief TUI0 — Desert Dawn primitives (Fable lane; codex may do plumbing under spec)

Own crates/haider-tui. Visual spec = the /tui sim (next-diffforge) — panel-for-panel parity is
the acceptance bar. Stack: ratatui + crossterm (workspace deps to add).

Order of work:
1. Theme system: the 3 themes (desert-dawn default, ivory, dark) as token structs (bg/panel/
   ink/dim/gold/maroon/frame/ok/warn/err/cyan) — one source, every widget reads tokens.
2. Boot screen: centerpiece (shahada w/ dignity rules — translit fallback tier for v0.1;
   sanctum region, never shares frame with errors), readiness checklist from
   HarnessStatus::Starting checks.
3. Launcher: identity line, recent sessions (name ▸ head callsign · blurb · meta), composer
   (first message starts a session), running rail.
4. Session view: transcript renderer over RawEnvelope stream (item lifecycle → blocks; run
   states → badge), status bar (badge · model · branch · context meter), composer w/ top-rule
   + gold ❯.
5. MockClient: drives all of the above from fixture envelope streams (the fake provider's
   output recorded) — `haider tui --demo` wires it before the daemon exists.
Tests: theme-token snapshot per widget (insta or manual golden strings), state-projection
(envelope seq → expected badge/transcript ops), --plain fallback renders. LOC: split widgets
into files early.
