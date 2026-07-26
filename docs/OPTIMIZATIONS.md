# Deferred optimizations ledger

Ideas surfaced by the efficiency rider (gpt-5.6 analysis in the clean-code pass) that were
NOT adopted at the time — kept here for later adoption. Each entry: where, idea, why deferred.
The clean-code reviewer moves entries in/out; adopted entries get the patch tag noted.

| Where | Idea | Why deferred | Status |
|---|---|---|---|
| scripts/supervise-process-lib.sh | Replace 1s ps-polling with kqueue NOTE_TRACK/NOTE_FORK process tracking (needs a tiny compiled helper) | Interim bash tooling; the Rust worker supervisor (W4/E1) owns this properly | planned → E1 |
| scripts/journal-cat.sh | Per-run monotonic seq field instead of timestamp sort keys | 1s ts + stable sort suffices for human reading | open |
| crates/haider-store | Batch append API for event bursts | API already exists; wire HarnessActor to batch per commit boundary | planned → W1 merge |
| crates/haider-store | CAS inline-small-blobs-in-SQLite threshold | Thousands-of-events scale doesn't need it | open |
| crates/haider-tui | Replace ratatui `unstable-rendered-line-info` (Paragraph::line_count scroll math) with our own unicode-width wrap/measure module (research rec 15) | v0-thin transcript; the width module arrives with markdown rendering | planned → TUI markdown wave |
| crates/haider-tui | Per-block rendered-line cache keyed revision × width × theme (research rec 9); today every frame rebuilds all lines | Demo transcripts are small; trigger = scroll-back over long sessions | planned → session-attach wave |
| crates/haider-tui | plain.rs + render.rs each encode item display shapes (exhaustive matches keep them honest; strings could drift) | Unify into shared block atoms when markdown/wrapping forces a re-touch anyway | open |

## Adopted

| Where | Idea | Patch |
|---|---|---|
| crates/haider-store | Persistent connection + cached prepared statements (highest ROI per rider) | adopted — W1/M1 |
