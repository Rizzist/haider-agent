#![allow(clippy::expect_used)]

use rusqlite::Connection;

use super::{
    CURRENT_SCHEMA_VERSION, LATEST_SCHEMA_VERSION, bootstrap_latest, migrate,
    migrate_incrementally, validate_registry,
};

#[derive(Debug, PartialEq, Eq)]
struct SchemaEntry {
    object_type: String,
    name: String,
    table_name: String,
    sql: Option<String>,
}

fn memory_database() -> Connection {
    let connection = Connection::open_in_memory().expect("open in-memory migration database");
    connection
        .pragma_update(None, "foreign_keys", true)
        .expect("enable foreign keys");
    connection
}

fn schema(connection: &Connection) -> Vec<SchemaEntry> {
    let mut statement = connection
        .prepare(
            "SELECT type, name, tbl_name, sql
             FROM sqlite_master
             ORDER BY type, name, tbl_name",
        )
        .expect("prepare sqlite_master query");
    statement
        .query_map([], |row| {
            let sql = row
                .get::<_, Option<String>>(3)?
                .map(|sql| sql.split_whitespace().collect::<Vec<_>>().join(" "));
            Ok(SchemaEntry {
                object_type: row.get(0)?,
                name: row.get(1)?,
                table_name: row.get(2)?,
                sql,
            })
        })
        .expect("query sqlite_master")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect sqlite_master")
}

#[test]
fn fresh_database_bootstraps_directly_to_latest() {
    assert_eq!(LATEST_SCHEMA_VERSION, CURRENT_SCHEMA_VERSION);
    let mut connection = memory_database();

    migrate(&mut connection).expect("bootstrap latest schema through production entry point");
    validate_registry(&connection).expect("validate bootstrap registry");

    let user_version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("read user_version");
    assert_eq!(user_version, CURRENT_SCHEMA_VERSION);
    let migration_count: u32 = connection
        .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
            row.get(0)
        })
        .expect("count migration audit rows");
    assert_eq!(migration_count, CURRENT_SCHEMA_VERSION);
    let profile_rows: u32 = connection
        .query_row("SELECT COUNT(*) FROM profile_meta", [], |row| row.get(0))
        .expect("count seeded profile rows");
    assert_eq!(profile_rows, 1);
}

/// MUTATION PIN: every new migration must also update `LATEST_SCHEMA_SQL`.
/// Whitespace is normalized, but every sqlite_master object and SQL token must
/// be identical between a direct fresh bootstrap and the complete historical
/// 0→latest migration chain.
#[test]
fn zero_to_latest_migration_matches_fresh_bootstrap_schema() {
    let mut bootstrapped = memory_database();
    bootstrap_latest(&mut bootstrapped).expect("bootstrap latest schema");

    let mut incrementally_migrated = memory_database();
    migrate_incrementally(&mut incrementally_migrated, 0)
        .expect("apply complete incremental migration chain");
    validate_registry(&incrementally_migrated).expect("validate incremental registry");

    assert_eq!(schema(&bootstrapped), schema(&incrementally_migrated));
}
