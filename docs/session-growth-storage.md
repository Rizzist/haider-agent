# Session growth storage upgrade

Schema v32 stores provider-view history in immutable trunk and leaf segments.
Each request keeps a short cursor and digest. The authoritative journal stores
that cursor in an internal envelope record and reconstructs the original
attempt on read. Segment rows remain after the seven-day provider-view CAS
index expiry so restart, replay, and older event reads do not depend on the
expiring cache. Existing v24–v31 request rows and journal envelopes remain
readable without rewriting them.

Schema v33 rebuilds the event-authority shadow from `events` and copies pending
hook outbox rows into tables whose primary keys are their only row trees.
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
storage in memory.

Older binaries reject a v32 or v33 profile as a newer schema. To downgrade,
stop the daemon and restore a **pre-upgrade backup of the entire profile**
before launching the older binary. A SQL `user_version` edit is not a
downgrade: older binaries cannot decode the compact journal records or the
new provider-view index. Events committed after that backup are not present
in the restored profile; export any needed session data with the newer binary
first.
