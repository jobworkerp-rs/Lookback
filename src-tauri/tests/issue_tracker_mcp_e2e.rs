//! Opt-in MCP path for curated Lookback Issue Gateway workers.
//!
//! Starts the staged `issue-tracker-server` and `all-in-one`, registers the
//! Gateway GRPC workers plus `lookback-mcp`, then exercises the MCP
//! claim-lifecycle tool surface.
//!
//! ```sh
//! LOOKBACK_SIDECAR_BIN_DIR=src-tauri/bin \
//!   cargo test -p lookback-tauri --test issue_tracker_mcp_e2e \
//!     -- --ignored --test-threads=1 --nocapture
//! ```

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use lookback_tauri_lib::{
    data::DataPaths, grpc::issue_tracker::render_server_config, jobworkerp::JobworkerpHandle,
};
use tokio::process::Child;

mod common;
use common::{RunningIssueTrackerServer, require_env, reserve_loopback_port, tempdir};

const READY_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, serde::Deserialize)]
struct WorkerYaml {
    workers: Vec<WorkerYamlEntry>,
}

#[derive(Debug, serde::Deserialize)]
struct WorkerYamlEntry {
    name: String,
    description: String,
}

fn gateway_worker_yaml() -> String {
    std::fs::read_to_string(
        lookback_tauri_lib::data::paths::lookback_issue_gateway_workers_yaml()
            .expect("gateway worker yaml"),
    )
    .expect("read gateway worker yaml")
}

fn expected_worker_descriptions(raw_yaml: &str) -> BTreeMap<String, String> {
    serde_yaml::from_str::<WorkerYaml>(raw_yaml)
        .expect("parse gateway worker yaml")
        .workers
        .into_iter()
        .map(|worker| (worker.name, worker.description))
        .collect()
}

fn replace_descriptions_with_plain_text(raw_yaml: &str) -> String {
    let mut value: serde_yaml::Value = serde_yaml::from_str(raw_yaml)
        .expect("parse gateway worker yaml for stale-definition fixture");
    let workers = value
        .as_mapping_mut()
        .and_then(|mapping| mapping.get_mut(serde_yaml::Value::String("workers".into())))
        .and_then(serde_yaml::Value::as_sequence_mut)
        .expect("gateway worker yaml has workers sequence");
    for worker in workers {
        let mapping = worker
            .as_mapping_mut()
            .expect("gateway worker is a mapping");
        let name = mapping
            .get(serde_yaml::Value::String("name".into()))
            .and_then(serde_yaml::Value::as_str)
            .expect("gateway worker has name");
        mapping.insert(
            serde_yaml::Value::String("description".into()),
            serde_yaml::Value::String(format!("obsolete description for {name}")),
        );
    }
    serde_yaml::to_string(&value).expect("serialize stale gateway worker fixture")
}

fn structured_description(raw: &str) -> serde_json::Value {
    serde_yaml::from_str(raw).expect("parse structured worker description")
}

fn tool_by_name<'a>(listed: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    listed
        .pointer("/result/tools")
        .and_then(serde_json::Value::as_array)
        .and_then(|tools| {
            tools
                .iter()
                .find(|tool| tool.get("name").and_then(serde_json::Value::as_str) == Some(name))
        })
        .unwrap_or_else(|| panic!("MCP tool {name} is missing: {listed}"))
}

fn property<'a>(tool: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    tool.pointer(&format!("/inputSchema/properties/{name}"))
        .unwrap_or_else(|| panic!("MCP input property {name} is missing from {tool}"))
}

fn proto_field_name(canonical: &str) -> String {
    let mut proto = String::with_capacity(canonical.len() + 4);
    for character in canonical.chars() {
        if character.is_ascii_uppercase() {
            proto.push('_');
            proto.push(character.to_ascii_lowercase());
        } else {
            proto.push(character);
        }
    }
    proto
}

fn staged_binary(dir: &Path, name: &str) -> PathBuf {
    std::fs::read_dir(dir)
        .expect("read staged sidecar directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|value| value.starts_with(&format!("{name}-")))
        })
        .unwrap_or_else(|| panic!("staged {name} binary is missing from {}", dir.display()))
}

fn wait_for_tcp(addr: &str, timeout: Duration) {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if std::net::TcpStream::connect(addr).is_ok() {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {addr}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn mcp_rpc(
    url: &str,
    body: &serde_json::Value,
    session: Option<&str>,
) -> Option<serde_json::Value> {
    let mut cmd = std::process::Command::new("curl");
    cmd.args([
        "-sS",
        "-i",
        "-X",
        "POST",
        url,
        "-H",
        "Content-Type: application/json",
        "-H",
        "Accept: application/json, text/event-stream",
    ]);
    if let Some(session) = session {
        cmd.args(["-H", &format!("Mcp-Session-Id: {session}")]);
    }
    let output = cmd
        .args(["--data-binary", &body.to_string()])
        .output()
        .expect("curl MCP request");
    let raw = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    eprintln!("MCP HTTP raw response for {}:\n{raw}", body["method"]);
    assert!(
        output.status.success(),
        "curl failed ({:?}): {raw}",
        output.status
    );
    if raw.contains("HTTP/1.1 202") {
        return None;
    }
    Some(parse_mcp_http(&raw))
}

fn parse_mcp_http(raw: &str) -> serde_json::Value {
    let (headers, body) = raw
        .split_once("\r\n\r\n")
        .or_else(|| raw.split_once("\n\n"))
        .unwrap_or_else(|| panic!("MCP response has headers and body: {raw}"));
    let session = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        (name.eq_ignore_ascii_case("mcp-session-id") || name.eq_ignore_ascii_case("Mcp-Session-Id"))
            .then(|| value.trim().to_string())
    });
    let json_text = if let Some(data) = body.lines().find_map(|line| line.strip_prefix("data: ")) {
        data.to_string()
    } else {
        body.trim().to_string()
    };
    let mut value: serde_json::Value = serde_json::from_str(&json_text)
        .unwrap_or_else(|error| panic!("decode MCP body `{json_text}`: {error}"));
    if let Some(session) = session {
        value
            .as_object_mut()
            .expect("MCP JSON object")
            .insert("sessionId".into(), serde_json::Value::String(session));
    }
    value
}

struct RunningAllInOne {
    child: Child,
    grpc_url: String,
    mcp_url: String,
}

impl RunningAllInOne {
    async fn start(root: &Path, mcp_port: u16, grpc_port: u16, binary: &Path) -> Self {
        let data = DataPaths::with_root(root);
        data.ensure().expect("create jobworkerp data root");
        let child = tokio::process::Command::new(binary)
            .current_dir(root)
            .env("STORAGE_TYPE", "Standalone")
            .env("STORAGE_RESTORE_AT_STARTUP", "false")
            .env("GRPC_ADDR", format!("127.0.0.1:{grpc_port}"))
            .env("SQLITE_URL", data.sqlite_url())
            .env("MCP_ENABLED", "true")
            .env("MCP_SET_NAME", "lookback-mcp")
            .env("MCP_ADDR", format!("127.0.0.1:{mcp_port}"))
            .env("MCP_AUTH_ENABLED", "false")
            .env("MCP_EXCLUDE_RUNNER", "true")
            .env_remove("MCP_AUTH_TOKEN_FILE")
            .env_remove("MCP_AUTH_TOKENS")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn all-in-one");
        let server = Self {
            child,
            grpc_url: format!("http://127.0.0.1:{grpc_port}"),
            mcp_url: format!("http://127.0.0.1:{mcp_port}/mcp"),
        };
        server.wait_until_ready().await;
        wait_for_tcp(&format!("127.0.0.1:{mcp_port}"), READY_TIMEOUT);
        server
    }

    async fn wait_until_ready(&self) {
        let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
        loop {
            if JobworkerpHandle::connect(&self.grpc_url).await.is_ok() {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "all-in-one did not become ready"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn stop(mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires staged sidecar binaries; opt in with LOOKBACK_SIDECAR_BIN_DIR"]
async fn mcp_exposes_lookback_issue_claim_lifecycle_through_gateway_workers() {
    let bin_dir = require_env("LOOKBACK_SIDECAR_BIN_DIR");
    let root = tempdir("issue-tracker-mcp");
    let data = DataPaths::with_root(&root);
    let issue_tracker = RunningIssueTrackerServer::start(
        render_server_config(&data, reserve_loopback_port()).expect("render config"),
        &staged_binary(&bin_dir, "issue-tracker-server"),
        Duration::from_secs(30),
        "issue-tracker-mcp-e2e",
    )
    .await;
    unsafe {
        std::env::set_var("ISSUE_TRACKER_GRPC_HOST", "127.0.0.1");
        std::env::set_var("ISSUE_TRACKER_GRPC_PORT", issue_tracker.port().to_string());
        std::env::set_var("ISSUE_TRACKER_GATEWAY_WORKER_HOST", "127.0.0.1");
        std::env::set_var(
            "ISSUE_TRACKER_GATEWAY_WORKER_PORT",
            issue_tracker
                .config
                .gateway_worker_url()
                .rsplit(':')
                .next()
                .unwrap(),
        );
    }

    let all_in_one = RunningAllInOne::start(
        &root,
        reserve_loopback_port(),
        reserve_loopback_port(),
        &staged_binary(&bin_dir, "all-in-one"),
    )
    .await;
    let handle = JobworkerpHandle::connect(&all_in_one.grpc_url)
        .await
        .expect("connect jobworkerp");
    let worker_yaml = gateway_worker_yaml();
    let expected_descriptions = expected_worker_descriptions(&worker_yaml);
    handle
        .register_workers_from_yaml_str(
            &replace_descriptions_with_plain_text(&worker_yaml),
            lookback_tauri_lib::data::paths::workers_bundle_dir()
                .expect("worker bundle")
                .as_path(),
        )
        .await
        .expect("register stale gateway worker definitions");
    let workers = handle
        .register_workers_from_yaml(
            &lookback_tauri_lib::data::paths::lookback_issue_gateway_workers_yaml()
                .expect("gateway worker yaml"),
        )
        .await
        .expect("register gateway workers");
    let expected_workers = [
        "lookback_issue_list_ready",
        "lookback_issue_claim",
        "lookback_issue_heartbeat",
        "lookback_issue_release",
        "lookback_issue_transition",
    ];
    for name in expected_workers {
        assert!(
            workers.contains_key(name),
            "{name} must register; got {:?}",
            workers.keys().collect::<Vec<_>>()
        );
        assert_eq!(
            handle
                .worker_description_by_name(name)
                .await
                .expect("read stored worker description"),
            expected_descriptions.get(name).cloned(),
            "the current worker YAML must replace stale saved description for {name}",
        );
    }
    assert!(
        !workers.contains_key("lookback_issue_create"),
        "the retired creation worker must not be registered"
    );

    let function_set_path = root.join("lookback-mcp.yaml");
    std::fs::write(
        &function_set_path,
        r#"
defaults:
  category: 0
function_sets:
  - name: lookback-mcp
    description: Curated Lookback Issue Gateway tools for MCP e2e.
    targets:
      - type: WORKER
        name: lookback_issue_list_ready
        using: unary
      - type: WORKER
        name: lookback_issue_claim
        using: unary
      - type: WORKER
        name: lookback_issue_heartbeat
        using: unary
      - type: WORKER
        name: lookback_issue_release
        using: unary
      - type: WORKER
        name: lookback_issue_transition
        using: unary
"#,
    )
    .expect("write function set");
    handle
        .register_function_sets_from_yaml(&function_set_path)
        .await
        .expect("register lookback-mcp");

    let initialized = mcp_rpc(
        &all_in_one.mcp_url,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "lookback-mcp-e2e", "version": "0.0.1"}
            }
        }),
        None,
    )
    .expect("initialize returns a JSON body");
    assert!(
        initialized.get("error").is_none(),
        "MCP initialize failed: {initialized}"
    );
    let session = initialized
        .get("sessionId")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned);
    let _ = mcp_rpc(
        &all_in_one.mcp_url,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }),
        session.as_deref(),
    );
    let listed = mcp_rpc(
        &all_in_one.mcp_url,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }),
        session.as_deref(),
    )
    .expect("tools/list returns a JSON body");
    eprintln!("MCP tools/list: {listed}");
    let names = listed
        .pointer("/result/tools")
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|tool| tool.get("name").and_then(|name| name.as_str()))
        .collect::<Vec<_>>();
    for name in expected_workers {
        assert!(
            names.iter().any(|tool| tool.starts_with(name)),
            "{name} must be MCP-visible; tools={names:?} raw={listed}"
        );
    }
    assert!(
        !names
            .iter()
            .any(|tool| tool.starts_with("lookback_issue_create")),
        "the retired creation tool must not be MCP-visible; tools={names:?} raw={listed}"
    );

    for (worker, raw_description) in &expected_descriptions {
        let expected = structured_description(raw_description);
        let tool = tool_by_name(&listed, &format!("{worker}___unary"));
        assert_eq!(
            tool.get("description").and_then(serde_json::Value::as_str),
            expected
                .get("description")
                .and_then(serde_json::Value::as_str),
            "MCP tool description must come from the structured description for {worker}",
        );
        let parameters = expected
            .get("parameters")
            .and_then(serde_json::Value::as_object)
            .expect("structured description has parameters");
        for (field, annotation) in parameters {
            let input = property(tool, field);
            assert_eq!(
                input.get("description").and_then(serde_json::Value::as_str),
                annotation
                    .get("description")
                    .and_then(serde_json::Value::as_str),
                "MCP field description must come from the structured description for {worker}.{field}",
            );
            assert_eq!(
                input.get("examples"),
                annotation.get("examples"),
                "MCP examples must come from the structured description for {worker}.{field}",
            );
            let proto_name = proto_field_name(field);
            if proto_name != *field {
                assert_eq!(
                    property(tool, &proto_name)
                        .get("description")
                        .and_then(serde_json::Value::as_str),
                    annotation
                        .get("description")
                        .and_then(serde_json::Value::as_str),
                    "MCP proto-name alias must retain the annotation for {worker}.{field}",
                );
            }
        }
    }

    let list_call = mcp_rpc(
        &all_in_one.mcp_url,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "lookback_issue_list_ready___unary",
                "arguments": {"pageSize": 0}
            }
        }),
        session.as_deref(),
    )
    .expect("tools/call returns a JSON body");
    assert!(
        list_call.get("error").is_none(),
        "typed ListReady MCP call failed: {list_call}"
    );
    assert!(
        list_call.pointer("/result/isError") != Some(&serde_json::Value::Bool(true)),
        "ListReady must reach the Gateway RPC instead of failing MCP input validation: {list_call}"
    );

    all_in_one.stop().await;
    issue_tracker.stop().await;
    let _ = std::fs::remove_dir_all(&root);
}
