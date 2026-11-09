//! Contract checks for the internal Issue tracker manager Phase 2 boundary.
//!
//! These checks intentionally inspect the local implementation documents and
//! the sibling runner source. They keep the planned internal integration
//! boundary executable without treating the plan as completion evidence.

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

fn issue_tracker_server_root() -> Option<PathBuf> {
    std::env::var_os("ISSUE_TRACKER_SERVER_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            app_root()
                .parent()
                .map(|root| root.join("issue-tracker-server"))
        })
        .filter(|root| root.is_dir())
}

#[test]
fn documents_define_the_standalone_grpc_local_integration_boundary() {
    let Some(plan) = read_app_doc("internal-issue-tracker-implementation-plan_ja.md") else {
        return;
    };
    let Some(design) = read_app_doc("internal-issue-tracker-implementation-design_ja.md") else {
        return;
    };

    for (text, terms) in [
        (
            &plan,
            &[
                "独立 `issue-tracker-server`",
                "IssueTrackerService",
                "loopback",
                "issue-tracker-server",
                "typed gRPC",
                "FunctionSet",
                "Tauri command",
            ][..],
        ),
        (
            &design,
            &[
                "IssueTrackerService",
                "private gRPC",
                "issue-tracker-server",
                "loopback",
                "FunctionSet",
                "Tauri command",
                "チャット",
            ][..],
        ),
    ] {
        for term in terms {
            assert!(
                text.contains(term),
                "Phase 2 document is missing the current local boundary term: {term}"
            );
        }
    }
}

#[test]
fn phase2_plan_has_local_integration_and_external_backup_test_gates() {
    let Some(plan) = read_app_doc("internal-issue-tracker-implementation-plan_ja.md") else {
        return;
    };
    let required_terms = [
        "外部ファイルベースの backup/restore",
        "先に作成するテスト",
        "第1群",
        "第2群",
        "第3群",
        "第4群",
        "issue_tracker_grpc_e2e.rs",
        "release 前の必須外部受入 gate",
    ];

    for term in required_terms {
        assert!(
            plan.contains(term),
            "Phase 2 completion gate is missing required term: {term}"
        );
    }
}

#[test]
fn active_documents_define_the_grpc_boundary_and_withdrawn_manager_data_api() {
    let documents = [
        "internal-issue-tracker-requirements_ja.md",
        "internal-issue-tracker-implementation-design_ja.md",
        "internal-issue-tracker-implementation-plan_ja.md",
        "internal-issue-tracker-requirements-traceability_ja.md",
        "internal-issue-tracker-manager-interface-specification_ja.md",
        "internal-issue-tracker-local-mvp-profile_ja.md",
    ];
    let required_boundary = "IssueTrackerService";

    for name in documents {
        let Some(document) = read_app_doc(name) else {
            // ai-docs is intentionally local-only and absent from clean CI clones.
            return;
        };
        assert!(
            document.contains(required_boundary),
            "{name} must define the private gRPC integration boundary"
        );
        for withdrawn in [
            "Phase 2B",
            "DataManagementCoordinator",
            "manager_create_coordinated_backup",
            "manager_create_coordinated_export",
            "manager_preflight_restore",
            "manager_restore_coordinated_backup",
        ] {
            assert!(
                !document.contains(withdrawn),
                "{name} must not retain the withdrawn manager data API term: {withdrawn}"
            );
        }
    }
}

#[test]
fn documents_separate_tracker_artifacts_from_stopped_external_file_operations() {
    let Some(requirements) = read_app_doc("internal-issue-tracker-requirements_ja.md") else {
        return;
    };
    let Some(manager_spec) =
        read_app_doc("internal-issue-tracker-manager-interface-specification_ja.md")
    else {
        return;
    };
    let Some(plan) = read_app_doc("internal-issue-tracker-implementation-plan_ja.md") else {
        return;
    };

    for (name, document) in [
        ("requirements", &requirements),
        ("manager interface", &manager_spec),
        ("plan", &plan),
    ] {
        for term in [
            "Tracker 単体の artifact API",
            "coordinated manager API",
            "Lookback を停止",
            "外部ファイル",
        ] {
            assert!(
                document.contains(term),
                "{name} must distinguish artifacts from stopped external file operations: {term}"
            );
        }
    }
}

#[test]
fn runner_registers_entity_specific_grpc_services_without_a_public_frontend() {
    let Some(root) = issue_tracker_server_root() else {
        // The runner is a sibling repository in the development workspace.
        // This assertion remains useful in CI environments that provide it.
        return;
    };
    let server = fs::read_to_string(root.join("src/server.rs"))
        .expect("runner server module must be readable");
    assert!(
        server.contains("ManagerInfoServiceServer")
            && server.contains("ManagerConsumerServiceServer"),
        "runner must register entity-specific typed manager gRPC services"
    );
    assert!(
        !server.contains("FunctionSet") && !server.contains("MCP"),
        "the standalone server must not register an MCP or FunctionSet frontend"
    );

    let catalog = fs::read_to_string(root.join("src/proto_schema/catalog.rs"))
        .expect("runner manager method catalog must be readable");
    // The manager API was intentionally consolidated: the evidence service was removed,
    // candidate operations merged into manager_resolve_candidate, and four manager keys added;
    // this count tracks the current sibling contract.
    assert_eq!(
        catalog.matches("\n        \"manager_").count(),
        32,
        "runner manager catalog must publish exactly 32 manager method keys"
    );
    for method in [
        "manager_list_review_candidates",
        "manager_apply_issue_admin_action",
        "manager_resolve_issue_operation",
        "manager_list_ownership_diagnostics",
    ] {
        assert!(
            catalog.contains(method),
            "runner manager catalog must publish new manager method key: {method}"
        );
    }
    for withdrawn in [
        "manager_create_coordinated_backup",
        "manager_get_coordinated_backup",
        "manager_create_coordinated_export",
        "manager_get_coordinated_export",
        "manager_preflight_restore",
        "manager_restore_coordinated_backup",
    ] {
        assert!(
            !catalog.contains(withdrawn),
            "runner manager catalog must not publish withdrawn method: {withdrawn}"
        );
    }
}

#[test]
fn manager_storage_schema_has_no_canonical_issue_shadow_columns() {
    let Some(root) = issue_tracker_server_root() else {
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
fn issue_tracker_protos_are_vendored_under_repo_proto() {
    let proto_root = app_root().join("proto/issue_tracker");
    assert!(
        proto_root.join("rpc/v1/issue_extract.proto").is_file(),
        "issue-tracker protos belong under proto/issue_tracker with jobworkerp and memories"
    );
    assert!(
        proto_root.join("rpc/v1/issue_gateway.proto").is_file(),
        "IssueGateway proto must sit next to the other issue-tracker RPC contracts"
    );
    assert!(
        !app_root().join("src-tauri/proto").exists(),
        "src-tauri/proto is not the vendored protobuf root"
    );
}

#[test]
fn issue_gateway_generated_client_is_compiled() {
    let name = std::any::type_name::<
        lookback_tauri_lib::grpc::proto::issue_tracker::rpc::v1::issue_gateway_client::IssueGatewayClient<
            tonic::transport::Channel,
        >,
    >();
    assert!(
        name.contains("IssueGatewayClient"),
        "issue_gateway.proto must be in build.rs so the generated client exists: {name}"
    );
}

#[test]
#[ignore = "requires a real issue-tracker-server artifact"]
fn standalone_grpc_server_gate_is_opt_in() {
    // The real server gRPC test lives in a dedicated integration target.
    // Keep this guard so normal document checks never silently claim that a
    // source-only assertion is equivalent to the live worker acceptance gate.
    let root = issue_tracker_server_root()
        .expect("ISSUE_TRACKER_SERVER_DIR must point to the standalone server");
    assert!(root.join("src/manager").is_dir());
}
