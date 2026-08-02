# C1 — fs tools: search/glob + anchored edit with freshness enforcement

Owner-directed wave (2026-08-02). One coherent package: token levers +
edit safety. NO haider-tui.

## Scope (haider-tools + broker/registry/protocol as needed)

1. **fs_search**: ripgrep-style bounded content search under the
   workspace root (walk the same confinement as fs_read: canonical,
   symlink-refusing, root-bounded). Input: regex/literal pattern,
   optional path prefix + glob filter, case mode. Output: matched
   lines with path + 1-based line numbers, BOUNDED (max ~200 matches /
   8 KiB preview with the existing ResultBounds + CAS-overflow
   pattern; report truncation honestly). Implement the walk + match
   in-crate (no new deps unless the workspace already has one usable —
   check; plain regex crate acceptable if present, else literal +
   simple patterns first, documented).
2. **fs_glob**: bounded file listing by glob under the root (same
   confinement; sorted, capped ~500 entries + truncation flag).
3. **fs_edit**: anchored string replacement. Input: path, old_string,
   new_string, optional replace_all. Laws: old_string must match
   EXACTLY once (or replace_all), else typed error naming the count;
   UTF-8; size-capped like fs_read.
4. **Freshness enforcement (never ship fs_edit without it)**:
   - Per-session read-state: blake3 digest recorded at every fs_read
     AND at every successful fs_write/fs_edit (self-edit updates
     freshness — an agent never locks itself out).
   - fs_edit and fs_write-over-existing REFUSE when (a) the file was
     never read this session (typed `unread_file`), or (b) the current
     digest differs from the recorded one (typed `stale_read`,
     remediable: "re-read before editing").
   - State rides the EffectBroker's journaled effects so recovery
     rebuilds it (crash-consistent); child/subagent sessions have
     their OWN state (a child's edit changes the file → the parent's
     next mutate trips stale_read naturally via digest mismatch).
5. **Registry/manifests**: three new ToolManifests (advertised ==
   dispatchable law), additive protocol shapes + goldens; providers
   see them next turn (tool inventory is re-read per turn already).

## Laws (minimum)

- search_and_glob_are_root_confined_and_bounded (symlink escape
  refused; truncation flagged, never silent).
- edit_requires_exactly_one_anchor_or_replace_all (count in error).
- unread_file_mutation_refused_typed.
- stale_edit_refused_with_typed_remediable_error (external change
  between read and edit; use literals not shared constants).
- self_edit_never_retrips (read→edit→edit chains stay fresh).
- recovery_rebuilds_freshness_from_journal (restart between read and
  edit preserves both freshness AND staleness verdicts).
- child_edit_trips_parent_stale (two sessions, one file).
- advertised_equals_dispatchable_for_all_three (registry law).
- existing fs_read/fs_write behavior byte-identical when unused
  (goldens).

Standing lane laws: tests never inline; mutation-notes doc with
RUNTIME failures (beware degenerate fixtures + self-referential
constants); CARGO_INCREMENTAL=0; fmt + workspace clippy -D warnings;
additive protocol only; ledger update; NO haider-tui; no Cargo.lock;
no version bumps; leave changes uncommitted; run no git commands. Use
up to 3 research subagents and 2 verify subagents. Finish with a
summary of files changed and tests added.
