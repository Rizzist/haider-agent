# Session growth storage upgrade

Schema v32 stores provider-view history in immutable trunk and leaf segments.
Each request keeps a short cursor and digest. The authoritative journal stores
that cursor in an internal envelope record and reconstructs the original
attempt on read. Segment rows remain after the seven-day provider-view CAS
index expiry so restart, replay, and older event reads do not depend on the
expiring cache. Segment rows also outlive deletion of the session that wrote
them, by design: a fork's copied requests can reference the source session's
segments. Existing v24–v31 request rows and journal envelopes remain readable
without rewriting them.

A request reuses its predecessor's trunk only while the predecessor's leaf
still ends at the trunk's current length. Otherwise (newer requests grew the
trunk and then expired first) it branches at the shorter of the common prefix
and that leaf's cutoff. Expiry is also clamped so that a request never expires
before the latest earlier request of its session, which keeps expiry order
equal to request order across wall-clock steps. A journal attempt fact is
stored as a compact segment cursor only when the segment rows rebuild its
exact ledger at append time; otherwise it keeps the self-contained form.

Expiry and CAS reclamation run in bounded steps: at most 128 expired requests
and 256 queued hashes per step. A persist runs at most one step when the
hourly or every-64-persists watermark is due, and later persists continue an
unfinished backlog. Store open and the explicit sweep run steps until done.
A hash is reclaimed only if no provider-view block row references it and no
surviving request of the sessions owning its history rows reaches it. That
coverage is computed once per step, per touched session, so a step does not
scan every live request's history.

Schema v33 rebuilds the event-authority shadow from `events` and copies pending
hook outbox rows into tables whose primary keys are their only row trees.
The store's global page, `pending_hook_dispatches_bounded`, still returns
pending hook work in journal commit order (the `events` rowid), so in that API
one busy session cannot starve another; it has no production caller. The
daemon drains per session instead: `pending_hook_dispatch_session_ids` in
session-id order, then `pending_hook_dispatch_metadata_bounded` by sequence,
which the new primary key serves in the same order as before v33.
Both steps run in the migration transaction. Its insert trigger tests the
ordered shadow before insertion and updates `sessions` only when an old
sequence or event ID is reused. Updates and deletes still advance mutation
authority and invalidate affected projection checkpoints. The retained
`journal_event_seq_high_water` covers pre-upgrade history; the shadow covers
the later history without a per-event session-row rewrite.

The journal is `<profile>/store.sqlite`, with SQLite WAL at
`<profile>/store.sqlite-wal`. A crash during migration rolls back to the
previous schema and retries on open. SQLite WAL commits keep the existing
`synchronous=NORMAL` default and the existing `FULL` override. The automatic
checkpoint uses 1,000 pages, as before the v32 candidate.

Every store connection sets `temp_store=MEMORY`. Group commits hold one
savepoint per request, and a turn's batch can modify more pages than SQLite's
64 KiB in-memory statement-journal budget. With the desktop default, that
journal spilled to an unlinked `etilqs_*` temporary file whose pages still
reached the disk (72–320 KiB on otherwise 4–8 KiB turns). Statement journals
only serve statement and savepoint rollback; crash recovery uses the WAL, so
durability is unchanged. Android's bundled SQLite already keeps all temporary
storage in memory. Profile-sized work switches to a file-backed temp store
and restores the in-memory setting afterwards: migrations, open-time backfills
and sweeps, and whole-session deletes. Memory temp storage has no size bound,
so those operations would otherwise hold a profile-sized statement journal or
sort in RAM. Android builds compile SQLite with `SQLITE_TEMP_STORE=3`, which
ignores the pragma, so there the earlier always-in-memory behaviour is
unchanged.

**Release note (0.0.973): v33 is a one-way store upgrade.** There is no
automatic pre-migration backup. A released 0.0.972 daemon refuses a v33
profile (`StoreCorrupt: database schema version 33 is newer than supported
version 31`, exit 74; the CLI's `--ready`/`run` exit 69) and leaves
`store.sqlite` untouched, so the newer binary can reopen it afterwards.

Older binaries reject a v32 or v33 profile as a newer schema. To downgrade,
stop the daemon and restore a **pre-upgrade backup of the entire profile**
before launching the older binary. A SQL `user_version` edit is not a
downgrade: older binaries cannot decode the compact journal records or the
new provider-view index. Events committed after that backup are not present
in the restored profile; export any needed session data with the newer binary
first.
