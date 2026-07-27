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

pub(crate) const CURRENT_SCHEMA_VERSION: u32 = 4;

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
