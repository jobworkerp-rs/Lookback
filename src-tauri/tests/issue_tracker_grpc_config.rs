use lookback_tauri_lib::{
    data::DataPaths,
    grpc::issue_tracker::{IssueTrackerServerConfig, render_server_config},
};

mod common;

#[test]
fn renders_private_loopback_server_config_with_manager_settings() {
    let root = common::tempdir("issue-tracker-grpc-config");
    let data = DataPaths::with_root(&root);

    let config = render_server_config(&data, 43123).expect("render server config");

    assert_eq!(config.listen_url(), "http://127.0.0.1:43123");
    assert!(config.toml().contains("listen = \"127.0.0.1:43123\""));
    assert!(
        config
            .toml()
            .contains(&data.issue_tracker_taskchampion_root().display().to_string())
    );
    assert!(config.toml().contains("[tracker.manager_settings]"));
    assert!(config.toml().contains("secret_filter_mode = 1"));
    assert!(config.toml().contains("secret_filter_mode = 2"));
    assert!(
        !config.toml().contains("expected_contract_fingerprint"),
        "the server owns default contract fingerprints"
    );
    assert!(
        !config.toml().contains("expected_schema_fingerprint"),
        "the server owns the current manager schema fingerprint"
    );
    assert!(config.toml().contains("[tracker.lease_policy]"));
    assert!(config.toml().contains("store_identities = []"));
}

#[test]
fn rejects_a_non_loopback_or_zero_port() {
    let root = common::tempdir("issue-tracker-grpc-invalid-config");
    let data = DataPaths::with_root(&root);

    assert!(IssueTrackerServerConfig::new(&data, 0, 43124, 43125).is_err());
}

#[cfg(not(windows))]
#[test]
fn escapes_special_characters_in_paths_for_server_toml() {
    let root = common::tempdir("issue-tracker-grpc-special-path").join(
        r#"C:\Users\x "quoted"
with space"#,
    );
    let data = DataPaths::with_root(&root);

    let config =
        IssueTrackerServerConfig::new(&data, 43126, 43127, 43128).expect("render server config");
    let parsed: toml::Value = toml::from_str(config.toml()).expect("parse generated TOML");
    let expected = data.issue_tracker_taskchampion_root().display().to_string();

    assert_eq!(
        parsed["tracker"]["data_root"].as_str(),
        Some(expected.as_str())
    );
    assert_eq!(
        parsed["tracker"]["manager_settings"]["data_root"].as_str(),
        Some(expected.as_str())
    );
}
