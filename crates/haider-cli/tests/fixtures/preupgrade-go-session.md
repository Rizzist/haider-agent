# Pre-upgrade Go session

Captured from the actual installed macOS arm64 Haider 0.0.971 release on
2026-09-13, using an empty isolated home/profile, ambient discovery disabled,
and `haider run --provider haider-code --model Go -p 'Reply with OK.' --json`.
It accepted Go, persisted this session, and failed with credential_missing.
The prompt and paths are disposable test inputs, not a user transcript.

Release SHA-256:
- haider: `6c56d701aa721f6a3200d483d42a0256b7ae60d91a17f7ae91ea049d2695942d`
- haiderd: `1ad1837aa80c40a7fd6c368f0def574065b98ca38fe23f2ff3347c0b5f094225`

The SQL contains byte-preserved sessions/events/command receipts/run index
rows from the stopped release profile, exported with SQLite quote(). Apply
it after opening a fresh Store. The companion metadata JSON is the exact
sessions.meta_json value, for focused factory/resolver tests. Tests relocate
only cwd for portability; the old provider/model and journal stay intact.
No credentials, signing material, caches or binaries are included.
