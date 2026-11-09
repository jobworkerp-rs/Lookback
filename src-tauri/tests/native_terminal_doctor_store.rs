use std::{path::Path, sync::Arc};

use lookback_tauri_lib::native_terminal::doctor_store::{
    NativeProbeEvidence, NativeProbeIntent, NativeProbeOperationState, NativeProbeUnknownEvidence,
    NativeRuntimeDoctorStore, NativeRuntimeDoctorStoreError,
};

fn digest(fill: char) -> String {
    std::iter::repeat_n(fill, 64).collect()
}

fn private_tempdir() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    root
}

fn intent(operation_id: &str) -> NativeProbeIntent {
    NativeProbeIntent::new(
        operation_id,
        "main",
        "00000000-0000-4000-8000-000000000001",
        digest('a'),
        digest('b'),
        digest('c'),
        digest('d'),
    )
    .unwrap()
}

fn sqlite_physical_bytes(database: &Path) -> u64 {
    ["", "-wal", "-shm"]
        .into_iter()
        .map(|suffix| {
            let mut path = database.as_os_str().to_owned();
            path.push(suffix);
            std::fs::metadata(path).map_or(0, |metadata| metadata.len())
        })
        .sum()
}

#[test]
fn journal_intent_is_unique_and_positive_job_id_is_durable_before_observation() {
    let root = private_tempdir();
    let database = root.path().join("doctor.sqlite3");
    let store = NativeRuntimeDoctorStore::open(&database).unwrap();
    let operation_id = "00000000-0000-4000-8000-000000000002";

    store.begin_probe(intent(operation_id)).unwrap();
    assert_eq!(
        store.operation_state(operation_id).unwrap(),
        Some(NativeProbeOperationState::Intent)
    );
    assert!(store.begin_probe(intent(operation_id)).is_err());
    assert!(
        store
            .begin_probe(intent("00000000-0000-4000-8000-000000000007"))
            .is_err()
    );

    store
        .record_job_accepted(operation_id, 481, &digest('e'))
        .unwrap();
    assert_eq!(
        store.operation_state(operation_id).unwrap(),
        Some(NativeProbeOperationState::Accepted { job_id: 481 })
    );
    assert!(
        store
            .record_job_accepted(operation_id, 0, &digest('e'))
            .is_err()
    );
    assert!(
        store
            .record_job_accepted(operation_id, 482, "not-a-digest")
            .is_err()
    );
    let connection = rusqlite::Connection::open(&database).unwrap();
    let run_schema_digest: String = connection
        .query_row(
            "SELECT run_schema_digest FROM native_runtime_probe WHERE operation_id = ?1",
            [operation_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(run_schema_digest, digest('e'));
}

#[test]
fn cancellation_claim_waiting_on_enqueue_is_dispatched_only_after_acceptance() {
    let root = private_tempdir();
    let store = NativeRuntimeDoctorStore::open(&root.path().join("doctor.sqlite3")).unwrap();
    let operation_id = "00000000-0000-4000-8000-000000000005";

    store.begin_probe(intent(operation_id)).unwrap();
    assert_eq!(
        store.claim_cancel(operation_id).unwrap(),
        lookback_tauri_lib::native_terminal::doctor_store::NativeProbeCancelClaim::PendingAcceptance
    );
    assert!(
        store
            .record_job_accepted(operation_id, 510, &digest('f'))
            .unwrap()
    );
    assert!(
        !store
            .record_job_accepted(operation_id, 510, &digest('f'))
            .unwrap(),
        "a recovered/ambiguous acceptance must not claim the same cancel twice"
    );
    assert_eq!(
        store.claim_cancel(operation_id).unwrap(),
        lookback_tauri_lib::native_terminal::doctor_store::NativeProbeCancelClaim::AlreadyClaimed
    );
}

#[test]
fn acceptance_persistence_can_be_retried_without_replaying_enqueue_or_losing_job_id() {
    let root = private_tempdir();
    let database = root.path().join("doctor.sqlite3");
    let store = NativeRuntimeDoctorStore::open(&database).unwrap();
    let operation_id = "00000000-0000-4000-8000-000000000009";
    store.begin_probe(intent(operation_id)).unwrap();
    assert_eq!(
        store.claim_cancel(operation_id).unwrap(),
        lookback_tauri_lib::native_terminal::doctor_store::NativeProbeCancelClaim::PendingAcceptance
    );

    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER fail_native_probe_accept
             BEFORE UPDATE OF state ON native_runtime_probe
             BEGIN SELECT RAISE(ABORT, 'injected acceptance persistence failure'); END;",
        )
        .unwrap();
    assert_eq!(
        store.record_job_accepted(operation_id, 521, &digest('8')),
        Err(NativeRuntimeDoctorStoreError::Database)
    );
    connection
        .execute_batch("DROP TRIGGER fail_native_probe_accept;")
        .unwrap();

    assert!(
        store
            .record_job_accepted(operation_id, 521, &digest('8'))
            .unwrap()
    );
    assert!(
        !store
            .record_job_accepted(operation_id, 521, &digest('8'))
            .unwrap()
    );
    assert_eq!(
        store.operation_state(operation_id).unwrap(),
        Some(NativeProbeOperationState::Accepted { job_id: 521 })
    );
    assert_eq!(
        store.claim_cancel(operation_id).unwrap(),
        lookback_tauri_lib::native_terminal::doctor_store::NativeProbeCancelClaim::AlreadyClaimed
    );
    assert_eq!(
        store.record_job_accepted(operation_id, 522, &digest('8')),
        Err(NativeRuntimeDoctorStoreError::Conflict),
        "a retry must not replace a different server-assigned identity"
    );
}

#[test]
fn known_job_identity_can_be_preserved_as_unknown_and_cancel_claimed_once() {
    let root = private_tempdir();
    let store = NativeRuntimeDoctorStore::open(&root.path().join("doctor.sqlite3")).unwrap();
    let operation_id = "00000000-0000-4000-8000-000000000010";
    store.begin_probe(intent(operation_id)).unwrap();
    assert_eq!(
        store.claim_cancel(operation_id).unwrap(),
        lookback_tauri_lib::native_terminal::doctor_store::NativeProbeCancelClaim::PendingAcceptance
    );

    assert!(
        store
            .record_unknown_job_id(operation_id, 523, &digest('9'))
            .unwrap()
    );
    assert_eq!(
        store.operation_state(operation_id).unwrap(),
        Some(NativeProbeOperationState::Unknown { job_id: Some(523) })
    );
    assert!(
        !store
            .record_unknown_job_id(operation_id, 523, &digest('9'))
            .unwrap()
    );
    assert_eq!(
        store.claim_cancel(operation_id).unwrap(),
        lookback_tauri_lib::native_terminal::doctor_store::NativeProbeCancelClaim::AlreadyClaimed
    );
}

#[test]
fn only_clean_parsed_probe_evidence_can_become_observed() {
    let root = private_tempdir();
    let database = root.path().join("doctor.sqlite3");
    let store = NativeRuntimeDoctorStore::open(&database).unwrap();
    let operation_id = "00000000-0000-4000-8000-000000000006";
    store.begin_probe(intent(operation_id)).unwrap();
    store
        .record_job_accepted(operation_id, 520, &digest('f'))
        .unwrap();

    assert!(
        store
            .finish_probe(
                operation_id,
                &NativeProbeEvidence {
                    stdout_sha256: digest('7'),
                    stdout_matches_pinned_version: true,
                    stderr_empty: true,
                    stream_error: false,
                    cli_version: "bad\nversion".to_owned(),
                    exit_code: 0,
                    end_received: true,
                    stream_eof: true,
                },
            )
            .is_err()
    );
    assert_eq!(
        store.operation_state(operation_id).unwrap(),
        Some(NativeProbeOperationState::Accepted { job_id: 520 })
    );
    for (stdout_matches_pinned_version, stderr_empty, stream_error) in
        [(false, true, false), (true, true, true)]
    {
        assert!(
            store
                .finish_probe(
                    operation_id,
                    &NativeProbeEvidence {
                        stdout_sha256: digest('7'),
                        stdout_matches_pinned_version,
                        stderr_empty,
                        stream_error,
                        cli_version: "0.200.0-nightly.4+linux".to_owned(),
                        exit_code: 0,
                        end_received: true,
                        stream_eof: true,
                    },
                )
                .is_err()
        );
    }
    assert!(
        store
            .finish_probe(
                operation_id,
                &NativeProbeEvidence {
                    stdout_sha256: digest('7'),
                    stdout_matches_pinned_version: true,
                    stderr_empty: false,
                    stream_error: false,
                    cli_version: "0.200.0-nightly.4+linux".to_owned(),
                    exit_code: 0,
                    end_received: true,
                    stream_eof: true,
                },
            )
            .unwrap()
    );
    assert_eq!(
        store.operation_state(operation_id).unwrap(),
        Some(NativeProbeOperationState::Observed { job_id: 520 })
    );
    drop(store);
    let reopened = NativeRuntimeDoctorStore::open(&database).unwrap();
    assert_eq!(
        reopened.operation_state(operation_id).unwrap(),
        Some(NativeProbeOperationState::Observed { job_id: 520 })
    );
    let connection = rusqlite::Connection::open(&database).unwrap();
    let stderr_empty: bool = connection
        .query_row(
            "SELECT stderr_empty FROM native_runtime_probe WHERE operation_id = ?1",
            [operation_id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        !stderr_empty,
        "the journal preserves the actual warning observation"
    );
}

#[test]
fn stream_failure_facts_are_preserved_only_on_an_unknown_job_identity() {
    let root = private_tempdir();
    let database = root.path().join("doctor.sqlite3");
    let store = NativeRuntimeDoctorStore::open(&database).unwrap();
    let operation_id = "00000000-0000-4000-8000-000000000013";
    store.begin_probe(intent(operation_id)).unwrap();
    store
        .record_job_accepted(operation_id, 526, &digest('f'))
        .unwrap();

    store
        .finish_probe_unknown(
            operation_id,
            &NativeProbeUnknownEvidence {
                stdout_sha256: Some(digest('7')),
                stdout_matches_pinned_version: Some(false),
                stderr_empty: Some(true),
                stream_error: Some(true),
                cli_version: None,
                exit_code: None,
                end_received: Some(false),
                stream_eof: Some(false),
            },
        )
        .unwrap();
    assert_eq!(
        store.operation_state(operation_id).unwrap(),
        Some(NativeProbeOperationState::Unknown { job_id: Some(526) })
    );

    let connection = rusqlite::Connection::open(database).unwrap();
    let (state, stream_error): (String, i64) = connection
        .query_row(
            "SELECT state, stream_error FROM native_runtime_probe WHERE operation_id = ?1",
            [operation_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, "unknown");
    assert_eq!(stream_error, 1);
}

#[test]
fn concurrent_cancel_claims_dispatch_at_most_once() {
    let root = private_tempdir();
    let database = root.path().join("doctor.sqlite3");
    let store = Arc::new(NativeRuntimeDoctorStore::open(&database).unwrap());
    let other_store = Arc::new(NativeRuntimeDoctorStore::open(&database).unwrap());
    let operation_id = "00000000-0000-4000-8000-000000000003";
    store.begin_probe(intent(operation_id)).unwrap();
    store
        .record_job_accepted(operation_id, 492, &digest('e'))
        .unwrap();

    let claimers = (0..12)
        .map(|index| {
            let store = if index % 2 == 0 {
                Arc::clone(&store)
            } else {
                Arc::clone(&other_store)
            };
            std::thread::spawn(move || store.claim_cancel(operation_id).unwrap())
        })
        .collect::<Vec<_>>();
    let dispatch_claims = claimers
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .filter(|claim| claim.dispatch_job_id() == Some(492))
        .count();

    assert_eq!(dispatch_claims, 1);
}

#[test]
fn reopening_never_replays_an_unresolved_probe_intent() {
    let root = private_tempdir();
    let database = root.path().join("doctor.sqlite3");
    let operation_id = "00000000-0000-4000-8000-000000000004";
    {
        let store = NativeRuntimeDoctorStore::open(&database).unwrap();
        store.begin_probe(intent(operation_id)).unwrap();
        store
            .record_job_accepted(operation_id, 503, &digest('e'))
            .unwrap();
    }

    let reopened = NativeRuntimeDoctorStore::open(&database).unwrap();
    assert_eq!(
        reopened.operation_state(operation_id).unwrap(),
        Some(NativeProbeOperationState::Unknown { job_id: Some(503) })
    );
    assert!(reopened.begin_probe(intent(operation_id)).is_err());
    assert!(
        reopened
            .begin_probe(intent("00000000-0000-4000-8000-000000000008"))
            .is_err()
    );
}

#[cfg(unix)]
#[test]
fn corrupt_journal_is_preserved_and_rejected_instead_of_reset() {
    let root = private_tempdir();
    let database = root.path().join("doctor.sqlite3");
    let bytes = b"not a sqlite database";
    std::fs::write(&database, bytes).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&database, std::fs::Permissions::from_mode(0o600)).unwrap();

    assert_eq!(
        NativeRuntimeDoctorStore::open(&database).unwrap_err(),
        NativeRuntimeDoctorStoreError::CorruptStore
    );
    assert_eq!(std::fs::read(database).unwrap(), bytes);
}

#[cfg(unix)]
#[test]
fn oversized_wal_is_rejected_before_sqlite_recovery() {
    let root = private_tempdir();
    let database = root.path().join("doctor.sqlite3");
    let wal = root.path().join("doctor.sqlite3-wal");
    std::fs::File::create(&wal)
        .unwrap()
        .set_len(32 * 1024 * 1024 + 1)
        .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&wal, std::fs::Permissions::from_mode(0o600)).unwrap();

    assert_eq!(
        NativeRuntimeDoctorStore::open(&database).unwrap_err(),
        NativeRuntimeDoctorStoreError::CapacityExceeded
    );
    assert_eq!(std::fs::metadata(&wal).unwrap().len(), 32 * 1024 * 1024 + 1);
}

#[cfg(unix)]
#[test]
fn open_reserves_checkpoint_and_recovery_growth_before_mutating_a_legacy_wal() {
    const MAX_BYTES: u64 = 32 * 1024 * 1024;
    const PREVIOUS_WRITE_RESERVE: u64 = 9 * 1024 * 1024;
    const FULL_WRITE_RESERVE: u64 = 17 * 1024 * 1024;
    const TARGET_PHYSICAL_BYTES: u64 = 24_000_000;
    const LEGAL_INTENT_ROWS: usize = 1_040;

    let root = private_tempdir();
    let database = root.path().join("doctor.sqlite3");
    let seed_id = "00000000-0000-4000-8000-000000000020";
    let store = NativeRuntimeDoctorStore::open(&database).unwrap();
    store.begin_probe(intent(seed_id)).unwrap();

    // Keep a read snapshot open while a legacy writer accumulates committed
    // WAL frames. This reproduces a WAL that could not be checkpointed by the
    // previous, smaller per-write reservation.
    let reader = rusqlite::Connection::open(&database).unwrap();
    reader.execute_batch("BEGIN DEFERRED;").unwrap();
    let _: i64 = reader
        .query_row("SELECT COUNT(*) FROM native_runtime_probe", [], |row| {
            row.get(0)
        })
        .unwrap();

    let writer = rusqlite::Connection::open(&database).unwrap();
    writer
        .execute_batch("PRAGMA wal_autocheckpoint = 0;")
        .unwrap();
    let cwd = format!("/{}", "x".repeat(4095));
    writer.execute_batch("BEGIN IMMEDIATE;").unwrap();
    {
        let mut insert = writer
            .prepare(
                "INSERT INTO native_runtime_probe (
                    operation_id, owner_window_label, preview_id, preview_digest,
                    context_digest, runtime_image_sha256, package_manifest_sha256,
                    sdk_config_sha256, worker_id, runner_id, worker_settings_digest,
                    matched_runner_schema_digest, prepared_cwd, state, created_at_ms
                 ) VALUES (?1, 'main', ?2, ?3, ?4, ?5, ?6, ?7, 1, 1, ?8, ?9,
                           ?10, 'intent', 1)",
            )
            .unwrap();
        for index in 0..LEGAL_INTENT_ROWS {
            let id = 1_000 + index as u64;
            let operation_id = format!("00000000-0000-4000-8000-{id:012x}");
            let preview_id = format!("00000000-0000-4000-8001-{id:012x}");
            insert
                .execute(rusqlite::params![
                    operation_id,
                    preview_id,
                    digest('a'),
                    digest('b'),
                    digest('c'),
                    digest('d'),
                    digest('e'),
                    digest('f'),
                    digest('7'),
                    cwd,
                ])
                .unwrap();
        }
    }
    writer.execute_batch("COMMIT;").unwrap();

    let page_size: i64 = writer
        .pragma_query_value(None, "page_size", |row| row.get(0))
        .unwrap();
    let page_count: i64 = writer
        .pragma_query_value(None, "page_count", |row| row.get(0))
        .unwrap();
    assert!(page_count > 0);
    assert!(page_size as u64 * page_count as u64 <= 8 * 1024 * 1024);

    let mut next_id = 1_000_u64;
    while sqlite_physical_bytes(&database) < TARGET_PHYSICAL_BYTES {
        writer.execute_batch("BEGIN IMMEDIATE;").unwrap();
        {
            let mut update = writer
                .prepare(
                    "UPDATE native_runtime_probe
                        SET context_digest = CASE WHEN context_digest = ?2 THEN ?3 ELSE ?2 END
                      WHERE operation_id = ?1",
                )
                .unwrap();
            // Small commits keep the pre-release WAL fixture inside the
            // aggregate cap while still putting its post-release source in
            // the narrow range that the previous reserve admitted.
            for _ in 0..10 {
                let operation_id = format!("00000000-0000-4000-8000-{next_id:012x}");
                update
                    .execute(rusqlite::params![operation_id, digest('b'), digest('c')])
                    .unwrap();
                next_id += 1;
                if next_id >= 1_000 + LEGAL_INTENT_ROWS as u64 {
                    next_id = 1_000;
                }
            }
        }
        writer.execute_batch("COMMIT;").unwrap();
        assert!(sqlite_physical_bytes(&database) < MAX_BYTES);
    }

    let before_open_bytes = sqlite_physical_bytes(&database);
    assert!(before_open_bytes >= TARGET_PHYSICAL_BYTES);
    assert!(before_open_bytes < MAX_BYTES);
    drop(writer);

    // Releasing the last reader allows a later open to recover/checkpoint this
    // WAL. The capacity rejection must happen before that update can commit.
    reader.execute_batch("ROLLBACK;").unwrap();
    drop(reader);
    let before_reopen = sqlite_physical_bytes(&database);
    assert!(before_reopen < MAX_BYTES);
    assert!(
        before_reopen + PREVIOUS_WRITE_RESERVE <= MAX_BYTES,
        "legacy source {before_reopen} must fit the previous reserve"
    );
    assert!(before_reopen + FULL_WRITE_RESERVE > MAX_BYTES);

    assert_eq!(
        NativeRuntimeDoctorStore::open(&database).unwrap_err(),
        NativeRuntimeDoctorStoreError::CapacityExceeded
    );
    assert_eq!(
        sqlite_physical_bytes(&database),
        before_reopen,
        "a rejected recovery must not checkpoint or append WAL frames"
    );

    let inspection = rusqlite::Connection::open(&database).unwrap();
    let non_intent_rows: i64 = inspection
        .query_row(
            "SELECT COUNT(*) FROM native_runtime_probe WHERE state != 'intent'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        non_intent_rows, 0,
        "capacity failure must not run recovery UPDATE"
    );
    let integrity: String = inspection
        .pragma_query_value(None, "integrity_check", |row| row.get(0))
        .unwrap();
    assert_eq!(integrity, "ok");
}

#[cfg(unix)]
#[test]
fn an_invalid_observed_row_in_any_journal_entry_rejects_the_whole_store() {
    let root = private_tempdir();
    let database = root.path().join("doctor.sqlite3");
    let operation_id = "00000000-0000-4000-8000-000000000011";
    let store = NativeRuntimeDoctorStore::open(&database).unwrap();
    store.begin_probe(intent(operation_id)).unwrap();
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute(
            "UPDATE native_runtime_probe SET state = 'observed', job_id = 524,
                    run_schema_digest = ?2, stdout_sha256 = NULL, cli_version = NULL,
                    exit_code = NULL, end_received = NULL, stream_eof = NULL
              WHERE operation_id = ?1",
            rusqlite::params![operation_id, digest('a')],
        )
        .unwrap();
    drop(connection);

    assert_eq!(
        store.operation_state("00000000-0000-4000-8000-000000000099"),
        Err(NativeRuntimeDoctorStoreError::CorruptStore),
        "reading one operation must first validate unrelated journal rows"
    );
    drop(store);
    assert_eq!(
        NativeRuntimeDoctorStore::open(&database).unwrap_err(),
        NativeRuntimeDoctorStoreError::CorruptStore,
        "a forged Observed row must not become an Unknown reusable intent"
    );
}

#[cfg(unix)]
#[test]
fn schema_migration_preserves_known_identity_but_downgrades_unverifiable_legacy_evidence() {
    let root = private_tempdir();
    let database = root.path().join("doctor.sqlite3");
    let operation_id = "00000000-0000-4000-8000-000000000012";
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE native_runtime_probe (
                operation_id TEXT PRIMARY KEY,
                owner_window_label TEXT NOT NULL,
                preview_id TEXT NOT NULL,
                preview_digest TEXT NOT NULL,
                context_digest TEXT NOT NULL,
                runtime_image_sha256 TEXT NOT NULL,
                package_manifest_sha256 TEXT NOT NULL,
                sdk_config_sha256 TEXT,
                worker_id INTEGER,
                runner_id INTEGER,
                worker_settings_digest TEXT,
                matched_runner_schema_digest TEXT,
                run_schema_digest TEXT,
                prepared_cwd TEXT,
                state TEXT NOT NULL CHECK (state IN ('intent', 'accepted', 'unknown', 'observed')),
                job_id INTEGER,
                cancel_requested INTEGER NOT NULL DEFAULT 0 CHECK (cancel_requested IN (0, 1)),
                stdout_sha256 TEXT,
                cli_version TEXT,
                exit_code INTEGER,
                end_received INTEGER CHECK (end_received IN (0, 1)),
                stream_eof INTEGER CHECK (stream_eof IN (0, 1)),
                created_at_ms INTEGER NOT NULL,
                CHECK (job_id IS NULL OR job_id > 0),
                CHECK (worker_id IS NULL OR worker_id > 0),
                CHECK (runner_id IS NULL OR runner_id > 0)
             ) STRICT;
             CREATE INDEX native_runtime_probe_preview
                ON native_runtime_probe(owner_window_label, preview_id, created_at_ms);
             PRAGMA user_version = 1;",
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO native_runtime_probe (
                operation_id, owner_window_label, preview_id, preview_digest,
                context_digest, runtime_image_sha256, package_manifest_sha256,
                state, job_id, run_schema_digest, stdout_sha256, cli_version,
                exit_code, end_received, stream_eof, created_at_ms
             ) VALUES (?1, 'main', ?2, ?3, ?4, ?5, ?6, 'observed', 525,
                       ?7, ?8, '1.18.3', 0, 1, 1, 1)",
            rusqlite::params![
                operation_id,
                "00000000-0000-4000-8000-000000000001",
                digest('a'),
                digest('b'),
                digest('c'),
                digest('d'),
                digest('e'),
                digest('f'),
            ],
        )
        .unwrap();
    drop(connection);
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&database, std::fs::Permissions::from_mode(0o600)).unwrap();

    let migrated = NativeRuntimeDoctorStore::open(&database).unwrap();
    assert_eq!(
        migrated.operation_state(operation_id).unwrap(),
        Some(NativeProbeOperationState::Unknown { job_id: Some(525) }),
        "legacy observations lack persisted stderr/transport facts and cannot remain Observed"
    );
    let connection = rusqlite::Connection::open(database).unwrap();
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 2);
}
