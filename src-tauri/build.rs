use std::env;
use std::path::PathBuf;

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

    let protos: Vec<PathBuf> = [
        "llm_memory/data/common.proto",
        "llm_memory/data/highlight.proto",
        "llm_memory/data/media.proto",
        "llm_memory/data/memory.proto",
        "llm_memory/data/memory_rating.proto",
        "llm_memory/data/memory_vector.proto",
        "llm_memory/data/reflection.proto",
        "llm_memory/data/reflection_filter.proto",
        "llm_memory/data/search_filter.proto",
        "llm_memory/data/thread.proto",
        "llm_memory/data/thread_vector.proto",
        "llm_memory/service/common.proto",
        "llm_memory/service/media.proto",
        "llm_memory/service/memory.proto",
        "llm_memory/service/memory_rating.proto",
        "llm_memory/service/memory_vector.proto",
        "llm_memory/service/reflection.proto",
        "llm_memory/service/reflection_vector.proto",
        "llm_memory/service/thread.proto",
        "llm_memory/service/thread_vector.proto",
        "jobworkerp/runner/llm/chat_args.proto",
        "jobworkerp/runner/llm/chat_result.proto",
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

    // `reflection.proto` imports google/protobuf/field_mask.proto (a protobuf
    // well-known type). protoc resolves these from its bundled include dir;
    // ensure the build host has it (e.g. apt's `libprotobuf-dev` alongside
    // `protobuf-compiler`, or Homebrew's `protobuf`).
    let includes = [proto_root.clone()];

    tonic_prost_build::configure()
        .protoc_arg("--experimental_allow_proto3_optional")
        .build_server(false)
        .build_client(true)
        .compile_protos(&protos, &includes)?;

    println!("cargo:rerun-if-changed={}", proto_root.display());
    for proto in &protos {
        println!("cargo:rerun-if-changed={}", proto.display());
    }

    Ok(())
}
