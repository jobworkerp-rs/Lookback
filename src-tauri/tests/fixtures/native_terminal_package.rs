use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use coding_agent_executor::{EXECUTOR_VERSION, PROTOCOL_VERSION, RepositoryId, TrustedConfig};
use lookback_tauri_lib::{
    data::DataPaths,
    native_terminal::bootstrap::{NativePackageSelection, TRUSTED_HOST_POLICY_RELATIVE_PATH},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

pub const FEATURES: &[&str] = &[
    "check_repository",
    "resolve_base",
    "prepare",
    "capture",
    "seal",
    "resolve",
    "artifact_only_publication",
    "push",
    "pull_request",
    "secret_scan_supported_formats_v1",
];

pub const PRIMARY_REPOSITORY: (&str, &str, &str) = ("github.com", "lookback", "agent-app");
pub const SECONDARY_REPOSITORY: (&str, &str, &str) = ("github.com", "lookback", "second-app");
pub const READ_TOKEN: &str = "test-only-read-token-do-not-log";

#[allow(dead_code)]
pub struct NativeFixture {
    pub temp: TempDir,
    pub data_paths: DataPaths,
    pub app_resource_dir: PathBuf,
    pub resource_root: PathBuf,
    pub manifest: Value,
    #[allow(dead_code)] // Shared fixture targets use the token path only in credential tests.
    pub source_token: Option<PathBuf>,
}

#[allow(dead_code)]
impl NativeFixture {
    pub fn new() -> Self {
        let temp = tempfile::tempdir().expect("temporary native-terminal fixture");
        let root = temp.path();
        let app_resource_dir = root.join("resources");
        let resource_root = app_resource_dir.join("coding-agent");
        make_dir(&app_resource_dir);
        make_dir(&resource_root);
        make_dir(&resource_root.join("bin"));
        make_dir(&resource_root.join("runtime"));
        make_dir(&resource_root.join("workers/workflows/coding-agent"));

        let helper = resource_root.join("bin/coding-agent-executor");
        let feature_list = serde_json::to_string(FEATURES).unwrap();
        let script = format!(
            "#!/bin/sh\ncase \"$1\" in\n  --capabilities) printf '%s\\n' '{{\"protocol_version\":{PROTOCOL_VERSION},\"executor_version\":\"{EXECUTOR_VERSION}\",\"features\":{feature_list}}}' ;;\n  *) exit 97 ;;\nesac\n"
        );
        write_private_executable(&helper, script.as_bytes());
        write_file(&resource_root.join("runtime/all-in-one"), b"test runtime");
        for path in configuration_paths() {
            write_file(
                &resource_root.join(path),
                format!("fixture for {path}\n").as_bytes(),
            );
        }

        let manifest = manifest_json(&resource_root);
        write_file(
            &resource_root.join("coding-agent-artifact-manifest.json"),
            serde_json::to_string_pretty(&manifest).unwrap().as_bytes(),
        );

        let data_paths = DataPaths::with_root(root.join("app-data"));
        make_dir(&data_paths.root);
        make_dir(&data_paths.root.join("coding-agent"));
        let policy_root = data_paths.root.join("coding-agent/native");
        make_dir(&policy_root);
        let source_token = policy_root.join("credential-sources/read-token");
        make_dir(source_token.parent().unwrap());
        write_private_file(&source_token, READ_TOKEN.as_bytes());
        write_host_policy(
            &data_paths,
            &manifest,
            &[PRIMARY_REPOSITORY, SECONDARY_REPOSITORY],
            Some("read-token"),
        );

        Self {
            temp,
            data_paths,
            app_resource_dir,
            resource_root,
            manifest,
            source_token: Some(source_token),
        }
    }

    pub fn selection(
        &self,
    ) -> Result<
        NativePackageSelection,
        lookback_tauri_lib::native_terminal::bootstrap::NativeBootstrapUnavailable,
    > {
        NativePackageSelection::from_app_resources(&self.app_resource_dir)
    }

    pub fn repository(values: (&str, &str, &str)) -> RepositoryId {
        RepositoryId {
            host: values.0.into(),
            owner: values.1.into(),
            name: values.2.into(),
        }
    }

    #[allow(dead_code)] // Kept for preparation integration targets that replace the policy set.
    pub fn configure_policy_repositories(&self, repositories: &[(&str, &str, &str)]) {
        write_host_policy(
            &self.data_paths,
            &self.manifest,
            repositories,
            Some("read-token"),
        );
    }

    pub fn policy_path(&self) -> PathBuf {
        self.data_paths.root.join(TRUSTED_HOST_POLICY_RELATIVE_PATH)
    }

    pub fn partition(repository: &RepositoryId) -> String {
        hex(&Sha256::digest(serde_json::to_vec(repository).unwrap()))
    }

    pub fn configured_trusted_config(&self, repository: &RepositoryId) -> TrustedConfig {
        let directory = self
            .data_paths
            .root
            .join("coding-agent/native/configs")
            .join(Self::partition(repository));
        let entries = fs::read_dir(directory)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(entries.len(), 1);
        serde_json::from_slice(&fs::read(entries[0].path()).unwrap()).unwrap()
    }

    #[allow(dead_code)] // Used by executor capability boundary tests in the preparation target.
    pub fn set_capabilities(&self, features: &[&str]) {
        write_capability_helper(
            &self.resource_root.join("bin/coding-agent-executor"),
            features,
        );
    }

    #[allow(dead_code)] // Used only by registry's fresh GUI check test; other targets include this fixture too.
    pub fn install_repository_check_executor(&mut self, marker: &Path) {
        let marker = marker.to_string_lossy().replace('\'', "'\\''");
        let feature_list = serde_json::to_string(FEATURES).unwrap();
        let script = format!(
            r##"#!/bin/sh
if [ "$1" = "--capabilities" ]; then
  printf '%s\n' '{{"protocol_version":{protocol},"executor_version":"{executor}","features":{feature_list}}}'
  exit 0
fi
printf invoked >> '{marker}'
digest=$(sha256sum "$2" | cut -d ' ' -f1)
printf '{{"protocol_version":{protocol},"status":"ok","result":{{"repository_checked":{{"repository":{{"host":"github.com","owner":"lookback","name":"agent-app"}},"trusted_config_digest":"%s","protocol_version":{protocol},"executor_version":"{executor}","stages":[{{"stage":"git_read","status":"passed","code":"git_readable"}},{{"stage":"forge_api","status":"skipped","code":"prerequisite_failed"}}]}}}}}}\n' "$digest"
"##,
            protocol = PROTOCOL_VERSION,
            executor = EXECUTOR_VERSION,
            feature_list = feature_list,
            marker = marker,
        );
        write_private_executable(
            &self.resource_root.join("bin/coding-agent-executor"),
            script.as_bytes(),
        );
        self.manifest = manifest_json(&self.resource_root);
        set_manifest(self, &self.manifest);
    }
}

pub fn manifest_json(resource_root: &Path) -> Value {
    manifest_json_with_features(resource_root, FEATURES)
}

pub fn manifest_json_with_features(resource_root: &Path, features: &[&str]) -> Value {
    let source = |repository: &str, reference: &str, revision: char| {
        json!({
            "repository": repository,
            "ref": reference,
            "revision": revision.to_string().repeat(40),
            "dirty": false,
            "submodules": []
        })
    };
    let mut capabilities = features
        .iter()
        .map(|feature| feature.to_string())
        .collect::<Vec<_>>();
    capabilities.sort();
    json!({
        "schema_version": 1,
        "profile": { "name": "linux-kvm", "version": 1 },
        "target": { "triple": target_triple() },
        "sources": {
            "agent_app": source("https://github.com/lookback/agent-app", "release/native-terminal", 'a'),
            "jobworkerp_runtime": source("https://github.com/jobworkerp-rs/jobworkerp-rs", "release/native-terminal", 'b'),
            "jobworkerp_client": {
                "repository": "https://github.com/jobworkerp-rs/jobworkerp-client-rs",
                "revision": "c".repeat(40),
                "lockfile_sha256": "d".repeat(64)
            }
        },
        "executor": {
            "version": EXECUTOR_VERSION,
            "protocol_version": PROTOCOL_VERSION,
            "capabilities": capabilities
        },
        "runtime_compatibility": { "protocol_version": null, "config_version": null },
        "artifacts": {
            "runtime": artifact(resource_root, "runtime/all-in-one"),
            "executor": artifact(resource_root, "bin/coding-agent-executor"),
            "configuration": configuration_paths().iter().map(|path| artifact(resource_root, path)).collect::<Vec<_>>()
        }
    })
}

pub fn write_host_policy(
    data_paths: &DataPaths,
    manifest: &Value,
    repositories: &[(&str, &str, &str)],
    read_credential_source: Option<&str>,
) {
    let policy = json!({
        "version": 1,
        "max_import_bytes": 16 * 1024 * 1024,
        "expected_package_identity": {
            "profile": manifest["profile"],
            "target": manifest["target"],
            "sources": manifest["sources"],
            "executor": manifest["executor"]
        },
        "repositories": repositories.iter().map(|(host, owner, name)| json!({
            "id": { "host": host, "owner": owner, "name": name },
            "clone_url": format!("https://{host}/{owner}/{name}.git"),
            "allowed_base_branches": ["main", "release/native-terminal"],
            "read_credential_source": read_credential_source,
        })).collect::<Vec<_>>(),
        "credential_sources": read_credential_source.map(|id| vec![json!({
            "id": id,
            "username": "lookback-read-only",
            "token_file": "read-token"
        })]).unwrap_or_default()
    });
    let path = data_paths.root.join(TRUSTED_HOST_POLICY_RELATIVE_PATH);
    write_private_file(
        &path,
        serde_json::to_string_pretty(&policy).unwrap().as_bytes(),
    );
}

#[allow(dead_code)]
pub fn set_manifest(fixture: &NativeFixture, manifest: &Value) {
    write_private_file(
        &fixture
            .resource_root
            .join("coding-agent-artifact-manifest.json"),
        serde_json::to_string_pretty(manifest).unwrap().as_bytes(),
    );
}

pub fn configuration_paths() -> Vec<&'static str> {
    vec![
        "workers/lookback-coding-agent-workers.yaml",
        "workers/workflows/coding-agent/prepare.yaml",
        "workers/workflows/coding-agent/one-shot.yaml",
        "workers/workflows/coding-agent/publish.yaml",
    ]
}

#[allow(dead_code)] // Shared by capability-focused preparation tests only.
pub fn write_capability_helper(path: &Path, features: &[&str]) {
    let feature_list = serde_json::to_string(features).unwrap();
    let script = format!(
        "#!/bin/sh\ncase \"$1\" in\n  --capabilities) printf '%s\\n' '{{\"protocol_version\":{PROTOCOL_VERSION},\"executor_version\":\"{EXECUTOR_VERSION}\",\"features\":{feature_list}}}' ;;\n  *) exit 97 ;;\nesac\n"
    );
    write_private_executable(path, script.as_bytes());
}

pub fn hash_file(path: &Path) -> String {
    hex(&Sha256::digest(fs::read(path).unwrap()))
}

pub fn write_private_file(path: &Path, contents: &[u8]) {
    make_dir(path.parent().unwrap());
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true).mode(0o600);
    let mut file = options.open(path).unwrap();
    file.write_all(contents).unwrap();
    file.sync_all().unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

pub fn write_private_executable(path: &Path, contents: &[u8]) {
    write_private_file(path, contents);
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

pub fn make_dir(path: &Path) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn write_file(path: &Path, contents: &[u8]) {
    make_dir(path.parent().unwrap());
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn artifact(root: &Path, path: &str) -> Value {
    json!({ "path": path, "sha256": hash_file(&root.join(path)) })
}

fn target_triple() -> &'static str {
    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
    {
        "x86_64-unknown-linux-gnu"
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64", target_env = "gnu"))]
    {
        "aarch64-unknown-linux-gnu"
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "musl"))]
    {
        "x86_64-unknown-linux-musl"
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64", target_env = "musl"))]
    {
        "aarch64-unknown-linux-musl"
    }
    #[cfg(not(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64"),
        any(target_env = "gnu", target_env = "musl")
    )))]
    {
        "unsupported-test-target"
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[allow(dead_code)]
pub fn sidecars(root: &Path) -> std::sync::Arc<lookback_tauri_lib::sidecar::Sidecars> {
    let data = DataPaths::with_root(root.join("sidecar-data"));
    std::sync::Arc::new(lookback_tauri_lib::sidecar::Sidecars::new(
        lookback_tauri_lib::sidecar::SidecarConfig {
            jobworkerp_bin: "/bin/true".into(),
            memories_bin: "/bin/true".into(),
            memories_db_migrate_bin: "/bin/true".into(),
            startup_database_migration_enabled: false,
            conductor_bin: "/bin/true".into(),
            data: data.clone(),
            worker_yaml_paths: Vec::new(),
            issue_tracker_server_bin: None,
            function_set_yaml_paths: Vec::new(),
            reflection_dispatch_enabled: false,
            auto_embedding_enabled: false,
            workflows_dir: None,
            lance_language_model_home: data.lance_language_model_home(),
            lindera_dict_staged: false,
            llm_model: None,
            llm_hf_repo: None,
            llm_ctx_size: None,
            llm_kv_cache_type: None,
            env_file: None,
        },
    ))
}
