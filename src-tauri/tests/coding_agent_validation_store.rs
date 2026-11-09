use std::fs;

use lookback_tauri_lib::coding_agent::ValidationEvidenceStore;
#[cfg(unix)]
use lookback_tauri_lib::coding_agent::{
    ApprovedValidationPlan, ValidationOutcome, ValidationTargetSnapshot, record_validation_not_run,
};
#[cfg(unix)]
use std::collections::BTreeMap;
use tempfile::TempDir;

#[cfg(unix)]
fn private_dir(parent: &TempDir, name: &str) -> std::path::PathBuf {
    let path = parent.path().join(name);
    fs::create_dir(&path).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    path
}

#[cfg(unix)]
fn plan() -> ApprovedValidationPlan {
    let target =
        ValidationTargetSnapshot::new("a".repeat(40), "b".repeat(40), "c".repeat(64)).unwrap();
    ApprovedValidationPlan::new_for_worker(
        "run-store",
        "attempt-store",
        "validation-store",
        false,
        "sandbox-worker",
        vec!["cargo".to_owned(), "test".to_owned()],
        "/workspace/source",
        BTreeMap::new(),
        "d".repeat(64),
        Some(30_000),
        target,
    )
    .unwrap()
}

#[test]
#[cfg(unix)]
fn not_run_validation_record_survives_reopening_without_becoming_a_receipt() {
    let temp = TempDir::new().unwrap();
    let root = private_dir(&temp, "native-store");
    let guest_root = private_dir(&temp, "guest-workspace");
    let approved = plan();

    let store = ValidationEvidenceStore::open(&root, std::slice::from_ref(&guest_root)).unwrap();
    let not_run = record_validation_not_run(&approved);
    store.persist_validation_record(&not_run).unwrap();
    drop(store);

    let reopened = ValidationEvidenceStore::open(&root, &[guest_root]).unwrap();
    let records = reopened.load_validation_records(&approved).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].outcome(), ValidationOutcome::NotRun);
    assert!(records[0].receipt().is_none());
    assert!(
        reopened
            .find_reusable_validation(&approved)
            .unwrap()
            .is_none()
    );
}

#[test]
#[cfg(unix)]
fn duplicate_keys_and_unknown_versions_are_rejected_during_record_recovery() {
    let temp = TempDir::new().unwrap();
    let root = private_dir(&temp, "native-store");
    let guest_root = private_dir(&temp, "guest-workspace");
    let approved = plan();
    let store = ValidationEvidenceStore::open(&root, std::slice::from_ref(&guest_root)).unwrap();
    store
        .persist_validation_record(&record_validation_not_run(&approved))
        .unwrap();

    let records_scope = fs::read_dir(root.join("records"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let record_dir = fs::read_dir(records_scope)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let record_file = record_dir.join("record.json");
    let bytes = fs::read(&record_file).unwrap();
    let original = String::from_utf8(bytes).unwrap();
    let duplicated = original.replacen('{', "{\"schema_version\":1,", 1);
    fs::write(&record_file, duplicated).unwrap();
    assert!(store.load_validation_records(&approved).is_err());

    let unique_record = serde_json::json!({
        "schema_version": 99,
        "run_id": "run-store",
        "attempt_id": "attempt-store",
        "validation_id": "validation-store",
        "required": false,
        "binding_digest": approved.binding_digest(),
        "outcome": "not_run",
        "child_job_id": null,
        "sandbox_id": null,
        "exit_code": null,
        "execution_time_ms": null,
        "end_observed": false,
        "transport_error_observed": false,
        "receipt_id": null
    });
    fs::write(&record_file, serde_json::to_vec(&unique_record).unwrap()).unwrap();
    assert!(store.load_validation_records(&approved).is_err());
}

#[test]
#[cfg(unix)]
fn evidence_root_must_be_private_and_disjoint_from_guest_writable_roots() {
    let temp = TempDir::new().unwrap();
    let missing = temp.path().join("not-created-by-the-store");
    assert!(ValidationEvidenceStore::open(&missing, &[]).is_err());
    assert!(!missing.exists());

    let root = private_dir(&temp, "private-store");
    let guest_root = private_dir(&temp, "guest-workspace");

    assert!(ValidationEvidenceStore::open(&root, std::slice::from_ref(&guest_root)).is_ok());
    assert!(ValidationEvidenceStore::open(&root, &[temp.path().to_path_buf()]).is_err());

    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(ValidationEvidenceStore::open(&root, &[guest_root]).is_err());
    }
}

#[cfg(unix)]
#[test]
fn evidence_root_with_an_untrusted_writable_parent_is_rejected() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().unwrap();
    let parent = temp.path().join("shared-parent");
    let root = parent.join("private-store");
    fs::create_dir(&parent).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o777)).unwrap();
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();

    assert!(ValidationEvidenceStore::open(&root, &[]).is_err());
}

#[test]
#[cfg(unix)]
fn reopening_a_partially_missing_store_layout_fails_closed() {
    let temp = TempDir::new().unwrap();
    let root = private_dir(&temp, "native-store");
    let store = ValidationEvidenceStore::open(&root, &[]).unwrap();
    drop(store);
    fs::remove_dir_all(root.join("records")).unwrap();

    assert!(ValidationEvidenceStore::open(&root, &[]).is_err());
}

#[cfg(not(unix))]
#[test]
fn evidence_store_reports_unsupported_platforms() {
    use lookback_tauri_lib::coding_agent::ValidationEvidenceStoreError;

    let temp = TempDir::new().unwrap();
    let root = temp.path().join("native-store");
    fs::create_dir(&root).unwrap();

    assert!(matches!(
        ValidationEvidenceStore::open(&root, &[]),
        Err(ValidationEvidenceStoreError::UnsupportedPlatform)
    ));
}

#[cfg(unix)]
#[test]
fn symlinked_evidence_root_is_rejected() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().unwrap();
    let real = private_dir(&temp, "real-store");
    let alias = temp.path().join("store-link");
    symlink(&real, &alias).unwrap();

    assert!(ValidationEvidenceStore::open(&alias, &[]).is_err());
}
