//! Query-only compatibility gate for the v0.0.7 `memory_kind` migration.
//!
//! Existing databases are opened without `CREATE` so SQLite can manage WAL
//! bookkeeping, then the connection is switched to `query_only` before any
//! schema inspection runs.

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateState {
    Fresh,
    ContractReady,
    MigrationRequired,
    DatabaseSchemaInvalid,
}

/// Inspects the existing SQLite schema through a query-only connection. The
/// database is opened read/write without `CREATE` so SQLite can manage WAL
/// bookkeeping, while SQL writes remain disabled by `query_only`. A missing
/// database (or one without either memories table) is fresh; a partial table
/// or column contract is unsafe to classify and is rejected.
pub fn inspect(db_path: &Path) -> Result<GateState, String> {
    if !db_path.exists() {
        return Ok(GateState::Fresh);
    }
    let conn = open_for_inspection(db_path)?;
    let memory_exists = table_exists(&conn, "memory")?;
    let thread_exists = table_exists(&conn, "thread")?;
    match (memory_exists, thread_exists) {
        (false, false) => Ok(GateState::Fresh),
        (true, true) => match (
            memory_kind_not_null(&conn, "memory")?,
            memory_kind_not_null(&conn, "thread")?,
        ) {
            (Some(true), Some(true)) => Ok(GateState::ContractReady),
            (None, None) | (Some(_), Some(_)) => Ok(GateState::MigrationRequired),
            _ => Ok(GateState::DatabaseSchemaInvalid),
        },
        _ => Ok(GateState::DatabaseSchemaInvalid),
    }
}

/// Opens an existing database without allowing inspection queries to mutate it.
///
/// A read/write handle is intentional here: SQLite may need to create or update
/// WAL bookkeeping files even when the caller only reads schema metadata. The
/// connection is switched to query-only immediately after opening, and CREATE
/// is omitted from the flags so a missing database cannot be created as a side
/// effect of the startup check.
fn open_for_inspection(db_path: &Path) -> Result<Connection, String> {
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(|error| error.to_string())?;
    conn.pragma_update(None, "query_only", true)
        .map_err(|error| error.to_string())?;
    Ok(conn)
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool, String> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [table],
        |row| row.get(0),
    )
    .map_err(|error| error.to_string())
}

/// Returns `None` when the column is absent; the boolean is SQLite's
/// `notnull` flag. A nullable column is the expand-stage schema and must not
/// be treated as ready for the v0.0.7 contract.
fn memory_kind_not_null(conn: &Connection, table: &str) -> Result<Option<bool>, String> {
    let mut statement = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|error| error.to_string())?;
    let mut rows = statement.query([]).map_err(|error| error.to_string())?;
    while let Some(row) = rows.next().map_err(|error| error.to_string())? {
        let name: String = row.get(1).map_err(|error| error.to_string())?;
        if name == "memory_kind" {
            let not_null: i64 = row.get(3).map_err(|error| error.to_string())?;
            return Ok(Some(not_null != 0));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rusqlite::Connection;
    use tempfile::tempdir;

    use super::{GateState, inspect, open_for_inspection};

    fn create_database(sql: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("default.sqlite3");
        Connection::open(&path).unwrap().execute_batch(sql).unwrap();
        (dir, path)
    }

    #[test]
    fn legacy_database_without_memory_kind_requires_migration() {
        let (_dir, path) = create_database(
            "CREATE TABLE memory (id INTEGER PRIMARY KEY); CREATE TABLE thread (id INTEGER PRIMARY KEY);",
        );
        assert_eq!(inspect(&path).unwrap(), GateState::MigrationRequired);
    }

    #[test]
    fn nullable_memory_kind_columns_require_the_contract_migration() {
        let (_dir, path) = create_database(
            "CREATE TABLE memory (id INTEGER PRIMARY KEY, memory_kind INTEGER); CREATE TABLE thread (id INTEGER PRIMARY KEY, memory_kind INTEGER);",
        );
        assert_eq!(inspect(&path).unwrap(), GateState::MigrationRequired);
    }

    #[test]
    fn one_sided_memory_kind_contract_is_schema_invalid_without_writing() {
        let (_dir, path) = create_database(
            "CREATE TABLE memory (id INTEGER PRIMARY KEY, memory_kind INTEGER NOT NULL); CREATE TABLE thread (id INTEGER PRIMARY KEY);",
        );
        assert_eq!(inspect(&path).unwrap(), GateState::DatabaseSchemaInvalid);

        let conn = Connection::open(&path).unwrap();
        let memory_columns: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('memory')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let thread_columns: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('thread')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!((memory_columns, thread_columns), (2, 1));
    }

    #[test]
    fn fresh_and_contract_ready_databases_pass_the_gate() {
        let dir = tempdir().unwrap();
        assert_eq!(
            inspect(&dir.path().join("missing.sqlite3")).unwrap(),
            GateState::Fresh
        );
        let (_db_dir, contract_path) = create_database(
            "CREATE TABLE memory (id INTEGER PRIMARY KEY, memory_kind INTEGER NOT NULL); CREATE TABLE thread (id INTEGER PRIMARY KEY, memory_kind INTEGER NOT NULL);",
        );
        assert_eq!(inspect(&contract_path).unwrap(), GateState::ContractReady);
    }

    #[test]
    fn wal_database_without_sidecars_passes_the_gate() {
        let (_dir, path) = create_database(
            "PRAGMA journal_mode = WAL; CREATE TABLE memory (id INTEGER PRIMARY KEY, memory_kind INTEGER NOT NULL); CREATE TABLE thread (id INTEGER PRIMARY KEY, memory_kind INTEGER NOT NULL);",
        );
        remove_wal_sidecars(&path);

        assert!(!wal_sidecar_path(&path).exists());
        assert!(!shm_sidecar_path(&path).exists());
        assert_eq!(inspect(&path).unwrap(), GateState::ContractReady);
    }

    #[test]
    fn wal_database_with_sidecars_passes_the_gate() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("default.sqlite3");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA wal_autocheckpoint = 0; CREATE TABLE memory (id INTEGER PRIMARY KEY); CREATE TABLE thread (id INTEGER PRIMARY KEY); PRAGMA wal_checkpoint(TRUNCATE); ALTER TABLE memory ADD COLUMN memory_kind INTEGER NOT NULL; ALTER TABLE thread ADD COLUMN memory_kind INTEGER NOT NULL;",
        )
        .unwrap();

        assert!(wal_sidecar_path(&path).exists());
        assert!(shm_sidecar_path(&path).exists());
        assert!(fs::metadata(wal_sidecar_path(&path)).unwrap().len() > 0);
        assert_eq!(inspect(&path).unwrap(), GateState::ContractReady);
    }

    #[test]
    fn inspection_connection_rejects_schema_writes() {
        let (_dir, path) = create_database(
            "CREATE TABLE memory (id INTEGER PRIMARY KEY, memory_kind INTEGER NOT NULL); CREATE TABLE thread (id INTEGER PRIMARY KEY, memory_kind INTEGER NOT NULL);",
        );
        let conn = open_for_inspection(&path).unwrap();

        let error = conn
            .execute("CREATE TABLE should_not_be_created (id INTEGER)", [])
            .unwrap_err();
        assert!(error.to_string().contains("readonly"));

        let error = conn
            .execute("INSERT INTO memory (memory_kind) VALUES (1)", [])
            .unwrap_err();
        assert!(error.to_string().contains("readonly"));
    }

    #[test]
    fn inspection_connection_does_not_create_missing_database() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("missing.sqlite3");

        assert!(open_for_inspection(&path).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn corrupted_database_is_rejected() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("default.sqlite3");
        fs::write(&path, b"not a sqlite database").unwrap();

        assert!(inspect(&path).is_err());
    }

    fn remove_wal_sidecars(path: &std::path::Path) {
        let _ = fs::remove_file(wal_sidecar_path(path));
        let _ = fs::remove_file(shm_sidecar_path(path));
    }

    fn wal_sidecar_path(path: &std::path::Path) -> std::path::PathBuf {
        std::path::PathBuf::from(format!("{}-wal", path.display()))
    }

    fn shm_sidecar_path(path: &std::path::Path) -> std::path::PathBuf {
        std::path::PathBuf::from(format!("{}-shm", path.display()))
    }
}
