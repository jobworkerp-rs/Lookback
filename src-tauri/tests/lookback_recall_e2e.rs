//! Live-sidecar smoke test for the RAG chat feature scaffolding.
//!
//! Verifies that the two apply-time artifacts come up cleanly:
//!
//!   1. `lookback_recall` (PR1) — WORKFLOW worker on the `rag` channel,
//!      backed by `workers/workflows/rag/lookback-recall.yaml`.
//!   2. `lookback-rag` function set (PR2) — defined in
//!      `workers/function-sets.yaml`, targeting `lookback_recall`. The
//!      chat command itself dispatches `memories-llm` directly with
//!      this set as `function_options.function_set_name`, so no
//!      separate chat worker is registered (ARCH-CHAT-1).
//!
//! End-to-end retrieval — embedding → HybridSearch on both layers →
//! source_kind projection — and the LLM tool-call loop are covered in
//! PR3 once the per-token streaming path lands.
//!
//! Marked `#[ignore]` so CI doesn't pull any plugin model. Local
//! invocation mirrors `streaming_dispatch_e2e.rs`:
//!
//! ```sh
//! LOOKBACK_JOBWORKERP_BIN=<all-in-one path> \
//! LOOKBACK_MEMORIES_BIN=<memories front path> \
//! LOOKBACK_PLUGINS_SRC=<repo>/plugins \
//!   cargo test -p lookback-tauri --test lookback_recall_e2e \
//!     -- --ignored --nocapture
//! ```

mod common;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spawns real jobworkerp + memories sidecars; opt in via --ignored"]
async fn rag_chat_scaffolding_registers_cleanly() {
    let (root, sidecars, report) = common::start_chat_e2e_sidecars("recall").await;
    eprintln!("sidecars ready: {:?}", report.endpoints);
    for w in &report.warnings {
        eprintln!("warning: {:?} {} ({:?})", w.kind, w.message, w.detail);
    }
    // Each PR1/PR2 artifact surfaces a registration failure as a
    // `WorkerApplyFailed` warning whose `message` carries the artifact
    // name (the upstream helper wraps gRPC errors behind
    // `registering worker '<name>' failed` /
    // `registering function set '<name>' failed`). The smoke test only
    // needs to prove that NONE of them errored — actual retrieval and
    // tool-call behaviour is covered in PR3's chat-flow e2e.
    for name in ["lookback_recall", "lookback-rag"] {
        assert!(
            !report.warnings.iter().any(|w| w.message.contains(name)),
            "{name} must register cleanly; sidecar warnings: {:?}",
            report.warnings,
        );
    }

    // Worker apply success on its own does not guarantee that the
    // workflow YAML embedded in `workflow_data` is loadable at job
    // runtime — the apply path stores the file verbatim and only the
    // WORKFLOW runner reparses it on dispatch. A bad YAML node (e.g. a
    // jq expression containing a flow-style `{key:value}` left
    // unquoted) survives apply and then explodes as "Failed to load
    // workflow from json=document:..." on the first job. Drive one
    // dispatch_unary so YAML reload happens before we declare the
    // workflow good. Embedding is disabled in this smoke test, so the
    // inner `embedQuery` step is expected to fail — but the error must
    // come from THAT step, not from "Failed to load workflow".
    let handle = lookback_tauri_lib::jobworkerp::JobworkerpHandle::connect(&format!(
        "http://127.0.0.1:{}",
        report.endpoints.jobworkerp_port
    ))
    .await
    .expect("connect");
    // Two dispatches exercise both jq branches of the summary-layer label
    // filter: (1) no `summary_labels` → the default four kinds / LABEL_ANY,
    // (2) `summary_labels` + match → the dynamic LABEL_ALL path. Both must
    // reparse cleanly; the jq only evaluates past the (failing) embedQuery
    // step, but a malformed jq node fails YAML load up front regardless.
    let inputs = [
        format!(
            "{{\"query\":\"cache\",\"memories_grpc_host\":\"127.0.0.1\",\"memories_grpc_port\":{},\"memories_grpc_tls\":false}}",
            report.endpoints.memories_port
        ),
        format!(
            "{{\"query\":\"yesterday\",\"summary_labels\":[\"daily_summary\",\"date:2026-05-29\"],\"summary_label_match\":\"ALL\",\"memories_grpc_host\":\"127.0.0.1\",\"memories_grpc_port\":{},\"memories_grpc_tls\":false}}",
            report.endpoints.memories_port
        ),
    ];
    for input in inputs {
        let dispatch = handle
            .dispatch_unary(
                "lookback_recall",
                serde_json::json!({ "input": input }),
                Some("run"),
            )
            .await;
        if let Err(e) = &dispatch {
            let msg = e.to_string();
            assert!(
                !msg.contains("Failed to load workflow"),
                "workflow YAML must reparse cleanly at job runtime, got: {msg}"
            );
            // Other failures (embedQuery without auto-embedding, gRPC
            // timeouts, etc.) are fine — they don't indicate a bad YAML.
            eprintln!("dispatch_unary expected failure (not YAML load): {msg}");
        } else {
            eprintln!("dispatch_unary unexpectedly succeeded: {:?}", dispatch.ok());
        }
    }

    let _ = sidecars.stop().await;
    let _ = std::fs::remove_dir_all(&root);
}

/// Drive `lookback_recall` with auto-embedding ON so the workflow runs
/// end-to-end (`embedQuery` → HybridSearch → projectHits) and dump the
/// envelope dispatch_unary returns. This exists because the chat tool
/// loop has been getting `sources=0` while `tool_text_len ≈ 7300` —
/// the search ran, but `extract_lookback_sources` failed to find
/// `payload["sources"]`, suggesting the runner wraps projectHits'
/// `{sources:[...]}` inside another envelope layer (e.g. `{result:..}`
/// / `{output:..}` / a string-encoded JSON). Without this test we have
/// no way to inspect the actual key path outside a live Tauri session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spawns real sidecars + boots embedding; opt in via --ignored"]
async fn lookback_recall_dispatch_unary_envelope_shape() {
    // Keep embedding OFF (via the shared setup) so memories starts
    // within the test's wait_for_tcp budget. The envelope shape we
    // care about is determined by the WORKFLOW runner / jobworkerp
    // wrapper, not by what the inner search returns — even a failing
    // `embedQuery` reveals whether the envelope has a `result` /
    // `output` / `sources` top-level key.
    let (root, sidecars, report) = common::start_chat_e2e_sidecars("recall-envelope").await;
    let handle = lookback_tauri_lib::jobworkerp::JobworkerpHandle::connect(&format!(
        "http://127.0.0.1:{}",
        report.endpoints.jobworkerp_port
    ))
    .await
    .expect("connect");

    // dispatch_unary will fail at embedQuery (WorkerNotFound) since
    // auto-embedding is off; the test value is in observing what
    // jobworkerp returns either as a successful envelope (workflow
    // engine collected partial output) or as an error payload. Either
    // way the user-visible run_chat_stream code path is the same — it
    // takes whatever dispatch_unary returns and passes it through
    // `build_tool_response`.
    let result = match handle
        .dispatch_unary(
            "lookback_recall",
            serde_json::json!({
                "input": format!(
                    "{{\"query\":\"cache\",\"memories_grpc_host\":\"127.0.0.1\",\"memories_grpc_port\":{},\"memories_grpc_tls\":false}}",
                    report.endpoints.memories_port
                )
            }),
            Some("run"),
        )
        .await
    {
        Ok(v) => v,
        Err(e) => {
            eprintln!("dispatch_unary returned Err (auto_embedding off, expected): {e:#}");
            // Synthesise a placeholder so we can still print the
            // assertion check below without panicking.
            serde_json::json!({ "_error_for_dump_only": e.to_string() })
        }
    };

    // Dump the full envelope and its top-level keys. Test always passes;
    // the value is in the captured stdout.
    eprintln!("==========");
    eprintln!(
        "top-level type: {}",
        if result.is_object() {
            "object"
        } else if result.is_array() {
            "array"
        } else if result.is_string() {
            "string"
        } else {
            "other"
        }
    );
    if let Some(obj) = result.as_object() {
        let keys: Vec<&str> = obj.keys().map(|s| s.as_str()).collect();
        eprintln!("top-level keys: {keys:?}");
    }
    eprintln!("full envelope: {result}");
    eprintln!("==========");

    let _ = sidecars.stop().await;
    let _ = std::fs::remove_dir_all(&root);
}
