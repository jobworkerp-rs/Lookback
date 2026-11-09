//! Isolated real-server coverage; the provider is a local HTTP fixture.

use lookback_tauri_lib::{
    coding_agent::semantic_judge::{RegisteredSemanticJudge, SemanticJudgePolicy},
    commands::llm_settings::LlmMode,
    jobworkerp::JobworkerpHandle,
};
use std::{
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

struct ServerProcess(Child);
impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[derive(Clone, Copy)]
enum Failure {
    Unavailable,
    Truncated,
    Unauthorized,
}

struct ProviderState {
    failure: Failure,
    failures_left: usize,
    calls: usize,
}

#[tokio::test]
#[ignore = "requires LOOKBACK_JOBWORKERP_BIN built with the LLM runner changes"]
async fn real_external_judge_retries_only_transient_tool_free_generation() {
    let binary = std::env::var("LOOKBACK_JOBWORKERP_BIN").expect("LOOKBACK_JOBWORKERP_BIN");
    let directory = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider_url = format!("http://{}/v1/", listener.local_addr().unwrap());
    let state = Arc::new(Mutex::new(ProviderState {
        failure: Failure::Unavailable,
        failures_left: 0,
        calls: 0,
    }));
    let provider_state = state.clone();
    let provider = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 8192];
            loop {
                let length = socket.read(&mut buffer).await.unwrap();
                if length == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..length]);
                if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                    let length: usize = header
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:").map(str::trim))
                        .unwrap_or("0")
                        .parse()
                        .unwrap();
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let body_start = request
                .windows(4)
                .position(|bytes| bytes == b"\r\n\r\n")
                .unwrap()
                + 4;
            let body: serde_json::Value = serde_json::from_slice(&request[body_start..]).unwrap();
            assert_ne!(body.get("stream"), Some(&serde_json::Value::Bool(true)));
            assert!(
                body.get("tools")
                    .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty))
            );
            let failure = {
                let mut state = provider_state.lock().unwrap();
                state.calls += 1;
                if state.failures_left > 0 {
                    state.failures_left -= 1;
                    Some(state.failure)
                } else {
                    None
                }
            };
            let (status, body, declared_length) = match failure {
                Some(Failure::Unavailable) => ("503 Service Unavailable", "{}".to_owned(), 2),
                Some(Failure::Unauthorized) => ("401 Unauthorized", "{}".to_owned(), 2),
                Some(Failure::Truncated) => ("200 OK", "{}".to_owned(), 999),
                None => {
                    let body = serde_json::json!({
                        "id":"fixture", "object":"chat.completion", "created":1, "model":"gpt-4o-mini",
                        "choices":[{"index":0,"message":{"role":"assistant","content":"{\"outcome\":\"PASS\"}"},"finish_reason":"stop"}],
                        "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
                    }).to_string();
                    let length = body.len();
                    ("200 OK", body, length)
                }
            };
            socket.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {declared_length}\r\nConnection: close\r\n\r\n{body}").as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        }
    });
    let port_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = port_listener.local_addr().unwrap();
    drop(port_listener);
    let log = std::fs::File::create(directory.path().join("server.log")).unwrap();
    let mut server = ServerProcess(
        Command::new(binary)
            .current_dir(directory.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("STORAGE_TYPE", "Standalone")
            .env(
                "SQLITE_URL",
                format!(
                    "sqlite://{}?mode=rwc",
                    directory.path().join("server.sqlite3").display()
                ),
            )
            .env("SQLITE_MAX_CONNECTIONS", "5")
            .env("GRPC_ADDR", address.to_string())
            .env("WORKER_DEFAULT_CONCURRENCY", "2")
            .env("WORKER_CHANNELS", "llm")
            .env("WORKER_CHANNEL_CONCURRENCIES", "1")
            .env("WORKER_INSTANCE_ENABLED", "false")
            .env("MCP_ENABLED", "false")
            .env("AG_UI_ENABLED", "false")
            .env("OPENAI_API_KEY", "fixture-only-key")
            .env("RUST_LOG", "debug")
            .env("LOG_LEVEL", "debug")
            .env("LOG_USE_JSON", "false")
            .env("LOG_USE_STDOUT", "true")
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap(),
    );
    let endpoint = format!("http://{address}");
    let handle = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Ok(handle) = JobworkerpHandle::connect(&endpoint).await {
                break handle;
            }
            assert!(
                server.0.try_wait().unwrap().is_none(),
                "server exited: {}",
                std::fs::read_to_string(directory.path().join("server.log")).unwrap()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("server startup deadline");
    let yaml = format!(
        r#"defaults:
  response_type: DIRECT
  store_success: false
  store_failure: false
  broadcast_results: false
  retry_policy:
    type: EXPONENTIAL
    interval: 800
    max_interval: 60000
    max_retry: 0
    basis: 2.0
workers:
  - name: memories-llm-external
    runner: LLM
    channel: llm
    use_static: false
    settings:
      genai:
        model: gpt-4o-mini
        base_url: {provider_url}
"#
    );
    handle
        .register_workers_from_yaml_str(&yaml, directory.path())
        .await
        .unwrap();
    for (index, failure, failures, max_retries, expected_calls, successful) in [
        (0, Failure::Unavailable, 1, 1, 2, true),
        (1, Failure::Truncated, 1, 1, 2, true),
        (2, Failure::Unauthorized, 10, 2, 1, false),
        (3, Failure::Unavailable, 10, 1, 2, false),
        (4, Failure::Unavailable, 10, 0, 1, false),
    ] {
        *state.lock().unwrap() = ProviderState {
            failure,
            failures_left: failures,
            calls: 0,
        };
        let judge = RegisteredSemanticJudge::observe(&handle, LlmMode::External)
            .await
            .unwrap()
            .with_policy(SemanticJudgePolicy {
                max_retries,
                deadline_seconds: 15,
            })
            .unwrap();
        let result = judge
            .evaluate(
                &handle,
                b"judge without tools",
                b"source",
                &format!("retry-fixture-{index}"),
            )
            .await;
        if result.is_ok() != successful {
            let db = rusqlite::Connection::open(directory.path().join("server.sqlite3")).unwrap();
            for query in [
                "SELECT id, retried, run_after_time, grabbed_until_time FROM job",
                "SELECT job_id, retried, status FROM job_result",
            ] {
                let mut statement = db.prepare(query).unwrap();
                let count = statement.column_count();
                let rows = statement
                    .query_map([], |row| {
                        (0..count)
                            .map(|index| row.get::<_, rusqlite::types::Value>(index))
                            .collect::<Result<Vec<_>, _>>()
                    })
                    .unwrap()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap();
                eprintln!("{query}: {rows:?}");
            }
        }
        assert_eq!(
            result.is_ok(),
            successful,
            "scenario {index}: {result:?}; calls={}; server={}",
            state.lock().unwrap().calls,
            std::fs::read_dir(directory.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.path().extension().is_some_and(|value| value == "log"))
                .map(|entry| std::fs::read_to_string(entry.path()).unwrap())
                .collect::<Vec<_>>()
                .join("\n")
        );
        assert_eq!(
            state.lock().unwrap().calls,
            expected_calls,
            "scenario {index}"
        );
        if let Ok(result) = result {
            assert_eq!(result, br#"{"outcome":"PASS"}"#);
            let db = rusqlite::Connection::open(directory.path().join("server.sqlite3")).unwrap();
            let (job_ids, retried): (i64, i64) = db.query_row(
                "SELECT COUNT(DISTINCT job_id), MAX(retried) FROM job_result WHERE uniq_key = ?1",
                [format!("retry-fixture-{index}")],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ).unwrap();
            assert_eq!(job_ids, 1, "provider retries must retain the actual job ID");
            assert_eq!(retried, 1);
        }
    }
    provider.abort();
}
