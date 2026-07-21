//! Read-only compatibility gate for the memory_kind migration.

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

/// The single Lookback-owned user id (mirrors `commands::summaries::LOOKBACK_USER_ID`,
/// duplicated here rather than imported so this gate stays a dependency-free
/// read-only check over a raw SQLite file).
const LOOKBACK_USER_ID: i64 = 1;
/// User ids at or above this value were synthesized by the legacy (pre-Lookback)
/// product for its own owners; rows in this range are recognized but still
/// require migration rather than being treated as unexpected data.
const LEGACY_OWNER_RANGE_START: i64 = 100_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateState {
    Fresh,
    ContractReady,
    MigrationRequired,
    /// The rows cannot be classified without guessing. Automatic migration
    /// must stop here so it never rewrites data owned by another product.
    UnexpectedMemoryData {
        reason: String,
    },
    DatabaseSchemaInvalid,
}

pub fn inspect(db_path: &Path) -> Result<GateState, String> {
    if !db_path.exists() {
        return Ok(GateState::Fresh);
    }
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| error.to_string())?;
    let memory = table_exists(&conn, "memory")?;
    let thread = table_exists(&conn, "thread")?;
    match (memory, thread) {
        (false, false) => Ok(GateState::Fresh),
        (true, true) => inspect_contract(&conn),
        _ => Ok(GateState::DatabaseSchemaInvalid),
    }
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool, String> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [table],
        |row| row.get(0),
    )
    .map_err(|error| error.to_string())
}

fn has_contract_column(conn: &Connection, table: &str) -> Result<bool, String> {
    let mut statement = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|error| error.to_string())?;
    let mut rows = statement.query([]).map_err(|error| error.to_string())?;
    while let Some(row) = rows.next().map_err(|error| error.to_string())? {
        let name: String = row.get(1).map_err(|error| error.to_string())?;
        let not_null: i64 = row.get(3).map_err(|error| error.to_string())?;
        if name == "memory_kind" {
            return Ok(not_null != 0);
        }
    }
    Ok(false)
}

fn exists(conn: &Connection, sql: &str) -> Result<bool, String> {
    conn.query_row(sql, [], |row| row.get(0))
        .map_err(|error| error.to_string())
}

fn inspect_contract(conn: &Connection) -> Result<GateState, String> {
    if !has_contract_column(conn, "memory")? || !has_contract_column(conn, "thread")? {
        return Ok(GateState::MigrationRequired);
    }
    for table in ["memory", "thread"] {
        if exists(
            conn,
            &format!(
                "SELECT EXISTS(SELECT 1 FROM {table} WHERE user_id IS NULL OR (user_id != {LOOKBACK_USER_ID} AND user_id < {LEGACY_OWNER_RANGE_START}))"
            ),
        )? {
            return Ok(GateState::UnexpectedMemoryData {
                reason: format!(
                    "{table}.user_id is NULL, negative, or not a Lookback/legacy owner"
                ),
            });
        }
        if exists(
            conn,
            &format!(
                "SELECT EXISTS(SELECT 1 FROM {table} WHERE user_id >= {LEGACY_OWNER_RANGE_START})"
            ),
        )? {
            return Ok(GateState::MigrationRequired);
        }
        if exists(
            conn,
            &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE memory_kind NOT BETWEEN 1 AND 7)"),
        )? || exists(
            conn,
            &format!(
                "SELECT EXISTS(SELECT 1 FROM {table} WHERE memory_kind IN (2,3,4,5,6,7) AND user_id >= {LEGACY_OWNER_RANGE_START})"
            ),
        )? {
            return Ok(GateState::MigrationRequired);
        }
    }
    if exists(
        conn,
        "SELECT EXISTS(SELECT 1 FROM memory WHERE (memory_kind = 3 AND (user_id IS NULL OR external_id IS NULL OR external_id NOT LIKE 'daily:' || CAST(user_id AS TEXT) || ':%')) OR (memory_kind = 4 AND (user_id IS NULL OR external_id IS NULL OR external_id NOT LIKE 'weekly:' || CAST(user_id AS TEXT) || ':%')) OR (memory_kind = 5 AND (user_id IS NULL OR external_id IS NULL OR external_id NOT LIKE 'monthly:' || CAST(user_id AS TEXT) || ':%')))",
    )? {
        return Ok(GateState::UnexpectedMemoryData {
            reason: "period summary external_id does not prove its owner".into(),
        });
    }
    Ok(GateState::ContractReady)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn db(sql: &str) -> std::path::PathBuf {
        let dir = tempdir().unwrap().keep();
        let path = dir.join("default.sqlite3");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(sql).unwrap();
        path
    }

    #[test]
    fn missing_database_is_fresh() {
        assert_eq!(
            inspect(&tempdir().unwrap().path().join("missing.sqlite3")).unwrap(),
            GateState::Fresh
        );
    }

    #[test]
    fn contract_database_is_ready() {
        let path = db(
            "CREATE TABLE memory (user_id INTEGER, memory_kind INTEGER NOT NULL, external_id TEXT); CREATE TABLE thread (user_id INTEGER, memory_kind INTEGER NOT NULL);",
        );
        assert_eq!(inspect(&path).unwrap(), GateState::ContractReady);
    }

    #[test]
    fn legacy_database_requires_migration() {
        let path =
            db("CREATE TABLE memory (user_id INTEGER); CREATE TABLE thread (user_id INTEGER);");
        assert_eq!(inspect(&path).unwrap(), GateState::MigrationRequired);
    }

    #[test]
    fn partial_database_is_invalid() {
        let path = db(
            "CREATE TABLE memory (user_id INTEGER, memory_kind INTEGER NOT NULL, external_id TEXT);",
        );
        assert_eq!(inspect(&path).unwrap(), GateState::DatabaseSchemaInvalid);
    }

    #[test]
    fn old_owner_or_period_id_requires_migration() {
        let path = db(
            "CREATE TABLE memory (user_id INTEGER, memory_kind INTEGER NOT NULL, external_id TEXT); CREATE TABLE thread (user_id INTEGER, memory_kind INTEGER NOT NULL); INSERT INTO memory VALUES (100000, 3, 'daily:2026-01-01:_all');",
        );
        assert_eq!(inspect(&path).unwrap(), GateState::MigrationRequired);
    }

    #[test]
    fn non_lookback_owner_is_unexpected() {
        let path = db(
            "CREATE TABLE memory (user_id INTEGER, memory_kind INTEGER NOT NULL, external_id TEXT); CREATE TABLE thread (user_id INTEGER, memory_kind INTEGER NOT NULL); INSERT INTO memory VALUES (42, 3, 'daily:42:2026-01-01:_all');",
        );
        assert!(matches!(
            inspect(&path).unwrap(),
            GateState::UnexpectedMemoryData { .. }
        ));
    }

    #[test]
    fn period_id_with_a_non_lookback_owner_is_unexpected() {
        let path = db(
            "CREATE TABLE memory (user_id INTEGER, memory_kind INTEGER NOT NULL, external_id TEXT); CREATE TABLE thread (user_id INTEGER, memory_kind INTEGER NOT NULL); INSERT INTO memory VALUES (42, 3, 'daily:1:2026-01-01:_all');",
        );
        assert!(matches!(
            inspect(&path).unwrap(),
            GateState::UnexpectedMemoryData { .. }
        ));
    }

    #[test]
    fn legacy_generated_owner_range_requires_migration() {
        let path = db(
            "CREATE TABLE memory (user_id INTEGER, memory_kind INTEGER NOT NULL, external_id TEXT); CREATE TABLE thread (user_id INTEGER, memory_kind INTEGER NOT NULL); INSERT INTO memory VALUES (100042, 1, NULL);",
        );
        assert_eq!(inspect(&path).unwrap(), GateState::MigrationRequired);
    }
}
