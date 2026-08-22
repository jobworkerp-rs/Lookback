//! Contract checks for the internal Issue tracker manager Phase 2 boundary.
//!
//! These checks intentionally inspect the local implementation documents and
//! the sibling runner source.  They make the completion gate executable while
//! the manager Gateway is still an internal, unexposed integration surface.

use std::{fs, path::PathBuf};

fn app_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("src-tauri must have an application root")
        .to_path_buf()
}

fn read_app_doc(name: &str) -> Option<String> {
    fs::read_to_string(app_root().join("ai-docs").join(name)).ok()
}

fn runner_root() -> Option<PathBuf> {
    std::env::var_os("ISSUE_TRACKER_RUNNER_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            app_root()
                .parent()
                .map(|root| root.join("issue-tracker-runner"))
        })
        .filter(|root| root.is_dir())
}

#[test]
fn phase2_documents_define_the_foundation_as_incomplete() {
    let Some(plan) = read_app_doc("internal-issue-tracker-implementation-plan_ja.md") else {
        return;
    };
    let Some(design) = read_app_doc("internal-issue-tracker-implementation-design_ja.md") else {
        return;
    };

    for text in [plan, design] {
        assert!(
            text.contains("foundation") || text.contains("基盤"),
            "Phase 2 documents must state the foundation status"
        );
        assert!(
            text.contains("未完了") || text.contains("未実装"),
            "Phase 2 documents must distinguish foundation from completion"
        );
    }
}

#[test]
fn phase2_documents_define_all_completion_gate_categories() {
    let Some(plan) = read_app_doc("internal-issue-tracker-implementation-plan_ja.md") else {
        return;
    };
    let required_terms = [
        "Manager SQLite repository",
        "内部 Gateway",
        "outbox",
        "再起動",
        "冪等",
        "canonical Issue snapshot",
        "PluginV2",
        "MCP",
    ];

    for term in required_terms {
        assert!(
            plan.contains(term),
            "Phase 2 completion gate is missing required term: {term}"
        );
    }
}

#[test]
fn manager_source_keeps_gateway_out_of_public_plugin_surface() {
    let Some(root) = runner_root() else {
        // The runner is a sibling repository in the development workspace.
        // This assertion remains useful in CI environments that provide it.
        return;
    };
    let manager = fs::read_to_string(root.join("src/manager/mod.rs"))
        .expect("manager module must be readable");
    assert!(manager.contains("out of the PluginV2"));
    assert!(
        !manager.contains("impl PluginV2") && !manager.contains("method_proto_map"),
        "manager module must not register public PluginV2/MCP methods"
    );
}

#[test]
fn manager_storage_schema_has_no_canonical_issue_shadow_columns() {
    let Some(root) = runner_root() else {
        return;
    };
    let storage = fs::read_to_string(root.join("src/manager/storage.rs"))
        .expect("manager storage module must be readable");
    for table in [
        "manager_entity_revision",
        "manager_idempotency",
        "manager_operation",
        "manager_outbox",
    ] {
        assert!(
            storage.contains(table),
            "manager schema must include {table}"
        );
    }
    for forbidden in ["canonical_issue_snapshot", "issue_title", "canonical_state"] {
        assert!(
            !storage.contains(forbidden),
            "manager schema must not persist canonical Issue data: {forbidden}"
        );
    }
}

#[test]
#[ignore = "requires the sibling issue-tracker-runner integration environment"]
fn phase2_restart_idempotency_and_outbox_skeleton() {
    // TDD placeholder: the runner integration fixture will exercise manager
    // SQLite reopen, same-key replay/conflict, backend marker reconciliation,
    // and the manager-only no-outbox invariant once the Gateway is wired.
    let root = runner_root().expect("ISSUE_TRACKER_RUNNER_DIR must point to runner");
    assert!(root.join("src/manager").is_dir());
}
