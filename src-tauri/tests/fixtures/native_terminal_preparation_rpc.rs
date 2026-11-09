use serde_json::Value;

use crate::unix_tests::package_fixture::{
    FEATURES, NativeFixture, manifest_json, set_manifest, write_host_policy,
    write_private_executable, write_private_file,
};

/// Installs a real stdio-protocol helper in the package fixture. The helper
/// accepts only the two preparation operations and returns protocol receipts
/// that are still checked by the production NativeExecutorClient.
pub fn install_preparation_executor(fixture: &mut NativeFixture) {
    let helper = fixture.resource_root.join("bin/coding-agent-executor");
    let script = r##"#!/usr/bin/env python3
import hashlib
import json
import os
import sys
import re

PROTOCOL = @PROTOCOL_VERSION@
EXECUTOR = "@EXECUTOR_VERSION@"
FEATURES = @FEATURES@

if sys.argv[1:] == ["--capabilities"]:
    print(json.dumps({"protocol_version": PROTOCOL, "executor_version": EXECUTOR, "features": FEATURES}, separators=(",", ":")))
    raise SystemExit(0)

try:
    config_path = sys.argv[2]
    with open(config_path, "rb") as config_file:
        config_bytes = config_file.read()
    config = json.loads(config_bytes)
    request = json.loads(sys.stdin.readline())
    operation, body = next(iter(request["operation"].items()))
    state_root = config["state_root"]
    os.makedirs(state_root, mode=0o700, exist_ok=True)
    with open(os.path.join(state_root, "native-preparation-fixture.log"), "a", encoding="utf-8") as log:
        base_commit = body.get("base_commit")
        commit_detail = " " + base_commit if base_commit else ""
        log.write(operation + " " + body.get("base_branch", "") + commit_detail + "\n")

    if operation == "resolve_base":
        result = {"base_resolved": {
            "run_id": body["run_id"],
            "attempt_id": body["attempt_id"],
            "operation_id": body["operation_id"],
            "repository": body["repository"],
            "base_branch": body["base_branch"] or "main",
            "base_commit": "b" * 40,
            "trusted_config_digest": hashlib.sha256(config_bytes).hexdigest(),
            "protocol_version": PROTOCOL,
            "executor_version": config["expected_executor_version"],
        }}
    elif operation == "prepare":
        workspace = os.path.join(config["workspace_root"], body["run_id"], body["attempt_id"])
        os.makedirs(workspace, mode=0o700, exist_ok=True)
        os.chmod(workspace, 0o700)
        result = {"prepared": {
            "run_id": body["run_id"],
            "attempt_id": body["attempt_id"],
            "repository": body["repository"],
            "base_branch": body["base_branch"],
            "base_commit": body["base_commit"],
            "branch": ("agent/issue-" + (re.sub(r"[^A-Za-z0-9_-]", "-", body["issue_id"][:64]).strip("-_") or "issue") + "-" + body["run_id"]) if body.get("issue_id") is not None else "agent/" + body["run_id"],
            "workspace_path": workspace,
        }}
    else:
        raise ValueError("unsupported operation")

    print(json.dumps({"protocol_version": PROTOCOL, "status": "ok", "result": result}, separators=(",", ":")))
except Exception:
    raise SystemExit(91)
"##
        .replace("@PROTOCOL_VERSION@", &coding_agent_executor::PROTOCOL_VERSION.to_string())
        .replace("@EXECUTOR_VERSION@", coding_agent_executor::EXECUTOR_VERSION)
        .replace("@FEATURES@", &serde_json::to_string(FEATURES).unwrap());
    write_private_executable(&helper, script.as_bytes());

    let manifest: Value = manifest_json(&fixture.resource_root);
    fixture.manifest = manifest.clone();
    set_manifest(fixture, &manifest);
    write_host_policy(
        &fixture.data_paths,
        &manifest,
        &[crate::unix_tests::package_fixture::PRIMARY_REPOSITORY],
        Some("read-token"),
    );
    let policy_path = fixture.policy_path();
    let mut policy: Value = serde_json::from_slice(&std::fs::read(&policy_path).unwrap()).unwrap();
    policy["repositories"][0]["allowed_base_branches"] = serde_json::json!(["main", "develop"]);
    write_private_file(&policy_path, serde_json::to_string_pretty(&policy).unwrap().as_bytes());
}
