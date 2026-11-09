// The checked-in Memories CLI and Atlas executables are Linux x86_64 artifacts.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::path::{Path, PathBuf};
use std::process::Command;

use lookback_tauri_lib::data::DataPaths;
use lookback_tauri_lib::sidecar::memories_migration::{
    ApplyOutcome, BackupPolicy, MigrationEnvironment, run_local_apply,
};
use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};

const LEGACY_GROUP_COUNT: i64 = 337;
const LEGACY_CANONICAL_KEY_DIGEST: &str =
    "a4ce2f4dd2cffbd303f6393fcdfa567ba87111f0e9d99ab3207e160bd1e36bf2";
const LEGACY_OWNER_ID_DIGEST: &str =
    "87f3dfc9787f04ae0c50de5d9b322e68439fd666762597f9f7b81b3424d78947";
const THREAD_MESSAGE_TIMES_DIGEST: &str =
    "d2fe9980cf1048115967a8ab019a53c4422b284f3a8cf185ce2c55988506ed1d";
const SOURCE_EVIDENCE_ID: i64 = 700;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ObservationEvidenceSnapshot {
    id: i64,
    subject_source: String,
    subject_identity_scope_known: i64,
    subject_identity_scope_value: String,
    subject_owner_scope: String,
    subject_native_id: String,
    candidate_parent_present: i64,
    candidate_parent_source: String,
    candidate_parent_identity_scope_known: i64,
    candidate_parent_identity_scope_value: String,
    candidate_parent_owner_scope: String,
    candidate_parent_native_id: String,
    relation_kind: Option<String>,
    origin: String,
    evidence_kind: String,
    polarity: String,
    source_confidence: Option<String>,
    evidence_fingerprint: String,
    source_record_ref: Option<String>,
    state: String,
    import_run_id: Option<String>,
    observed_at: i64,
    created_at: i64,
    updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ThreadRelationSnapshot {
    id: i64,
    parent_thread_id: Option<i64>,
    child_thread_id: Option<i64>,
    parent_thread_canonical_key: String,
    child_thread_canonical_key: String,
    parent_owner_scope: String,
    parent_source: Option<String>,
    parent_identity_scope: Option<String>,
    parent_native_id: Option<String>,
    child_owner_scope: String,
    child_source: Option<String>,
    child_identity_scope: Option<String>,
    child_native_id: Option<String>,
    relation_type: String,
    state: String,
    selection_basis: String,
    source_confidence: Option<String>,
    selected_observation_id: Option<i64>,
    selected_operator_decision_id: Option<i64>,
    created_at: i64,
    updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MemoryRelationSnapshot {
    group_id: i64,
    memory_id: i64,
    purpose: String,
    on_group_delete: String,
    created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MemberSnapshot {
    group_id: i64,
    thread_id: Option<i64>,
    thread_canonical_key: String,
    owner_scope: String,
    source: Option<String>,
    identity_scope: Option<String>,
    native_id: Option<String>,
    state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DeletionMarkerSnapshot {
    owner_scope: String,
    source: String,
    identity_scope: String,
    native_id: String,
    forbid_reimport: i64,
    recursive: i64,
    actor_id: String,
    reason: Option<String>,
    deleted_at: i64,
}

fn bundled_cli() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("migration-bundle")
        .join("memories-db-migrate")
}

fn bundled_atlas() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("migration-bundle")
        .join("atlas")
}

fn url_for_directory(path: &Path) -> String {
    url::Url::from_directory_path(path)
        .expect("migration bundle path must be absolute")
        .to_string()
}

fn apply_official_legacy_migrations(data: &DataPaths, workspace: &Path) {
    let atlas = bundled_atlas();
    let migration_dir = atlas.join("sqlite").join("migrations");
    let output = Command::new(atlas.join("bin").join("atlas"))
        .current_dir(workspace)
        .args([
            "migrate",
            "apply",
            "--dir",
            &url_for_directory(&migration_dir),
            "--url",
            &data.memories_sqlite_url(),
            "--to-version",
            "20260926000001",
        ])
        .output()
        .expect("run the bundled Atlas executable against the isolated SQLite fixture");
    assert!(
        output.status.success(),
        "official legacy migrations failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn owner_for_thread(id: i64) -> i64 {
    if id % 2 == 0 { 2 } else { 1 }
}

fn owner_scope_user_id(owner_scope: &str) -> i64 {
    owner_scope
        .strip_prefix("user:")
        .expect("rehearsal owner scopes use the persisted user:<id> representation")
        .parse()
        .expect("rehearsal owner scope contains a numeric user ID")
}

fn canonical_key(id: i64) -> String {
    format!("{:064x}", 100_000 + id)
}

fn source_evidence_fingerprint() -> String {
    format!(
        "{:x}",
        Sha256::digest(b"thread-group-owner-cli-rehearsal-evidence-v1")
    )
}

fn observation_evidence_snapshots(
    connection: &Connection,
) -> rusqlite::Result<Vec<ObservationEvidenceSnapshot>> {
    connection
        .prepare(
            "SELECT id, subject_source, subject_identity_scope_known,
                    subject_identity_scope_value, subject_owner_scope, subject_native_id,
                    candidate_parent_present, candidate_parent_source,
                    candidate_parent_identity_scope_known,
                    candidate_parent_identity_scope_value, candidate_parent_owner_scope,
                    candidate_parent_native_id, relation_kind, origin, evidence_kind,
                    polarity, source_confidence, evidence_fingerprint, source_record_ref,
                    state, import_run_id, observed_at, created_at, updated_at
             FROM thread_observation ORDER BY id",
        )?
        .query_map([], |row| {
            Ok(ObservationEvidenceSnapshot {
                id: row.get(0)?,
                subject_source: row.get(1)?,
                subject_identity_scope_known: row.get(2)?,
                subject_identity_scope_value: row.get(3)?,
                subject_owner_scope: row.get(4)?,
                subject_native_id: row.get(5)?,
                candidate_parent_present: row.get(6)?,
                candidate_parent_source: row.get(7)?,
                candidate_parent_identity_scope_known: row.get(8)?,
                candidate_parent_identity_scope_value: row.get(9)?,
                candidate_parent_owner_scope: row.get(10)?,
                candidate_parent_native_id: row.get(11)?,
                relation_kind: row.get(12)?,
                origin: row.get(13)?,
                evidence_kind: row.get(14)?,
                polarity: row.get(15)?,
                source_confidence: row.get(16)?,
                evidence_fingerprint: row.get(17)?,
                source_record_ref: row.get(18)?,
                state: row.get(19)?,
                import_run_id: row.get(20)?,
                observed_at: row.get(21)?,
                created_at: row.get(22)?,
                updated_at: row.get(23)?,
            })
        })?
        .collect()
}

fn thread_relation_snapshots(
    connection: &Connection,
) -> rusqlite::Result<Vec<ThreadRelationSnapshot>> {
    connection
        .prepare(
            "SELECT id, parent_thread_id, child_thread_id,
                    parent_thread_canonical_key, child_thread_canonical_key,
                    parent_owner_scope, parent_source, parent_identity_scope, parent_native_id,
                    child_owner_scope, child_source, child_identity_scope, child_native_id,
                    relation_type, state, selection_basis, source_confidence,
                    selected_observation_id, selected_operator_decision_id,
                    created_at, updated_at
             FROM thread_relation ORDER BY id",
        )?
        .query_map([], |row| {
            Ok(ThreadRelationSnapshot {
                id: row.get(0)?,
                parent_thread_id: row.get(1)?,
                child_thread_id: row.get(2)?,
                parent_thread_canonical_key: row.get(3)?,
                child_thread_canonical_key: row.get(4)?,
                parent_owner_scope: row.get(5)?,
                parent_source: row.get(6)?,
                parent_identity_scope: row.get(7)?,
                parent_native_id: row.get(8)?,
                child_owner_scope: row.get(9)?,
                child_source: row.get(10)?,
                child_identity_scope: row.get(11)?,
                child_native_id: row.get(12)?,
                relation_type: row.get(13)?,
                state: row.get(14)?,
                selection_basis: row.get(15)?,
                source_confidence: row.get(16)?,
                selected_observation_id: row.get(17)?,
                selected_operator_decision_id: row.get(18)?,
                created_at: row.get(19)?,
                updated_at: row.get(20)?,
            })
        })?
        .collect()
}

fn memory_relation_snapshots(
    connection: &Connection,
) -> rusqlite::Result<Vec<MemoryRelationSnapshot>> {
    connection
        .prepare(
            "SELECT group_id, memory_id, purpose, on_group_delete, created_at
             FROM thread_group_memory_relation ORDER BY group_id, memory_id",
        )?
        .query_map([], |row| {
            Ok(MemoryRelationSnapshot {
                group_id: row.get(0)?,
                memory_id: row.get(1)?,
                purpose: row.get(2)?,
                on_group_delete: row.get(3)?,
                created_at: row.get(4)?,
            })
        })?
        .collect()
}

fn member_snapshots(connection: &Connection) -> rusqlite::Result<Vec<MemberSnapshot>> {
    connection
        .prepare(
            "SELECT group_id, thread_id, thread_canonical_key, owner_scope,
                    source, identity_scope, native_id, state
             FROM thread_group_member ORDER BY group_id, state, thread_canonical_key",
        )?
        .query_map([], |row| {
            Ok(MemberSnapshot {
                group_id: row.get(0)?,
                thread_id: row.get(1)?,
                thread_canonical_key: row.get(2)?,
                owner_scope: row.get(3)?,
                source: row.get(4)?,
                identity_scope: row.get(5)?,
                native_id: row.get(6)?,
                state: row.get(7)?,
            })
        })?
        .collect()
}

fn deletion_marker_snapshots(
    connection: &Connection,
) -> rusqlite::Result<Vec<DeletionMarkerSnapshot>> {
    connection
        .prepare(
            "SELECT owner_scope, source, identity_scope, native_id, forbid_reimport,
                    recursive, actor_id, reason, deleted_at
             FROM thread_deletion_marker
             ORDER BY owner_scope, source, identity_scope, native_id",
        )?
        .query_map([], |row| {
            Ok(DeletionMarkerSnapshot {
                owner_scope: row.get(0)?,
                source: row.get(1)?,
                identity_scope: row.get(2)?,
                native_id: row.get(3)?,
                forbid_reimport: row.get(4)?,
                recursive: row.get(5)?,
                actor_id: row.get(6)?,
                reason: row.get(7)?,
                deleted_at: row.get(8)?,
            })
        })?
        .collect()
}

fn insert_completed_task_state(
    connection: &Connection,
    identity: &str,
    digest: &str,
) -> rusqlite::Result<()> {
    connection.execute(
        "INSERT INTO memories_data_migration_task_state (
             task_identity, canonical_definition_digest, state, fencing_token,
             attempt_count, started_at, updated_at, completed_at
         ) VALUES (?1, ?2, 'completed', 1, 1, 1, 1, 1)",
        params![identity, digest],
    )?;
    Ok(())
}

fn seed_representative_legacy_data(data: &DataPaths) {
    let connection = Connection::open(data.memories_sqlite_path()).unwrap();
    let transaction = connection.unchecked_transaction().unwrap();
    for id in 1..=LEGACY_GROUP_COUNT {
        let owner_id = owner_for_thread(id);
        let key = canonical_key(id);
        transaction
            .execute(
                "INSERT INTO thread (id, user_id, created_at, updated_at, memory_kind)
                 VALUES (?1, ?2, 10, 20, 1)",
                params![id, owner_id],
            )
            .unwrap();
        transaction
            .execute(
                "INSERT INTO thread_canonical_key
                 (thread_id, owner_scope, key, origin, assigned_at)
                 VALUES (?1, ?2, ?3, 'creation_uuid', 10)",
                params![id, format!("user:{owner_id}"), key],
            )
            .unwrap();
        transaction
            .execute(
                "INSERT INTO thread_group (
                     id, group_canonical_key, title, status, grouping_authority,
                     redirect_to_group_id, created_at, updated_at
                 ) VALUES (?1, ?2, ?3, 'active', 'operator', NULL, 10, 20)",
                params![id, format!("{:064x}", 200_000 + id), format!("group-{id}")],
            )
            .unwrap();
        transaction
            .execute(
                "INSERT INTO thread_group_member (
                     group_id, thread_id, thread_canonical_key, owner_scope,
                     role, state, provenance, created_at, updated_at
                 ) VALUES (?1, ?1, ?2, ?3, 'root', 'active', 'operator', 10, 20)",
                params![id, key, format!("user:{owner_id}")],
            )
            .unwrap();
    }

    let additional_member_thread = LEGACY_GROUP_COUNT + 1;
    let additional_member_key = canonical_key(additional_member_thread);
    transaction
        .execute(
            "INSERT INTO thread (id, user_id, created_at, updated_at, memory_kind)
             VALUES (?1, 2, 10, 20, 1)",
            [additional_member_thread],
        )
        .unwrap();
    transaction
        .execute(
            "INSERT INTO thread_canonical_key
             (thread_id, owner_scope, key, origin, assigned_at)
             VALUES (?1, 'user:2', ?2, 'creation_uuid', 10)",
            params![additional_member_thread, additional_member_key],
        )
        .unwrap();
    transaction
        .execute(
            "INSERT INTO thread_group_member (
                 group_id, thread_id, thread_canonical_key, owner_scope,
                 role, state, provenance, created_at, updated_at
             ) VALUES (1, ?1, ?2, 'user:2', 'member', 'active', 'operator', 10, 20)",
            params![additional_member_thread, additional_member_key],
        )
        .unwrap();

    let deleted_key = format!("{:064x}", 900_337);
    transaction
        .execute(
            "INSERT INTO thread_group_member (
                 group_id, thread_id, thread_canonical_key, owner_scope,
                 source, identity_scope, native_id, role, state, provenance,
                 deleted_at, created_at, updated_at
             ) VALUES (?1, NULL, ?2, 'user:2', 'codex', 'default',
                       'deleted-thread-337', 'member', 'deleted', 'operator', 30, 10, 20)",
            params![LEGACY_GROUP_COUNT, deleted_key],
        )
        .unwrap();
    let evidence_fingerprint = source_evidence_fingerprint();
    transaction
        .execute(
            "INSERT INTO thread_observation (
                 id, subject_source, subject_identity_scope_known,
                 subject_identity_scope_value, subject_owner_scope, subject_native_id,
                 candidate_parent_present, candidate_parent_source,
                 candidate_parent_identity_scope_known, candidate_parent_identity_scope_value,
                 candidate_parent_owner_scope, candidate_parent_native_id, relation_kind,
                 origin, evidence_kind, polarity, source_confidence, evidence_fingerprint,
                 source_record_ref, state, observed_at, created_at, updated_at
             ) VALUES (
                 ?1, 'codex', 1, 'default', 'user:2', 'thread-2', 1,
                 'codex', 1, 'default', 'user:1', 'thread-1', 'fork',
                 'adapter', 'source_field', 'supports', 'exact', ?2,
                 'fixture:thread-2-parent-thread-1', 'selected', 10, 10, 20
             )",
            params![SOURCE_EVIDENCE_ID, evidence_fingerprint],
        )
        .unwrap();
    transaction
        .execute(
            "INSERT INTO thread_relation (
                 id, parent_thread_id, child_thread_id,
                 parent_thread_canonical_key, child_thread_canonical_key,
                 parent_owner_scope, parent_source, parent_identity_scope, parent_native_id,
                 child_owner_scope, child_source, child_identity_scope, child_native_id,
                 relation_type, state, selection_basis, source_confidence,
                 selected_observation_id, created_at, updated_at
             ) VALUES (500, 1, 2, ?1, ?2, 'user:1', 'codex', 'default', 'thread-1',
                       'user:2', 'codex', 'default', 'thread-2', 'fork', 'active',
                       'source_exact', 'exact', ?3, 10, 20)",
            params![canonical_key(1), canonical_key(2), SOURCE_EVIDENCE_ID],
        )
        .unwrap();
    transaction
        .execute(
            "INSERT INTO thread_group_memory_relation
             (group_id, memory_id, purpose, on_group_delete, created_at)
             VALUES (1, 910, 'rehearsal', 'retain', 10)",
            [],
        )
        .unwrap();
    transaction
        .execute(
            "INSERT INTO thread_deletion_marker (
                 owner_scope, source, identity_scope, native_id, forbid_reimport,
                 recursive, actor_id, reason, deleted_at
             ) VALUES ('user:2', 'codex', 'default', 'deleted-thread-337',
                       1, 0, 'rehearsal', 'preserve marker', 30)",
            [],
        )
        .unwrap();

    insert_completed_task_state(
        &transaction,
        "thread-message-times-v1@1",
        THREAD_MESSAGE_TIMES_DIGEST,
    )
    .unwrap();
    insert_completed_task_state(
        &transaction,
        "thread-groups-canonical-keys-v1@1",
        LEGACY_CANONICAL_KEY_DIGEST,
    )
    .unwrap();
    insert_completed_task_state(
        &transaction,
        "thread-groups-user-ids-v1@1",
        LEGACY_OWNER_ID_DIGEST,
    )
    .unwrap();
    transaction.commit().unwrap();
}

fn scalar_i64(connection: &Connection, sql: &str) -> i64 {
    connection.query_row(sql, [], |row| row.get(0)).unwrap()
}

fn scalar_i64_with_text_param(connection: &Connection, sql: &str, value: &str) -> i64 {
    connection
        .query_row(sql, [value], |row| row.get(0))
        .unwrap()
}

fn selected_evidence_link_count(connection: &Connection) -> i64 {
    scalar_i64_with_text_param(
        connection,
        "SELECT COUNT(*) FROM thread_relation relation
         JOIN thread_observation observation
           ON observation.id = relation.selected_observation_id
         WHERE relation.id = 500
           AND relation.parent_thread_id = 1 AND relation.child_thread_id = 2
           AND relation.selection_basis = 'source_exact'
           AND observation.id = 700 AND observation.state = 'selected'
           AND observation.subject_native_id = 'thread-2'
           AND observation.candidate_parent_native_id = 'thread-1'
           AND observation.evidence_fingerprint = ?1",
        &source_evidence_fingerprint(),
    )
}

#[tokio::test]
async fn populated_official_legacy_schema_migrates_without_owner_inventory_json() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("cli-workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let data = DataPaths::with_root(temp.path().join("data"));
    data.ensure().unwrap();

    apply_official_legacy_migrations(&data, &workspace);
    seed_representative_legacy_data(&data);

    let connection = Connection::open(data.memories_sqlite_path()).unwrap();
    let before_groups: Vec<(i64, String, String)> = connection
        .prepare("SELECT id, group_canonical_key, title FROM thread_group ORDER BY id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let before_members = member_snapshots(&connection).unwrap();
    let before_thread_keys: Vec<(i64, String)> = connection
        .prepare("SELECT thread_id, key FROM thread_canonical_key ORDER BY thread_id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let before_legacy_task_rows: Vec<(String, String, String, Option<i64>)> = connection
        .prepare(
            "SELECT task_identity, canonical_definition_digest, state, completed_at
             FROM memories_data_migration_task_state ORDER BY task_identity",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let before_evidence = observation_evidence_snapshots(&connection).unwrap();
    assert_eq!(
        before_evidence.len(),
        1,
        "the legacy rehearsal must include a representative persisted observation and evidence fingerprint"
    );
    assert_eq!(
        before_evidence[0].evidence_fingerprint,
        source_evidence_fingerprint()
    );
    assert_eq!(before_evidence[0].state, "selected");
    assert_eq!(before_evidence[0].subject_owner_scope, "user:2");
    assert_eq!(before_evidence[0].candidate_parent_owner_scope, "user:1");
    assert_eq!(before_evidence[0].evidence_fingerprint.len(), 64);
    assert!(
        before_evidence[0]
            .evidence_fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    );
    let before_relations = thread_relation_snapshots(&connection).unwrap();
    assert_eq!(before_relations.len(), 1);
    assert_eq!(
        before_relations[0].selected_observation_id,
        Some(SOURCE_EVIDENCE_ID),
        "the persisted relation must point at its selected observation"
    );
    assert_eq!(
        selected_evidence_link_count(&connection),
        1,
        "the selected relation must reference the exact persisted evidence row"
    );
    let before_memory_relations = memory_relation_snapshots(&connection).unwrap();
    let before_deletion_markers = deletion_marker_snapshots(&connection).unwrap();
    assert_eq!(before_groups.len() as i64, LEGACY_GROUP_COUNT);
    assert_eq!(before_members.len() as i64, LEGACY_GROUP_COUNT + 2);
    assert_eq!(before_thread_keys.len() as i64, LEGACY_GROUP_COUNT + 1);
    assert_eq!(before_legacy_task_rows.len(), 3);
    assert!(!data.root.join("thread-group-owner-inventory.json").exists());
    drop(connection);

    let env = MigrationEnvironment {
        database_url: data.memories_sqlite_url(),
        thread_vector_enabled: false,
        memory_fts_tokenizer: "simple".into(),
        lance_language_model_home: data.lance_language_model_home(),
        extra_env: Default::default(),
    };
    // Files left by the removed startup gate must go once a migration runs.
    std::fs::write(data.database_migration_receipt_path(), b"{}").unwrap();
    std::fs::write(data.database_migration_attempt_path(), b"{}").unwrap();
    let backups = data.database_migration_backups_dir();
    let policy = BackupPolicy::Backup {
        dir: backups.clone(),
        keep: 1,
    };
    let backup_count = || std::fs::read_dir(&backups).map_or(0, |entries| entries.count());

    let outcome = run_local_apply(&bundled_cli(), &env, &policy, &data, &mut || {})
        .await
        .expect("the bundled tool migrates a populated legacy schema without an owner inventory");
    assert_eq!(outcome, ApplyOutcome::Migrated);
    assert!(!data.root.join("thread-group-owner-inventory.json").exists());
    assert!(!data.database_migration_receipt_path().exists());
    assert!(!data.database_migration_attempt_path().exists());
    assert_eq!(
        backup_count(),
        1,
        "a migration that changes data leaves one backup"
    );
    assert_eq!(
        scalar_i64(
            &Connection::open(data.memories_sqlite_path()).unwrap(),
            "SELECT COUNT(*) FROM thread_group WHERE user_id = 1",
        ),
        LEGACY_GROUP_COUNT,
        "the official SQLite migration assigns historical groups to owner 1"
    );

    {
        let connection = Connection::open(data.memories_sqlite_path()).unwrap();
        connection
            .execute("UPDATE thread_group SET user_id = 7 WHERE id = 7", [])
            .unwrap();
    }
    let outcome = run_local_apply(&bundled_cli(), &env, &policy, &data, &mut || {})
        .await
        .expect("an up-to-date database needs no migration");
    assert_eq!(outcome, ApplyOutcome::NoOp);
    assert_eq!(backup_count(), 1, "a no-op startup takes no backup");

    let connection = Connection::open(data.memories_sqlite_path()).unwrap();
    assert_eq!(
        scalar_i64(&connection, "SELECT COUNT(*) FROM thread_group"),
        LEGACY_GROUP_COUNT
    );
    assert_eq!(
        scalar_i64(
            &connection,
            "SELECT COUNT(*) FROM thread_group WHERE user_id = 1"
        ),
        LEGACY_GROUP_COUNT - 1
    );
    assert_eq!(
        scalar_i64(
            &connection,
            "SELECT COUNT(*) FROM thread_group WHERE user_id = 7"
        ),
        1
    );
    assert_eq!(
        scalar_i64(&connection, "SELECT COUNT(*) FROM thread_group_member"),
        LEGACY_GROUP_COUNT + 2
    );
    assert_eq!(
        scalar_i64(
            &connection,
            "SELECT COUNT(*) FROM thread_group_member WHERE user_id = 1 AND owner_scope = 'user:1'",
        ),
        169
    );
    assert_eq!(
        scalar_i64(
            &connection,
            "SELECT COUNT(*) FROM thread_group_member WHERE user_id = 2 AND owner_scope = 'user:2'",
        ),
        170,
        "member owner IDs are converted from their original owner scopes"
    );
    let after_groups: Vec<(i64, String, String)> = connection
        .prepare("SELECT id, group_canonical_key, title FROM thread_group ORDER BY id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let after_members = member_snapshots(&connection).unwrap();
    let after_thread_keys: Vec<(i64, String)> = connection
        .prepare("SELECT thread_id, key FROM thread_canonical_key ORDER BY thread_id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let after_evidence = observation_evidence_snapshots(&connection).unwrap();
    let after_relations = thread_relation_snapshots(&connection).unwrap();
    let after_memory_relations = memory_relation_snapshots(&connection).unwrap();
    let after_deletion_markers = deletion_marker_snapshots(&connection).unwrap();
    assert_eq!(
        after_groups, before_groups,
        "group identity and payload are preserved"
    );
    assert_eq!(
        after_members, before_members,
        "member identity and deleted markers are preserved"
    );
    assert_eq!(
        after_thread_keys, before_thread_keys,
        "saved canonical keys are immutable"
    );
    assert_eq!(
        after_evidence, before_evidence,
        "selected observation payload and evidence fingerprint are unchanged"
    );
    assert_eq!(
        after_relations, before_relations,
        "relation endpoints, source identities, and selected observation reference are unchanged"
    );
    assert_eq!(
        after_memory_relations, before_memory_relations,
        "group-memory relation identity and policy are unchanged"
    );
    assert_eq!(
        after_deletion_markers, before_deletion_markers,
        "deletion marker identity, policy flags, and audit metadata are unchanged"
    );
    assert_eq!(
        scalar_i64(&connection, "SELECT COUNT(*) FROM thread_relation"),
        1
    );
    let typed_relation: (Option<i64>, Option<i64>, Option<i64>) = connection
        .query_row(
            "SELECT parent_user_id, child_user_id, selected_observation_id
             FROM thread_relation WHERE id = 500",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        typed_relation,
        (
            Some(owner_scope_user_id(&before_relations[0].parent_owner_scope)),
            Some(owner_scope_user_id(&before_relations[0].child_owner_scope)),
            before_relations[0].selected_observation_id,
        ),
        "typed relation endpoints must preserve their original owner-scope semantics"
    );
    let typed_evidence_owners: (Option<i64>, Option<i64>) = connection
        .query_row(
            "SELECT subject_user_id, candidate_parent_user_id
             FROM thread_observation WHERE id = ?1",
            [SOURCE_EVIDENCE_ID],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        typed_evidence_owners,
        (
            Some(owner_scope_user_id(&before_evidence[0].subject_owner_scope)),
            Some(owner_scope_user_id(
                &before_evidence[0].candidate_parent_owner_scope
            )),
        ),
        "typed observation endpoints must preserve their original owner-scope semantics"
    );
    assert_eq!(selected_evidence_link_count(&connection), 1);
    assert_eq!(after_memory_relations.len(), 1);
    assert_eq!(after_deletion_markers.len(), 1);
    assert_eq!(
        scalar_i64(
            &connection,
            "SELECT COUNT(*) FROM thread_deletion_marker
             WHERE owner_scope = 'user:2' AND user_id = 2
               AND forbid_reimport = 1 AND recursive = 0",
        ),
        1
    );
    let typed_deletion_marker: Option<i64> = connection
        .query_row(
            "SELECT user_id FROM thread_deletion_marker
             WHERE owner_scope = 'user:2' AND source = 'codex'
               AND identity_scope = 'default' AND native_id = 'deleted-thread-337'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        typed_deletion_marker,
        Some(owner_scope_user_id(&before_deletion_markers[0].owner_scope))
    );
    let after_legacy_task_rows: Vec<(String, String, String, Option<i64>)> = connection
        .prepare(
            "SELECT task_identity, canonical_definition_digest, state, completed_at
             FROM memories_data_migration_task_state
             WHERE task_identity IN (
                 'thread-message-times-v1@1',
                 'thread-groups-canonical-keys-v1@1',
                 'thread-groups-user-ids-v1@1'
             ) ORDER BY task_identity",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(after_legacy_task_rows, before_legacy_task_rows);

    let group_owner_column = connection
        .prepare("PRAGMA table_info(thread_group)")
        .unwrap()
        .query_map([], |row| {
            let name: String = row.get(1)?;
            if name == "user_id" {
                Ok(Some((
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<String>>(4)?,
                )))
            } else {
                Ok(None)
            }
        })
        .unwrap()
        .filter_map(|row| row.transpose())
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(
        group_owner_column,
        [(1, None)],
        "user_id is required, not DEFAULT 1"
    );

    println!(
        "CLI rehearsal passed: bundled memories-db-migrate local apply; groups={}; members={}; thread_relations=1; memory_relations=1; deletion_markers=1; retained_task_history={}",
        scalar_i64(&connection, "SELECT COUNT(*) FROM thread_group"),
        scalar_i64(&connection, "SELECT COUNT(*) FROM thread_group_member"),
        after_legacy_task_rows.len()
    );
}
