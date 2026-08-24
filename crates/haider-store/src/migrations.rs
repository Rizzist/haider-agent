//! Schema migrations. Owns the versioning rules:
//!
//! - The `user_version` pragma is the authority for which schema a database
//!   has; `schema_migrations` is an audit trail, cross-checked after
//!   migrating to be exactly `1..=CURRENT_SCHEMA_VERSION` with no gaps.
//! - `MIGRATIONS` is append-only: never edit a shipped entry, add the next
//!   numbered one (and bump `CURRENT_SCHEMA_VERSION`). Each entry applies in
//!   its own IMMEDIATE transaction together with its registry row and
//!   version bump, so a crash leaves the database at a whole version.
//! - A database from a newer writer (version > supported) is refused rather
//!   than migrated downward.

use crate::{StoreResult, now_ms, store_error, to_sqlite_integer};
use haider_protocol::error::{ErrorCode, HaiderError};
use rusqlite::{Connection, TransactionBehavior, params};

pub(crate) const CURRENT_SCHEMA_VERSION: u32 = 19;

struct Migration {
    version: u32,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        sql: "
            CREATE TABLE IF NOT EXISTS schema_migrations (
                version         INTEGER PRIMARY KEY,
                applied_at_ms   INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS sessions (
                id              TEXT PRIMARY KEY,
                created_at_ms   INTEGER NOT NULL,
                meta_json       TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS events (
                session_id      TEXT NOT NULL,
                seq             INTEGER NOT NULL CHECK (seq > 0),
                envelope_json   TEXT NOT NULL,
                event_id        TEXT NOT NULL UNIQUE,
                committed_at_ms INTEGER NOT NULL,
                PRIMARY KEY (session_id, seq),
                FOREIGN KEY (session_id) REFERENCES sessions(id)
            );
        ",
    },
    Migration {
        version: 2,
        sql: "
            CREATE TABLE IF NOT EXISTS profile_meta (
                singleton         INTEGER PRIMARY KEY CHECK (singleton = 1),
                worker_generation INTEGER NOT NULL CHECK (worker_generation >= 0)
            );

            INSERT OR IGNORE INTO profile_meta(singleton, worker_generation) VALUES (1, 0);
        ",
    },
    Migration {
        version: 3,
        sql: "
            ALTER TABLE profile_meta
            ADD COLUMN daemon_generation INTEGER NOT NULL DEFAULT 0
            CHECK (daemon_generation >= 0);
        ",
    },
    Migration {
        version: 4,
        sql: "
            CREATE TABLE menu_resolutions (
                session_id        TEXT NOT NULL,
                menu_id           TEXT NOT NULL,
                request_seq       INTEGER NOT NULL CHECK (request_seq > 0),
                worker_generation INTEGER NOT NULL CHECK (worker_generation >= 0),
                command_id        TEXT NOT NULL UNIQUE,
                answer_json       TEXT NOT NULL,
                input_is_secret_reference INTEGER NOT NULL
                    CHECK (input_is_secret_reference IN (0, 1)),
                resolution_seq    INTEGER NOT NULL CHECK (resolution_seq > request_seq),
                PRIMARY KEY (session_id, menu_id),
                UNIQUE (session_id, resolution_seq),
                FOREIGN KEY (session_id, request_seq)
                    REFERENCES events(session_id, seq),
                FOREIGN KEY (session_id, resolution_seq)
                    REFERENCES events(session_id, seq)
            );
        ",
    },
    Migration {
        version: 5,
        sql: "
            CREATE TABLE command_receipts (
                command_id       TEXT PRIMARY KEY,
                method           TEXT NOT NULL,
                request_digest   TEXT NOT NULL,
                request_json     TEXT NOT NULL,
                state            TEXT NOT NULL
                    CHECK (state IN ('pending', 'committed', 'failed')),
                session_id       TEXT,
                run_id           TEXT,
                accepted_seq     INTEGER CHECK (accepted_seq IS NULL OR accepted_seq > 0),
                response_json    TEXT,
                created_at_ms    INTEGER NOT NULL,
                updated_at_ms    INTEGER NOT NULL
            );
        ",
    },
    Migration {
        version: 6,
        sql: "
            ALTER TABLE profile_meta
            ADD COLUMN management_revision INTEGER NOT NULL DEFAULT 0
            CHECK (management_revision >= 0);

            ALTER TABLE command_receipts
            ADD COLUMN final_revision INTEGER
            CHECK (final_revision IS NULL OR final_revision > 0);
        ",
    },
    Migration {
        version: 7,
        sql: "
            ALTER TABLE command_receipts
            ADD COLUMN recovery_json TEXT;

            CREATE TABLE account_alias_reservations (
                alias           TEXT PRIMARY KEY,
                command_id      TEXT NOT NULL UNIQUE,
                provider        TEXT NOT NULL,
                was_active      INTEGER NOT NULL
                    CHECK (was_active IN (0, 1)),
                created_at_ms   INTEGER NOT NULL,
                FOREIGN KEY (command_id) REFERENCES command_receipts(command_id)
            );
        ",
    },
    Migration {
        version: 8,
        sql: "
            CREATE TABLE provider_models (
                provider        TEXT PRIMARY KEY,
                models_json     TEXT NOT NULL,
                etag            TEXT,
                fetched_at_ms   INTEGER NOT NULL,
                CHECK (fetched_at_ms >= 0)
            );
        ",
    },
    Migration {
        version: 9,
        sql: "
            CREATE TABLE delegations (
                agent_id          TEXT PRIMARY KEY,
                child_session_id  TEXT NOT NULL UNIQUE,
                child_run_id      TEXT NOT NULL,
                parent_session_id TEXT NOT NULL,
                parent_run_id     TEXT NOT NULL,
                call_id           TEXT NOT NULL,
                tool_item_id      TEXT NOT NULL,
                parent_agent_id   TEXT,
                root_session_id   TEXT NOT NULL,
                depth             INTEGER NOT NULL CHECK (depth >= 1),
                task              TEXT NOT NULL,
                prompt            TEXT NOT NULL,
                manifest_json     TEXT NOT NULL,
                state             TEXT NOT NULL
                    CHECK (state IN ('spawned', 'running', 'reported', 'collected')),
                report_json       TEXT,
                created_at_ms     INTEGER NOT NULL,
                updated_at_ms     INTEGER NOT NULL,
                UNIQUE (parent_session_id, parent_run_id, call_id),
                FOREIGN KEY (child_session_id) REFERENCES sessions(id),
                FOREIGN KEY (parent_session_id) REFERENCES sessions(id)
            );

            CREATE INDEX delegations_parent_run
            ON delegations(parent_session_id, parent_run_id);
        ",
    },
    Migration {
        version: 10,
        sql: "
            CREATE TABLE branches (
                session_id       TEXT NOT NULL,
                branch_id        TEXT NOT NULL,
                display_name     TEXT NOT NULL,
                source_branch_id TEXT,
                fork_node_id     TEXT NOT NULL,
                fork_seq         INTEGER NOT NULL CHECK (fork_seq > 0),
                created_seq      INTEGER NOT NULL CHECK (created_seq > 0),
                created_at_ms    INTEGER NOT NULL CHECK (created_at_ms >= 0),
                head_node_id     TEXT NOT NULL,
                head_seq         INTEGER NOT NULL CHECK (head_seq > 0),
                PRIMARY KEY (session_id, branch_id),
                UNIQUE (session_id, created_seq),
                FOREIGN KEY (session_id) REFERENCES sessions(id),
                FOREIGN KEY (session_id, source_branch_id)
                    REFERENCES branches(session_id, branch_id),
                FOREIGN KEY (session_id, fork_seq) REFERENCES events(session_id, seq),
                FOREIGN KEY (session_id, created_seq) REFERENCES events(session_id, seq),
                FOREIGN KEY (session_id, head_seq) REFERENCES events(session_id, seq)
            );

            CREATE INDEX branches_source
            ON branches(session_id, source_branch_id);

            ALTER TABLE delegations ADD COLUMN parent_branch_id TEXT;
        ",
    },
    Migration {
        version: 11,
        sql: "
            CREATE TABLE hook_dispatch_outbox (
                session_id TEXT NOT NULL,
                seq        INTEGER NOT NULL CHECK (seq > 0),
                PRIMARY KEY (session_id, seq),
                FOREIGN KEY (session_id, seq) REFERENCES events(session_id, seq)
            );
        ",
    },
    Migration {
        version: 12,
        sql: "
            CREATE TABLE command_receipts_v12 (
                command_id       TEXT PRIMARY KEY,
                method           TEXT NOT NULL,
                request_digest   TEXT NOT NULL,
                request_json     TEXT NOT NULL,
                state            TEXT NOT NULL
                    CHECK (state IN ('pending', 'committed', 'failed')),
                session_id       TEXT,
                run_id           TEXT,
                accepted_seq     INTEGER CHECK (accepted_seq IS NULL OR accepted_seq > 0),
                response_json    TEXT,
                created_at_ms    INTEGER NOT NULL,
                updated_at_ms    INTEGER NOT NULL,
                final_revision   INTEGER
                    CHECK (final_revision IS NULL OR final_revision >= 0),
                recovery_json    TEXT
            );

            CREATE TABLE account_alias_reservations_v12 (
                alias           TEXT PRIMARY KEY,
                command_id      TEXT NOT NULL UNIQUE,
                provider        TEXT NOT NULL,
                was_active      INTEGER NOT NULL
                    CHECK (was_active IN (0, 1)),
                created_at_ms   INTEGER NOT NULL,
                FOREIGN KEY (command_id) REFERENCES command_receipts_v12(command_id)
            );

            INSERT INTO command_receipts_v12(
                rowid, command_id, method, request_digest, request_json, state,
                session_id, run_id, accepted_seq, response_json,
                created_at_ms, updated_at_ms, final_revision, recovery_json
            )
            SELECT rowid, command_id, method, request_digest, request_json, state,
                   session_id, run_id, accepted_seq, response_json,
                   created_at_ms, updated_at_ms, final_revision, recovery_json
            FROM command_receipts;

            INSERT INTO account_alias_reservations_v12(
                alias, command_id, provider, was_active, created_at_ms
            )
            SELECT alias, command_id, provider, was_active, created_at_ms
            FROM account_alias_reservations;

            DROP TABLE account_alias_reservations;
            DROP TABLE command_receipts;
            ALTER TABLE command_receipts_v12 RENAME TO command_receipts;
            ALTER TABLE account_alias_reservations_v12
                RENAME TO account_alias_reservations;
        ",
    },
    Migration {
        version: 13,
        sql: "
            CREATE TABLE loom_agent_types (
                id             TEXT PRIMARY KEY,
                rev            INTEGER NOT NULL CHECK (rev > 0),
                digest         TEXT NOT NULL,
                record_json    TEXT NOT NULL,
                created_at_ms  INTEGER NOT NULL,
                updated_at_ms  INTEGER NOT NULL
            );
            CREATE TABLE loom_workflows (
                id             TEXT PRIMARY KEY,
                rev            INTEGER NOT NULL CHECK (rev > 0),
                digest         TEXT NOT NULL,
                record_json    TEXT NOT NULL,
                created_at_ms  INTEGER NOT NULL,
                updated_at_ms  INTEGER NOT NULL
            );
        ",
    },
    Migration {
        version: 14,
        sql: "
            ALTER TABLE events ADD COLUMN payload_kind TEXT;
        ",
    },
    Migration {
        version: 15,
        sql: "
            CREATE INDEX events_payload_kind_session_seq
            ON events(payload_kind, session_id, seq);

            CREATE TABLE graph_telemetry_projection (
                session_id       TEXT PRIMARY KEY,
                through_seq      INTEGER NOT NULL CHECK (through_seq > 0),
                reducer_version  INTEGER NOT NULL,
                tool_state       BLOB NOT NULL,
                projection       BLOB NOT NULL,
                FOREIGN KEY (session_id) REFERENCES sessions(id)
            );
        ",
    },
    Migration {
        version: 16,
        sql: "
            CREATE TABLE graph_telemetry_dirty (
                session_id TEXT PRIMARY KEY,
                FOREIGN KEY (session_id) REFERENCES sessions(id)
            );

            UPDATE events SET payload_kind = 'item_legacy'
            WHERE payload_kind = 'item';
        ",
    },
    Migration {
        version: 17,
        sql: "
            ALTER TABLE sessions ADD COLUMN seen_at_ms INTEGER
                CHECK (seen_at_ms IS NULL OR seen_at_ms >= 0);
        ",
    },
    Migration {
        version: 18,
        sql: "
            CREATE TABLE session_projection_checkpoints (
                session_id        TEXT NOT NULL,
                projection        TEXT NOT NULL,
                timeline_key      TEXT NOT NULL,
                through_seq       INTEGER NOT NULL CHECK (through_seq > 0),
                boundary_event_id TEXT NOT NULL,
                payload           BLOB NOT NULL,
                payload_digest    BLOB NOT NULL,
                PRIMARY KEY (session_id, projection, timeline_key),
                FOREIGN KEY (session_id) REFERENCES sessions(id),
                FOREIGN KEY (session_id, through_seq)
                    REFERENCES events(session_id, seq)
            );
        ",
    },
    Migration {
        version: 19,
        sql: "
            ALTER TABLE profile_meta ADD COLUMN installation_id TEXT;
            ALTER TABLE profile_meta ADD COLUMN usage_backfill_version INTEGER
                NOT NULL DEFAULT 0 CHECK (usage_backfill_version >= 0);
        ",
    },
];

/// Brings a fresh or older-version database up to `CURRENT_SCHEMA_VERSION`.
/// Idempotent: re-running on an up-to-date database applies nothing.
pub(crate) fn migrate(connection: &mut Connection) -> StoreResult<()> {
    let found: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(sqlite_error)?;
    if found > CURRENT_SCHEMA_VERSION {
        return Err(store_error(
            ErrorCode::StoreCorrupt,
            format!(
                "database schema version {found} is newer than supported version \
                 {CURRENT_SCHEMA_VERSION}"
            ),
            false,
        ));
    }

    for migration in MIGRATIONS.iter().filter(|item| item.version > found) {
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_error)?;
        transaction
            .execute_batch(migration.sql)
            .map_err(sqlite_error)?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO schema_migrations(version, applied_at_ms) VALUES (?1, ?2)",
                params![migration.version, to_sqlite_integer(now_ms()?)?],
            )
            .map_err(sqlite_error)?;
        transaction
            .pragma_update(None, "user_version", migration.version)
            .map_err(sqlite_error)?;
        transaction.commit().map_err(sqlite_error)?;
    }

    validate_registry(connection)
}

/// Checks the audit registry lists exactly versions `1..=CURRENT`, catching a
/// database that skipped or half-applied a migration outside this module.
fn validate_registry(connection: &Connection) -> StoreResult<()> {
    if CURRENT_SCHEMA_VERSION == 0 {
        // With no registered migrations, the schema_migrations table itself
        // does not exist yet; there is nothing to validate.
        return Ok(());
    }

    let mut statement = connection
        .prepare("SELECT version FROM schema_migrations ORDER BY version")
        .map_err(sqlite_error)?;
    let versions = statement
        .query_map([], |row| row.get::<_, u32>(0))
        .map_err(sqlite_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sqlite_error)?;
    let expected = (1..=CURRENT_SCHEMA_VERSION).collect::<Vec<_>>();
    if versions != expected {
        return Err(store_error(
            ErrorCode::StoreCorrupt,
            format!(
                "migration registry is inconsistent: expected {expected:?}, found {versions:?}"
            ),
            false,
        ));
    }
    Ok(())
}

/// Any SQLite failure while migrating means the schema cannot be trusted, so
/// everything maps to non-retryable `StoreCorrupt`.
fn sqlite_error(error: rusqlite::Error) -> HaiderError {
    store_error(
        ErrorCode::StoreCorrupt,
        format!("schema migration failed: {error}"),
        false,
    )
}
