//! Schema migrations. Owns the versioning rules:
//!
//! - The `user_version` pragma is the authority for which schema a database
//!   has; `schema_migrations` is an audit trail, cross-checked after
//!   migrating to be exactly `1..=CURRENT_SCHEMA_VERSION` with no gaps.
//! - `MIGRATIONS` is append-only: never edit a shipped entry, add the next
//!   numbered one (and bump `CURRENT_SCHEMA_VERSION`). Existing stores apply
//!   each entry in its own IMMEDIATE transaction; schema-zero stores bootstrap
//!   the equivalent latest schema and audit in one transaction. Either path
//!   leaves the database at a whole version after a crash.
//! - A database from a newer writer (version > supported) is refused rather
//!   than migrated downward.

#[cfg(test)]
#[path = "migrations_tests.rs"]
mod tests;

use crate::{StoreResult, now_ms, store_error, to_sqlite_integer};
use haider_protocol::error::{ErrorCode, HaiderError};
use rusqlite::{Connection, Transaction, TransactionBehavior, params};

pub(crate) const CURRENT_SCHEMA_VERSION: u32 = 28;
const LATEST_SCHEMA_VERSION: u32 = 28;

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
    Migration {
        version: 20,
        sql: "
            CREATE TABLE loom_cli_install_jobs (
                job_id             TEXT PRIMARY KEY
                    CHECK (length(job_id) BETWEEN 1 AND 128),
                agent_type_id      TEXT NOT NULL,
                agent_type_rev     INTEGER NOT NULL CHECK (agent_type_rev > 0),
                agent_type_digest  TEXT NOT NULL
                    CHECK (length(agent_type_digest) = 32),
                state              TEXT NOT NULL
                    CHECK (state IN (
                        'queued', 'installing', 'verifying', 'succeeded', 'failed'
                    )),
                total              INTEGER NOT NULL CHECK (total BETWEEN 1 AND 32),
                completed          INTEGER NOT NULL
                    CHECK (completed >= 0 AND completed <= total),
                current_cli        TEXT
                    CHECK (current_cli IS NULL OR length(current_cli) BETWEEN 1 AND 128),
                error              TEXT
                    CHECK (error IS NULL OR length(error) BETWEEN 1 AND 512),
                created_at_ms      INTEGER NOT NULL CHECK (created_at_ms >= 0),
                updated_at_ms      INTEGER NOT NULL
                    CHECK (updated_at_ms >= created_at_ms),
                UNIQUE (agent_type_id, agent_type_rev),
                FOREIGN KEY (agent_type_id) REFERENCES loom_agent_types(id)
            );

            CREATE INDEX loom_cli_install_jobs_state_updated
            ON loom_cli_install_jobs(state, updated_at_ms);

            CREATE TABLE loom_cli_install_items (
                job_id          TEXT NOT NULL,
                ordinal         INTEGER NOT NULL CHECK (ordinal BETWEEN 0 AND 31),
                cli_program     TEXT NOT NULL
                    CHECK (length(cli_program) BETWEEN 1 AND 128),
                state           TEXT NOT NULL
                    CHECK (state IN (
                        'queued', 'installing', 'verifying', 'succeeded', 'failed'
                    )),
                error           TEXT
                    CHECK (error IS NULL OR length(error) BETWEEN 1 AND 512),
                created_at_ms   INTEGER NOT NULL CHECK (created_at_ms >= 0),
                updated_at_ms   INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms),
                PRIMARY KEY (job_id, ordinal),
                UNIQUE (job_id, cli_program),
                FOREIGN KEY (job_id) REFERENCES loom_cli_install_jobs(job_id)
                    ON DELETE CASCADE
            );
        ",
    },
    Migration {
        version: 21,
        sql: "
            CREATE TABLE loom_workflow_revisions (
                id             TEXT NOT NULL,
                rev            INTEGER NOT NULL CHECK (rev > 0),
                digest         TEXT NOT NULL,
                record_json    TEXT NOT NULL,
                created_at_ms  INTEGER NOT NULL,
                PRIMARY KEY (id, rev)
            );

            INSERT INTO loom_workflow_revisions(
                id, rev, digest, record_json, created_at_ms
            )
            SELECT id, rev, digest, record_json, created_at_ms
            FROM loom_workflows;
        ",
    },
    Migration {
        version: 22,
        sql: "
            CREATE TABLE loom_cli_install_events (
                cursor             INTEGER PRIMARY KEY AUTOINCREMENT
                    CHECK (cursor > 0),
                job_id             TEXT NOT NULL,
                agent_type_id      TEXT NOT NULL,
                agent_type_rev     INTEGER NOT NULL CHECK (agent_type_rev > 0),
                agent_type_digest  TEXT NOT NULL
                    CHECK (length(agent_type_digest) = 32),
                state              TEXT NOT NULL
                    CHECK (state IN (
                        'queued', 'installing', 'verifying', 'succeeded', 'failed'
                    )),
                total              INTEGER NOT NULL CHECK (total BETWEEN 1 AND 32),
                completed          INTEGER NOT NULL
                    CHECK (completed >= 0 AND completed <= total),
                current_cli        TEXT
                    CHECK (current_cli IS NULL OR length(current_cli) BETWEEN 1 AND 128),
                error              TEXT
                    CHECK (error IS NULL OR length(error) BETWEEN 1 AND 512),
                created_at_ms      INTEGER NOT NULL CHECK (created_at_ms >= 0),
                updated_at_ms      INTEGER NOT NULL
                    CHECK (updated_at_ms >= created_at_ms),
                FOREIGN KEY (job_id) REFERENCES loom_cli_install_jobs(job_id)
                    ON DELETE CASCADE
            );

            CREATE INDEX loom_cli_install_events_job_cursor
            ON loom_cli_install_events(job_id, cursor);

            INSERT INTO loom_cli_install_events(
                job_id, agent_type_id, agent_type_rev, agent_type_digest,
                state, total, completed, current_cli, error,
                created_at_ms, updated_at_ms
            )
            SELECT job_id, agent_type_id, agent_type_rev, agent_type_digest,
                   state, total, completed, current_cli, error,
                   created_at_ms, updated_at_ms
            FROM loom_cli_install_jobs
            ORDER BY created_at_ms, job_id;
        ",
    },
    Migration {
        version: 23,
        sql: "
            CREATE TABLE run_head_sessions (
                session_id        TEXT PRIMARY KEY,
                through_seq       INTEGER NOT NULL CHECK (through_seq >= 0),
                run_count         INTEGER NOT NULL CHECK (run_count >= 0),
                nonterminal_count INTEGER NOT NULL CHECK (nonterminal_count >= 0),
                FOREIGN KEY (session_id) REFERENCES sessions(id)
            );

            CREATE TABLE run_heads (
                session_id    TEXT NOT NULL,
                run_id        TEXT NOT NULL,
                state_json    TEXT NOT NULL CHECK (length(state_json) > 0),
                state_seq     INTEGER CHECK (state_seq IS NULL OR state_seq > 0),
                terminal      INTEGER NOT NULL CHECK (terminal IN (0, 1)),
                accepted_seq  INTEGER CHECK (accepted_seq IS NULL OR accepted_seq > 0),
                branch_id     TEXT,
                prompt_run_id TEXT,
                checksum      BLOB NOT NULL CHECK (length(checksum) = 32),
                PRIMARY KEY (session_id, run_id),
                FOREIGN KEY (session_id) REFERENCES sessions(id)
            );

            CREATE INDEX run_heads_nonterminal
            ON run_heads(session_id, state_seq DESC)
            WHERE terminal = 0 AND state_seq IS NOT NULL;

            INSERT OR IGNORE INTO run_head_sessions(
                session_id, through_seq, run_count, nonterminal_count
            )
            SELECT id, 0, 0, 0 FROM sessions;
        ",
    },
    Migration {
        version: 24,
        sql: "
            CREATE TABLE provider_view_session_cursors (
                session_id              TEXT PRIMARY KEY,
                next_request_ordinal    INTEGER NOT NULL
                    CHECK (next_request_ordinal > 0),
                FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
            );

            CREATE TABLE provider_view_requests (
                -- No sessions FK: copied fork ledgers retain this source CAS
                -- cursor after the source session itself is deleted. Expiry
                -- owns reclamation.
                session_id       TEXT NOT NULL,
                request_ordinal  INTEGER NOT NULL CHECK (request_ordinal > 0),
                provider         TEXT NOT NULL,
                model            TEXT NOT NULL,
                cache_epoch      TEXT NOT NULL,
                expires_at_ms    INTEGER NOT NULL CHECK (expires_at_ms >= 0),
                PRIMARY KEY (session_id, request_ordinal)
            );

            CREATE TABLE provider_view_blocks (
                provider         TEXT NOT NULL,
                model            TEXT NOT NULL,
                cache_epoch      TEXT NOT NULL,
                session_id       TEXT NOT NULL,
                request_ordinal  INTEGER NOT NULL CHECK (request_ordinal > 0),
                section          TEXT NOT NULL
                    CHECK (section IN ('system', 'tools', 'history')),
                block_ordinal    INTEGER NOT NULL CHECK (block_ordinal >= 0),
                content_hash     TEXT NOT NULL
                    CHECK (length(content_hash) = 71),
                byte_len         INTEGER NOT NULL CHECK (byte_len >= 0),
                expires_at_ms    INTEGER NOT NULL CHECK (expires_at_ms >= 0),
                PRIMARY KEY (
                    provider, model, cache_epoch, session_id,
                    request_ordinal, section, block_ordinal, content_hash
                ),
                FOREIGN KEY (session_id, request_ordinal)
                    REFERENCES provider_view_requests(session_id, request_ordinal)
                    ON DELETE CASCADE
            );

            CREATE INDEX provider_view_requests_expiry
            ON provider_view_requests(expires_at_ms);

            CREATE INDEX provider_view_blocks_hash
            ON provider_view_blocks(content_hash);

            CREATE INDEX provider_view_blocks_request
            ON provider_view_blocks(session_id, request_ordinal);

            CREATE TABLE provider_view_gc (
                content_hash  TEXT PRIMARY KEY
                    CHECK (length(content_hash) = 71),
                queued_at_ms  INTEGER NOT NULL CHECK (queued_at_ms >= 0)
            );
        ",
    },
    Migration {
        version: 25,
        sql: "
            ALTER TABLE profile_meta ADD COLUMN workflow_graph_backfill_version INTEGER
                NOT NULL DEFAULT 0 CHECK (workflow_graph_backfill_version >= 0);

            CREATE TABLE workflow_graph_instances (
                session_id       TEXT NOT NULL,
                graph_id         TEXT NOT NULL,
                through_seq      INTEGER NOT NULL CHECK (through_seq > 0),
                phase            TEXT NOT NULL
                    CHECK (phase IN ('active', 'completed', 'rejected')),
                input_ready      INTEGER NOT NULL
                    CHECK (input_ready IN (0, 1)),
                state_json       TEXT NOT NULL CHECK (length(state_json) > 0),
                PRIMARY KEY (session_id, graph_id),
                FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE,
                FOREIGN KEY (session_id, through_seq)
                    REFERENCES events(session_id, seq)
            );

            CREATE INDEX workflow_graph_instances_session_head
            ON workflow_graph_instances(session_id, through_seq DESC, graph_id ASC);

            CREATE INDEX workflow_graph_instances_session_input
            ON workflow_graph_instances(
                session_id, phase, input_ready, through_seq DESC, graph_id ASC
            );

            CREATE TABLE workflow_node_states (
                session_id       TEXT NOT NULL,
                graph_id         TEXT NOT NULL,
                node             TEXT NOT NULL,
                phase            TEXT NOT NULL
                    CHECK (phase IN ('waiting', 'activated', 'completed', 'rejected')),
                iteration        INTEGER NOT NULL CHECK (iteration >= 0),
                updated_seq      INTEGER NOT NULL CHECK (updated_seq > 0),
                state_json       TEXT NOT NULL CHECK (length(state_json) > 0),
                PRIMARY KEY (session_id, graph_id, node),
                FOREIGN KEY (session_id, graph_id)
                    REFERENCES workflow_graph_instances(session_id, graph_id)
                    ON DELETE CASCADE,
                FOREIGN KEY (session_id, updated_seq)
                    REFERENCES events(session_id, seq)
            );

            CREATE INDEX workflow_node_states_session_phase
            ON workflow_node_states(session_id, phase, updated_seq DESC);
        ",
    },
    Migration {
        version: 26,
        sql: "
            CREATE TABLE IF NOT EXISTS loom_registry_events (
                cursor         INTEGER PRIMARY KEY AUTOINCREMENT CHECK (cursor > 0),
                entry_kind     TEXT NOT NULL
                    CHECK (entry_kind IN ('agent_type', 'workflow')),
                entry_id       TEXT NOT NULL,
                change_kind    TEXT NOT NULL
                    CHECK (change_kind IN (
                        'upserted', 'archived', 'unarchived', 'revision_added'
                    )),
                rev            INTEGER NOT NULL CHECK (rev > 0),
                digest         TEXT NOT NULL CHECK (length(digest) = 32),
                archived       INTEGER NOT NULL CHECK (archived IN (0, 1)),
                created_at_ms  INTEGER NOT NULL CHECK (created_at_ms >= 0)
            );

            CREATE INDEX IF NOT EXISTS loom_registry_events_entry_cursor
            ON loom_registry_events(entry_kind, entry_id, cursor);

            CREATE TABLE IF NOT EXISTS loom_agent_type_revisions (
                id          TEXT NOT NULL,
                rev         INTEGER NOT NULL CHECK (rev > 0),
                digest      TEXT NOT NULL CHECK (length(digest) = 32),
                record_json TEXT NOT NULL,
                PRIMARY KEY (id, rev)
            );

            INSERT OR IGNORE INTO loom_agent_type_revisions(id, rev, digest, record_json)
            SELECT id, rev, digest, record_json FROM loom_agent_types;
        ",
    },
    Migration {
        version: 27,
        sql: "
            CREATE TABLE checkpoints (
                session_id         TEXT NOT NULL,
                seq                INTEGER NOT NULL CHECK (seq > 0),
                checkpoint_id      TEXT NOT NULL UNIQUE,
                branch_id          TEXT,
                run_id             TEXT NOT NULL,
                effect_id          TEXT NOT NULL,
                call_id            TEXT NOT NULL,
                workspace_revision TEXT NOT NULL,
                kind               TEXT NOT NULL
                    CHECK (kind IN ('edit', 'write', 'create', 'delete', 'move')),
                origin             TEXT NOT NULL
                    CHECK (origin IN ('tool', 'undo', 'redo', 'rollback_turn')),
                post_digest        TEXT NOT NULL,
                recorded_at_ms     INTEGER NOT NULL CHECK (recorded_at_ms >= 0),
                record_json        TEXT NOT NULL CHECK (length(record_json) > 0),
                PRIMARY KEY (session_id, seq),
                FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE,
                FOREIGN KEY (session_id, seq) REFERENCES events(session_id, seq)
            );

            CREATE INDEX checkpoints_session_branch_seq
            ON checkpoints(session_id, branch_id, seq DESC);

            CREATE INDEX checkpoints_session_run_seq
            ON checkpoints(session_id, run_id, seq DESC);
        ",
    },
    Migration {
        version: 28,
        sql: "
            ALTER TABLE hook_dispatch_outbox RENAME TO hook_dispatch_outbox_v27;
            CREATE TABLE hook_dispatch_outbox (
                session_id TEXT NOT NULL,
                seq        INTEGER NOT NULL CHECK (seq > 0),
                run_id     TEXT,
                workspace_unavailable INTEGER NOT NULL DEFAULT 0
                    CHECK (workspace_unavailable IN (0, 1)),
                PRIMARY KEY (session_id, seq),
                FOREIGN KEY (session_id, seq) REFERENCES events(session_id, seq)
            );
            INSERT INTO hook_dispatch_outbox(session_id, seq)
            SELECT session_id, seq FROM hook_dispatch_outbox_v27;
            DROP TABLE hook_dispatch_outbox_v27;

            CREATE TABLE workspace_unavailable_runs (
                session_id TEXT NOT NULL,
                run_id     TEXT NOT NULL,
                notice_seq INTEGER NOT NULL CHECK (notice_seq > 0),
                PRIMARY KEY (session_id, run_id),
                FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE,
                FOREIGN KEY (session_id, notice_seq)
                    REFERENCES events(session_id, seq) ON DELETE CASCADE
            );
        ",
    },
];

// The direct schema for an empty profile. The equivalence pin in
// `migrations_tests.rs` compares this path with the complete 0→latest chain;
// every new migration must update both the append-only registry and this SQL.
const LATEST_SCHEMA_SQL: &str = r#"
CREATE TABLE "account_alias_reservations" (
                alias           TEXT PRIMARY KEY,
                command_id      TEXT NOT NULL UNIQUE,
                provider        TEXT NOT NULL,
                was_active      INTEGER NOT NULL
                    CHECK (was_active IN (0, 1)),
                created_at_ms   INTEGER NOT NULL,
                FOREIGN KEY (command_id) REFERENCES "command_receipts"(command_id)
            );

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

CREATE TABLE checkpoints (
                session_id         TEXT NOT NULL,
                seq                INTEGER NOT NULL CHECK (seq > 0),
                checkpoint_id      TEXT NOT NULL UNIQUE,
                branch_id          TEXT,
                run_id             TEXT NOT NULL,
                effect_id          TEXT NOT NULL,
                call_id            TEXT NOT NULL,
                workspace_revision TEXT NOT NULL,
                kind               TEXT NOT NULL
                    CHECK (kind IN ('edit', 'write', 'create', 'delete', 'move')),
                origin             TEXT NOT NULL
                    CHECK (origin IN ('tool', 'undo', 'redo', 'rollback_turn')),
                post_digest        TEXT NOT NULL,
                recorded_at_ms     INTEGER NOT NULL CHECK (recorded_at_ms >= 0),
                record_json        TEXT NOT NULL CHECK (length(record_json) > 0),
                PRIMARY KEY (session_id, seq),
                FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE,
                FOREIGN KEY (session_id, seq) REFERENCES events(session_id, seq)
            );

CREATE TABLE "command_receipts" (
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
                updated_at_ms     INTEGER NOT NULL, parent_branch_id TEXT,
                UNIQUE (parent_session_id, parent_run_id, call_id),
                FOREIGN KEY (child_session_id) REFERENCES sessions(id),
                FOREIGN KEY (parent_session_id) REFERENCES sessions(id)
            );

CREATE TABLE events (
                session_id      TEXT NOT NULL,
                seq             INTEGER NOT NULL CHECK (seq > 0),
                envelope_json   TEXT NOT NULL,
                event_id        TEXT NOT NULL UNIQUE,
                committed_at_ms INTEGER NOT NULL, payload_kind TEXT,
                PRIMARY KEY (session_id, seq),
                FOREIGN KEY (session_id) REFERENCES sessions(id)
            );

CREATE TABLE graph_telemetry_dirty (
                session_id TEXT PRIMARY KEY,
                FOREIGN KEY (session_id) REFERENCES sessions(id)
            );

CREATE TABLE graph_telemetry_projection (
                session_id       TEXT PRIMARY KEY,
                through_seq      INTEGER NOT NULL CHECK (through_seq > 0),
                reducer_version  INTEGER NOT NULL,
                tool_state       BLOB NOT NULL,
                projection       BLOB NOT NULL,
                FOREIGN KEY (session_id) REFERENCES sessions(id)
            );

CREATE TABLE hook_dispatch_outbox (
                session_id TEXT NOT NULL,
                seq        INTEGER NOT NULL CHECK (seq > 0),
                run_id     TEXT,
                workspace_unavailable INTEGER NOT NULL DEFAULT 0
                    CHECK (workspace_unavailable IN (0, 1)),
                PRIMARY KEY (session_id, seq),
                FOREIGN KEY (session_id, seq) REFERENCES events(session_id, seq)
            );

CREATE TABLE workspace_unavailable_runs (
                session_id TEXT NOT NULL,
                run_id     TEXT NOT NULL,
                notice_seq INTEGER NOT NULL CHECK (notice_seq > 0),
                PRIMARY KEY (session_id, run_id),
                FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE,
                FOREIGN KEY (session_id, notice_seq)
                    REFERENCES events(session_id, seq) ON DELETE CASCADE
            );

CREATE TABLE loom_agent_type_revisions (
                id          TEXT NOT NULL,
                rev         INTEGER NOT NULL CHECK (rev > 0),
                digest      TEXT NOT NULL CHECK (length(digest) = 32),
                record_json TEXT NOT NULL,
                PRIMARY KEY (id, rev)
            );

CREATE TABLE loom_agent_types (
                id             TEXT PRIMARY KEY,
                rev            INTEGER NOT NULL CHECK (rev > 0),
                digest         TEXT NOT NULL,
                record_json    TEXT NOT NULL,
                created_at_ms  INTEGER NOT NULL,
                updated_at_ms  INTEGER NOT NULL
            , archived INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0, 1)));

CREATE TABLE loom_cli_install_events (
                cursor             INTEGER PRIMARY KEY AUTOINCREMENT
                    CHECK (cursor > 0),
                job_id             TEXT NOT NULL,
                agent_type_id      TEXT NOT NULL,
                agent_type_rev     INTEGER NOT NULL CHECK (agent_type_rev > 0),
                agent_type_digest  TEXT NOT NULL
                    CHECK (length(agent_type_digest) = 32),
                state              TEXT NOT NULL
                    CHECK (state IN (
                        'queued', 'installing', 'verifying', 'succeeded', 'failed'
                    )),
                total              INTEGER NOT NULL CHECK (total BETWEEN 1 AND 32),
                completed          INTEGER NOT NULL
                    CHECK (completed >= 0 AND completed <= total),
                current_cli        TEXT
                    CHECK (current_cli IS NULL OR length(current_cli) BETWEEN 1 AND 128),
                error              TEXT
                    CHECK (error IS NULL OR length(error) BETWEEN 1 AND 512),
                created_at_ms      INTEGER NOT NULL CHECK (created_at_ms >= 0),
                updated_at_ms      INTEGER NOT NULL
                    CHECK (updated_at_ms >= created_at_ms), cancelled INTEGER NOT NULL DEFAULT 0 CHECK (cancelled IN (0, 1)),
                FOREIGN KEY (job_id) REFERENCES loom_cli_install_jobs(job_id)
                    ON DELETE CASCADE
            );

CREATE TABLE loom_cli_install_items (
                job_id          TEXT NOT NULL,
                ordinal         INTEGER NOT NULL CHECK (ordinal BETWEEN 0 AND 31),
                cli_program     TEXT NOT NULL
                    CHECK (length(cli_program) BETWEEN 1 AND 128),
                state           TEXT NOT NULL
                    CHECK (state IN (
                        'queued', 'installing', 'verifying', 'succeeded', 'failed'
                    )),
                error           TEXT
                    CHECK (error IS NULL OR length(error) BETWEEN 1 AND 512),
                created_at_ms   INTEGER NOT NULL CHECK (created_at_ms >= 0),
                updated_at_ms   INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms),
                PRIMARY KEY (job_id, ordinal),
                UNIQUE (job_id, cli_program),
                FOREIGN KEY (job_id) REFERENCES loom_cli_install_jobs(job_id)
                    ON DELETE CASCADE
            );

CREATE TABLE loom_cli_install_jobs (
                job_id             TEXT PRIMARY KEY
                    CHECK (length(job_id) BETWEEN 1 AND 128),
                agent_type_id      TEXT NOT NULL,
                agent_type_rev     INTEGER NOT NULL CHECK (agent_type_rev > 0),
                agent_type_digest  TEXT NOT NULL
                    CHECK (length(agent_type_digest) = 32),
                state              TEXT NOT NULL
                    CHECK (state IN (
                        'queued', 'installing', 'verifying', 'succeeded', 'failed'
                    )),
                total              INTEGER NOT NULL CHECK (total BETWEEN 1 AND 32),
                completed          INTEGER NOT NULL
                    CHECK (completed >= 0 AND completed <= total),
                current_cli        TEXT
                    CHECK (current_cli IS NULL OR length(current_cli) BETWEEN 1 AND 128),
                error              TEXT
                    CHECK (error IS NULL OR length(error) BETWEEN 1 AND 512),
                created_at_ms      INTEGER NOT NULL CHECK (created_at_ms >= 0),
                updated_at_ms      INTEGER NOT NULL
                    CHECK (updated_at_ms >= created_at_ms), cancelled INTEGER NOT NULL DEFAULT 0 CHECK (cancelled IN (0, 1)),
                UNIQUE (agent_type_id, agent_type_rev),
                FOREIGN KEY (agent_type_id) REFERENCES loom_agent_types(id)
            );

CREATE TABLE loom_registry_events (
                cursor         INTEGER PRIMARY KEY AUTOINCREMENT CHECK (cursor > 0),
                entry_kind     TEXT NOT NULL
                    CHECK (entry_kind IN ('agent_type', 'workflow')),
                entry_id       TEXT NOT NULL,
                change_kind    TEXT NOT NULL
                    CHECK (change_kind IN (
                        'upserted', 'archived', 'unarchived', 'revision_added'
                    )),
                rev            INTEGER NOT NULL CHECK (rev > 0),
                digest         TEXT NOT NULL CHECK (length(digest) = 32),
                archived       INTEGER NOT NULL CHECK (archived IN (0, 1)),
                created_at_ms  INTEGER NOT NULL CHECK (created_at_ms >= 0)
            );

CREATE TABLE loom_workflow_revisions (
                id             TEXT NOT NULL,
                rev            INTEGER NOT NULL CHECK (rev > 0),
                digest         TEXT NOT NULL,
                record_json    TEXT NOT NULL,
                created_at_ms  INTEGER NOT NULL,
                PRIMARY KEY (id, rev)
            );

CREATE TABLE loom_workflows (
                id             TEXT PRIMARY KEY,
                rev            INTEGER NOT NULL CHECK (rev > 0),
                digest         TEXT NOT NULL,
                record_json    TEXT NOT NULL,
                created_at_ms  INTEGER NOT NULL,
                updated_at_ms  INTEGER NOT NULL
            , archived INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0, 1)));

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

CREATE TABLE profile_meta (
                singleton         INTEGER PRIMARY KEY CHECK (singleton = 1),
                worker_generation INTEGER NOT NULL CHECK (worker_generation >= 0)
            , daemon_generation INTEGER NOT NULL DEFAULT 0
            CHECK (daemon_generation >= 0), management_revision INTEGER NOT NULL DEFAULT 0
            CHECK (management_revision >= 0), installation_id TEXT, usage_backfill_version INTEGER
                NOT NULL DEFAULT 0 CHECK (usage_backfill_version >= 0), workflow_graph_backfill_version INTEGER
                NOT NULL DEFAULT 0 CHECK (workflow_graph_backfill_version >= 0));

CREATE TABLE provider_models (
                provider        TEXT PRIMARY KEY,
                models_json     TEXT NOT NULL,
                etag            TEXT,
                fetched_at_ms   INTEGER NOT NULL,
                CHECK (fetched_at_ms >= 0)
            );

CREATE TABLE provider_view_blocks (
                provider         TEXT NOT NULL,
                model            TEXT NOT NULL,
                cache_epoch      TEXT NOT NULL,
                session_id       TEXT NOT NULL,
                request_ordinal  INTEGER NOT NULL CHECK (request_ordinal > 0),
                section          TEXT NOT NULL
                    CHECK (section IN ('system', 'tools', 'history')),
                block_ordinal    INTEGER NOT NULL CHECK (block_ordinal >= 0),
                content_hash     TEXT NOT NULL
                    CHECK (length(content_hash) = 71),
                byte_len         INTEGER NOT NULL CHECK (byte_len >= 0),
                expires_at_ms    INTEGER NOT NULL CHECK (expires_at_ms >= 0),
                PRIMARY KEY (
                    provider, model, cache_epoch, session_id,
                    request_ordinal, section, block_ordinal, content_hash
                ),
                FOREIGN KEY (session_id, request_ordinal)
                    REFERENCES provider_view_requests(session_id, request_ordinal)
                    ON DELETE CASCADE
            );

CREATE TABLE provider_view_gc (
                content_hash  TEXT PRIMARY KEY
                    CHECK (length(content_hash) = 71),
                queued_at_ms  INTEGER NOT NULL CHECK (queued_at_ms >= 0)
            );

CREATE TABLE provider_view_requests (
                -- No sessions FK: copied fork ledgers retain this source CAS
                -- cursor after the source session itself is deleted. Expiry
                -- owns reclamation.
                session_id       TEXT NOT NULL,
                request_ordinal  INTEGER NOT NULL CHECK (request_ordinal > 0),
                provider         TEXT NOT NULL,
                model            TEXT NOT NULL,
                cache_epoch      TEXT NOT NULL,
                expires_at_ms    INTEGER NOT NULL CHECK (expires_at_ms >= 0),
                PRIMARY KEY (session_id, request_ordinal)
            );

CREATE TABLE provider_view_session_cursors (
                session_id              TEXT PRIMARY KEY,
                next_request_ordinal    INTEGER NOT NULL
                    CHECK (next_request_ordinal > 0),
                FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
            );

CREATE TABLE run_head_sessions (
                session_id        TEXT PRIMARY KEY,
                through_seq       INTEGER NOT NULL CHECK (through_seq >= 0),
                run_count         INTEGER NOT NULL CHECK (run_count >= 0),
                nonterminal_count INTEGER NOT NULL CHECK (nonterminal_count >= 0),
                FOREIGN KEY (session_id) REFERENCES sessions(id)
            );

CREATE TABLE run_heads (
                session_id    TEXT NOT NULL,
                run_id        TEXT NOT NULL,
                state_json    TEXT NOT NULL CHECK (length(state_json) > 0),
                state_seq     INTEGER CHECK (state_seq IS NULL OR state_seq > 0),
                terminal      INTEGER NOT NULL CHECK (terminal IN (0, 1)),
                accepted_seq  INTEGER CHECK (accepted_seq IS NULL OR accepted_seq > 0),
                branch_id     TEXT,
                prompt_run_id TEXT,
                checksum      BLOB NOT NULL CHECK (length(checksum) = 32),
                PRIMARY KEY (session_id, run_id),
                FOREIGN KEY (session_id) REFERENCES sessions(id)
            );

CREATE TABLE schema_migrations (
                version         INTEGER PRIMARY KEY,
                applied_at_ms   INTEGER NOT NULL
            );

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

CREATE TABLE sessions (
                id              TEXT PRIMARY KEY,
                created_at_ms   INTEGER NOT NULL,
                meta_json       TEXT NOT NULL
            , seen_at_ms INTEGER
                CHECK (seen_at_ms IS NULL OR seen_at_ms >= 0));

CREATE TABLE workflow_graph_instances (
                session_id       TEXT NOT NULL,
                graph_id         TEXT NOT NULL,
                through_seq      INTEGER NOT NULL CHECK (through_seq > 0),
                phase            TEXT NOT NULL
                    CHECK (phase IN ('active', 'completed', 'rejected')),
                input_ready      INTEGER NOT NULL
                    CHECK (input_ready IN (0, 1)),
                state_json       TEXT NOT NULL CHECK (length(state_json) > 0),
                PRIMARY KEY (session_id, graph_id),
                FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE,
                FOREIGN KEY (session_id, through_seq)
                    REFERENCES events(session_id, seq)
            );

CREATE TABLE workflow_node_states (
                session_id       TEXT NOT NULL,
                graph_id         TEXT NOT NULL,
                node             TEXT NOT NULL,
                phase            TEXT NOT NULL
                    CHECK (phase IN ('waiting', 'activated', 'completed', 'rejected')),
                iteration        INTEGER NOT NULL CHECK (iteration >= 0),
                updated_seq      INTEGER NOT NULL CHECK (updated_seq > 0),
                state_json       TEXT NOT NULL CHECK (length(state_json) > 0),
                PRIMARY KEY (session_id, graph_id, node),
                FOREIGN KEY (session_id, graph_id)
                    REFERENCES workflow_graph_instances(session_id, graph_id)
                    ON DELETE CASCADE,
                FOREIGN KEY (session_id, updated_seq)
                    REFERENCES events(session_id, seq)
            );

CREATE INDEX branches_source
            ON branches(session_id, source_branch_id);

CREATE INDEX checkpoints_session_branch_seq
            ON checkpoints(session_id, branch_id, seq DESC);

CREATE INDEX checkpoints_session_run_seq
            ON checkpoints(session_id, run_id, seq DESC);

CREATE INDEX delegations_parent_run
            ON delegations(parent_session_id, parent_run_id);

CREATE INDEX events_payload_kind_session_seq
            ON events(payload_kind, session_id, seq);

CREATE INDEX loom_cli_install_events_job_cursor
            ON loom_cli_install_events(job_id, cursor);

CREATE INDEX loom_cli_install_jobs_state_updated
            ON loom_cli_install_jobs(state, updated_at_ms);

CREATE INDEX loom_registry_events_entry_cursor
            ON loom_registry_events(entry_kind, entry_id, cursor);

CREATE INDEX provider_view_blocks_hash
            ON provider_view_blocks(content_hash);

CREATE INDEX provider_view_blocks_request
            ON provider_view_blocks(session_id, request_ordinal);

CREATE INDEX provider_view_requests_expiry
            ON provider_view_requests(expires_at_ms);

CREATE INDEX run_heads_nonterminal
            ON run_heads(session_id, state_seq DESC)
            WHERE terminal = 0 AND state_seq IS NOT NULL;

CREATE INDEX workflow_graph_instances_session_head
            ON workflow_graph_instances(session_id, through_seq DESC, graph_id ASC);

CREATE INDEX workflow_graph_instances_session_input
            ON workflow_graph_instances(
                session_id, phase, input_ready, through_seq DESC, graph_id ASC
            );

CREATE INDEX workflow_node_states_session_phase
            ON workflow_node_states(session_id, phase, updated_seq DESC);

INSERT OR IGNORE INTO profile_meta(singleton, worker_generation) VALUES (1, 0);
"#;

/// Brings a fresh or older-version database up to `CURRENT_SCHEMA_VERSION`.
/// Idempotent: re-running on an up-to-date database applies nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MigrationOutcome {
    ExistingSchema,
    BootstrappedFromZero,
}

pub(crate) fn migrate(connection: &mut Connection) -> StoreResult<MigrationOutcome> {
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

    let outcome = if found == 0 && database_has_no_schema(connection)? {
        bootstrap_latest(connection)?;
        MigrationOutcome::BootstrappedFromZero
    } else {
        migrate_incrementally(connection, found)?;
        MigrationOutcome::ExistingSchema
    };

    validate_registry(connection)?;
    Ok(outcome)
}

fn database_has_no_schema(connection: &Connection) -> StoreResult<bool> {
    connection
        .query_row(
            "SELECT NOT EXISTS (
                 SELECT 1 FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'
             )",
            [],
            |row| row.get(0),
        )
        .map_err(sqlite_error)
}

fn bootstrap_latest(connection: &mut Connection) -> StoreResult<()> {
    if LATEST_SCHEMA_VERSION != CURRENT_SCHEMA_VERSION {
        return Err(store_error(
            ErrorCode::StoreCorrupt,
            format!(
                "fresh bootstrap schema version {LATEST_SCHEMA_VERSION} does not match current \
                 version {CURRENT_SCHEMA_VERSION}"
            ),
            false,
        ));
    }
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sqlite_error)?;
    transaction
        .execute_batch(LATEST_SCHEMA_SQL)
        .map_err(sqlite_error)?;
    let applied_at_ms = to_sqlite_integer(now_ms()?)?;
    for migration in MIGRATIONS {
        transaction
            .execute(
                "INSERT INTO schema_migrations(version, applied_at_ms) VALUES (?1, ?2)",
                params![migration.version, applied_at_ms],
            )
            .map_err(sqlite_error)?;
    }
    transaction
        .pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION)
        .map_err(sqlite_error)?;
    transaction.commit().map_err(sqlite_error)
}

fn migrate_incrementally(connection: &mut Connection, found: u32) -> StoreResult<()> {
    for migration in MIGRATIONS.iter().filter(|item| item.version > found) {
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_error)?;
        apply_guarded_columns(&transaction, migration.version)?;
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
    Ok(())
}

/// Columns owned by an unshipped migration are added through an explicit
/// schema guard instead of an unconditional `ALTER TABLE`. This preserves the
/// numbered v26 owner while tolerating an interrupted/merged pre-release store
/// whose column landed before its `user_version` and audit row did.
fn apply_guarded_columns(transaction: &Transaction<'_>, version: u32) -> StoreResult<()> {
    if version != 26 {
        return Ok(());
    }
    for (table, column, declaration) in [
        (
            "loom_agent_types",
            "archived",
            "INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0, 1))",
        ),
        (
            "loom_workflows",
            "archived",
            "INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0, 1))",
        ),
        (
            "loom_cli_install_jobs",
            "cancelled",
            "INTEGER NOT NULL DEFAULT 0 CHECK (cancelled IN (0, 1))",
        ),
        (
            "loom_cli_install_events",
            "cancelled",
            "INTEGER NOT NULL DEFAULT 0 CHECK (cancelled IN (0, 1))",
        ),
    ] {
        add_column_if_missing(transaction, table, column, declaration)?;
    }
    Ok(())
}

/// Runs `PRAGMA table_info` before the one permitted `ADD COLUMN`. Table,
/// column, and declaration inputs are compile-time migration constants, never
/// database or user strings.
fn add_column_if_missing(
    transaction: &Transaction<'_>,
    table: &str,
    column: &str,
    declaration: &str,
) -> StoreResult<()> {
    let mut statement = transaction
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(sqlite_error)?;
    let present = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(sqlite_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sqlite_error)?
        .iter()
        .any(|name| name == column);
    drop(statement);
    if !present {
        transaction
            .execute_batch(&format!(
                "ALTER TABLE {table} ADD COLUMN {column} {declaration};"
            ))
            .map_err(sqlite_error)?;
    }
    Ok(())
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
