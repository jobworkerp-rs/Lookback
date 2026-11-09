use std::env;
use std::path::PathBuf;

fn compile_client_protos(
    protos: &[PathBuf],
    includes: &[PathBuf],
) -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .protoc_arg("--experimental_allow_proto3_optional")
        .build_server(false)
        .build_client(true)
        .compile_protos(protos, includes)?;
    Ok(())
}

fn compile_server_proto(
    proto: &PathBuf,
    includes: &[PathBuf],
    output_dir: &PathBuf,
    descriptor_set: &PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .protoc_arg("--experimental_allow_proto3_optional")
        .build_server(true)
        .build_client(true)
        .out_dir(output_dir)
        .file_descriptor_set_path(descriptor_set)
        .compile_protos(std::slice::from_ref(proto), includes)?;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Tauri build script (icons, capabilities, etc.)
    tauri_build::build();

    println!("cargo:rerun-if-env-changed=GPU");
    let target_os = env::var("CARGO_CFG_TARGET_OS")?;
    println!(
        "cargo:rustc-env=LOOKBACK_TARGET_TRIPLE={}",
        env::var("TARGET")?
    );
    let embedding_gpu = env::var("GPU").unwrap_or_else(|_| {
        if target_os == "macos" {
            "metal".to_string()
        } else {
            "cuda".to_string()
        }
    });
    println!("cargo:rustc-env=LOOKBACK_EMBEDDING_GPU={embedding_gpu}");

    // Compile the memories protos, the runner-level LLM chat pair, and the
    // conductor admin protos used by the local periodic-task sidecar.
    let proto_root = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?).join("../proto");
    // Include root is proto/issue_tracker because upstream imports
    // `v1/...`, `manager/v1/...`, and `rpc/v1/...` without an
    // `issue_tracker/` prefix.
    let issue_tracker_proto_root = proto_root.join("issue_tracker");

    let sidecar_protos: Vec<PathBuf> = [
        "llm_memory/data/common.proto",
        "llm_memory/data/embedding_space.proto",
        "llm_memory/data/highlight.proto",
        "llm_memory/data/media.proto",
        "llm_memory/data/memory.proto",
        "llm_memory/data/memory_rating.proto",
        "llm_memory/data/memory_vector.proto",
        "llm_memory/data/reflection.proto",
        "llm_memory/data/reflection_filter.proto",
        "llm_memory/data/search_filter.proto",
        "llm_memory/data/thread.proto",
        "llm_memory/data/thread_group.proto",
        "llm_memory/data/thread_vector.proto",
        "llm_memory/service/common.proto",
        "llm_memory/service/embedding_space.proto",
        "llm_memory/service/media.proto",
        "llm_memory/service/memory.proto",
        "llm_memory/service/memory_rating.proto",
        "llm_memory/service/memory_vector.proto",
        "llm_memory/service/reflection.proto",
        "llm_memory/service/reflection_vector.proto",
        "llm_memory/service/search_index_maintenance.proto",
        "llm_memory/service/thread.proto",
        "llm_memory/service/thread_group.proto",
        "llm_memory/service/thread_vector.proto",
        "jobworkerp/runner/llm/chat_args.proto",
        "jobworkerp/runner/llm/chat_result.proto",
        "jobworkerp/runner/sandbox_result.proto",
        "jobworkerp_conductor/data/common.proto",
        "jobworkerp_conductor/data/jobworkerp_server.proto",
        "jobworkerp_conductor/data/cron_scheduler.proto",
        "jobworkerp_conductor/data/execution_ref.proto",
        "jobworkerp_conductor/service/common.proto",
        "jobworkerp_conductor/service/jobworkerp_server.proto",
        "jobworkerp_conductor/service/cron_scheduler.proto",
        "jobworkerp_conductor/service/execution_status.proto",
    ]
    .iter()
    .map(|p| proto_root.join(p))
    .collect();

    let issue_tracker_protos: Vec<PathBuf> = [
        "v1/issue_tracker_common.proto",
        "v1/issue_tracker_methods.proto",
        "v1/issue_tracker_settings.proto",
        "manager/v1/issue_tracker_manager_common.proto",
        "manager/v1/issue_tracker_manager_methods.proto",
        "manager/v1/issue_tracker_manager_settings.proto",
        "rpc/v1/activity_service.proto",
        "rpc/v1/artifact_service.proto",
        "rpc/v1/candidate_service.proto",
        "rpc/v1/canonical_issue_service.proto",
        "rpc/v1/claim_service.proto",
        "rpc/v1/extraction_run_service.proto",
        "rpc/v1/issue_extract.proto",
        "rpc/v1/gateway_control_service.proto",
        "rpc/v1/issue_gateway.proto",
        "rpc/v1/issue_service.proto",
        "rpc/v1/issue_tracker_service.proto",
        "rpc/v1/manager_info_service.proto",
        "rpc/v1/manager_consumer_service.proto",
        "rpc/v1/manager_trusted_read_service.proto",
        "rpc/v1/manager_operation_service.proto",
        "rpc/v1/operation_service.proto",
        "rpc/v1/outbox_maintenance_service.proto",
        "rpc/v1/relation_service.proto",
        "rpc/v1/tracker_info_service.proto",
    ]
    .iter()
    .map(|p| issue_tracker_proto_root.join(p))
    .collect();

    // Compile separately so `proto/` cannot resolve issue-tracker files as
    // `issue_tracker/v1/...` while the same files are also `v1/...`.
    compile_client_protos(&sidecar_protos, std::slice::from_ref(&proto_root))?;
    compile_client_protos(
        &issue_tracker_protos,
        std::slice::from_ref(&issue_tracker_proto_root),
    )?;

    // This worker-facing facade is generated separately so its server code
    // and reflection descriptor do not overwrite the trusted-consumer client
    // package output above.
    let chat_issue_proto =
        issue_tracker_proto_root.join("rpc/v1/lookback_chat_issue_service.proto");
    let out_dir = PathBuf::from(env::var("OUT_DIR")?);
    let chat_proto_out_dir = out_dir.join("lookback_chat_generated");
    std::fs::create_dir_all(&chat_proto_out_dir)?;
    let descriptor_set = out_dir.join("lookback_chat_issue_descriptor.bin");
    compile_server_proto(
        &chat_issue_proto,
        std::slice::from_ref(&issue_tracker_proto_root),
        &chat_proto_out_dir,
        &descriptor_set,
    )?;

    println!("cargo:rerun-if-changed={}", proto_root.display());
    println!(
        "cargo:rerun-if-changed={}",
        issue_tracker_proto_root.display()
    );
    for proto in sidecar_protos.iter().chain(&issue_tracker_protos) {
        println!("cargo:rerun-if-changed={}", proto.display());
    }
    println!("cargo:rerun-if-changed={}", chat_issue_proto.display());

    Ok(())
}
