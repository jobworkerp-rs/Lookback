use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_yaml::{Mapping, Value};

use crate::error::{AppError, AppResult};

/// The Issue agent tools are the last entries of [`ISSUE_WORKER_NAMES`].
const ISSUE_AGENT_TOOL_COUNT: usize = 2;

/// Agent tools exposed in both the chat and the MCP FunctionSet.
pub(super) fn issue_agent_tool_names() -> &'static [&'static str] {
    &ISSUE_WORKER_NAMES[ISSUE_WORKER_NAMES.len() - ISSUE_AGENT_TOOL_COUNT..]
}

pub(super) const ISSUE_WORKER_NAMES: [&str; 25] = [
    "lookback_issue_list",
    "lookback_issue_get",
    "lookback_issue_create",
    "lookback_issue_update",
    "lookback_issue_cancel",
    "lookback_issue_list_ready",
    "lookback_issue_claim",
    "lookback_issue_heartbeat",
    "lookback_issue_release",
    "lookback_issue_transition",
    "lookback-issue-extract",
    "lookback-thread-group-issue-resolution",
    "lookback-thread-issue-resolution",
    "lookback-thread-issue-resolution-manual",
    "lookback-issue-resolution-review-candidates",
    "lookback-issue-resolution-prepare",
    "lookback-issue-resolution-origin",
    "lookback-issue-resolution-judge",
    "lookback-issue-resolution-apply",
    "lookback-cross-thread-issue-resolution",
    "lookback-cross-thread-issue-resolution-manual",
    "lookback-cross-thread-issue-resolution-candidate",
    "lookback-cross-thread-issue-resolution-watermark",
    "lookback_issue_agent_plan",
    "lookback_issue_agent_get",
];

const MEMORY_WORKER_NAMES: [&str; 3] = [
    "lookback_recall",
    "lookback_get_context",
    "lookback_get_reflections",
];
pub(super) const REQUIRED_FUNCTION_SET_NAMES: [&str; 3] =
    ["lookback-chat", "lookback-mcp", "lookback-rag"];
const STAGED_MANIFEST_NAME: &str = "function-sets.issue-staged.yaml";

pub(super) fn stage_function_set_manifest(
    source: &Path,
    data_root: &Path,
    enabled: bool,
) -> AppResult<PathBuf> {
    let destination = data_root.join(STAGED_MANIFEST_NAME);
    let result = stage_function_set_manifest_inner(source, data_root, &destination, enabled);
    if result.is_err() {
        // A failed staging attempt must not leave a previously successful manifest
        // available for a caller that accidentally reuses the deterministic path.
        let _ = fs::remove_file(&destination);
    }
    result.map(|()| destination)
}

fn stage_function_set_manifest_inner(
    source: &Path,
    data_root: &Path,
    destination: &Path,
    enabled: bool,
) -> AppResult<()> {
    let source_bytes = fs::read(source)?;
    let mut manifest: Value = serde_yaml::from_slice(&source_bytes).map_err(|error| {
        AppError::WorkerRegistration(format!(
            "parse function-set manifest {}: {error}",
            source.display()
        ))
    })?;
    validate_function_set_manifest(&mut manifest, enabled)?;

    let serialized = serde_yaml::to_string(&manifest).map_err(|error| {
        AppError::WorkerRegistration(format!("serialize function-set manifest: {error}"))
    })?;
    fs::create_dir_all(data_root)?;

    // Keep the temporary file beside the destination so rename stays on one filesystem.
    let mut staged_file = tempfile::NamedTempFile::new_in(data_root)?;
    staged_file.write_all(serialized.as_bytes())?;
    staged_file.as_file().sync_all()?;
    staged_file.persist(destination).map_err(|error| {
        AppError::WorkerRegistration(format!(
            "install staged function-set manifest {}: {}",
            destination.display(),
            error.error
        ))
    })?;
    Ok(())
}

fn validate_function_set_manifest(manifest: &mut Value, enabled: bool) -> AppResult<()> {
    let function_sets = manifest
        .as_mapping_mut()
        .and_then(|root| mapping_value_mut(root, "function_sets"))
        .and_then(|value| value.as_sequence_mut())
        .ok_or_else(|| registration_error("function_sets must be a sequence"))?;

    let mut names = HashSet::new();
    let mut required_sets = HashSet::new();
    for function_set in function_sets.iter_mut() {
        let mapping = function_set
            .as_mapping_mut()
            .ok_or_else(|| registration_error("each FunctionSet must be a mapping"))?;
        let name = mapping_value(mapping, "name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| registration_error("each FunctionSet needs a non-empty name"))?;
        if !names.insert(name.to_owned()) {
            return Err(registration_error(format!(
                "duplicate FunctionSet name: {name}"
            )));
        }
        if REQUIRED_FUNCTION_SET_NAMES.contains(&name) {
            required_sets.insert(name.to_owned());
        }

        let targets = mapping_value(mapping, "targets");
        let required = REQUIRED_FUNCTION_SET_NAMES.contains(&name);
        let targets = match targets {
            Some(Value::Sequence(targets)) => targets,
            Some(_) => {
                return Err(registration_error(format!(
                    "targets for FunctionSet {name} must be a sequence"
                )));
            }
            None if required => {
                return Err(registration_error(format!(
                    "required FunctionSet {name} has no targets"
                )));
            }
            None => continue,
        };
        if required && targets.is_empty() {
            return Err(registration_error(format!(
                "required FunctionSet {name} has an empty target list"
            )));
        }

        let mut target_names = HashSet::new();
        for target in targets {
            let target = target
                .as_mapping()
                .ok_or_else(|| registration_error(format!("target in {name} must be a mapping")))?;
            let target_type = mapping_value(target, "type")
                .and_then(Value::as_str)
                .filter(|target_type| !target_type.trim().is_empty());
            if target_type != Some("WORKER") {
                return Err(registration_error(format!(
                    "target in {name} must have type WORKER"
                )));
            }
            let target_name = mapping_value(target, "name")
                .and_then(Value::as_str)
                .filter(|target_name| !target_name.trim().is_empty())
                .ok_or_else(|| {
                    registration_error(format!("target in {name} needs a non-empty name"))
                })?;
            if !target_names.insert(target_name.to_owned()) {
                return Err(registration_error(format!(
                    "duplicate target name {target_name} in FunctionSet {name}"
                )));
            }
            if target_name.starts_with("lookback_issue_")
                && !ISSUE_WORKER_NAMES.contains(&target_name)
            {
                return Err(registration_error(format!(
                    "unowned Issue target {target_name} in FunctionSet {name}"
                )));
            }
            if ISSUE_WORKER_NAMES.contains(&target_name)
                && !matches!(name, "lookback-chat" | "lookback-mcp")
            {
                return Err(registration_error(format!(
                    "Issue target {target_name} is unexpected in FunctionSet {name}"
                )));
            }
        }
        if enabled && required {
            let expected: HashSet<&str> = MEMORY_WORKER_NAMES
                .iter()
                .copied()
                .chain(match name {
                    "lookback-chat" => ISSUE_WORKER_NAMES[..5].iter().copied(),
                    "lookback-mcp" => ISSUE_WORKER_NAMES[5..10].iter().copied(),
                    _ => ISSUE_WORKER_NAMES[..0].iter().copied(),
                })
                .chain(if matches!(name, "lookback-chat" | "lookback-mcp") {
                    issue_agent_tool_names().iter().copied()
                } else {
                    ISSUE_WORKER_NAMES[..0].iter().copied()
                })
                .collect();
            if target_names
                .iter()
                .map(String::as_str)
                .collect::<HashSet<_>>()
                != expected
            {
                return Err(registration_error(format!(
                    "enabled FunctionSet {name} has unexpected or missing targets"
                )));
            }
        }
    }

    for required in REQUIRED_FUNCTION_SET_NAMES {
        if !required_sets.contains(required) {
            return Err(registration_error(format!(
                "required FunctionSet {required} is missing"
            )));
        }
    }

    if !enabled {
        for function_set in function_sets.iter_mut() {
            let mapping = function_set
                .as_mapping_mut()
                .expect("all FunctionSets were validated as mappings");
            let name = mapping_value(mapping, "name")
                .and_then(Value::as_str)
                .expect("all FunctionSets were validated with names")
                .to_owned();
            if !matches!(name.as_str(), "lookback-chat" | "lookback-mcp") {
                continue;
            }
            let targets = mapping_value_mut(mapping, "targets")
                .and_then(Value::as_sequence_mut)
                .expect("required target sequences were validated");
            targets.retain(|target| {
                target
                    .as_mapping()
                    .and_then(|target| mapping_value(target, "name"))
                    .and_then(Value::as_str)
                    .is_some_and(|name| !ISSUE_WORKER_NAMES.contains(&name))
            });

            let remaining: HashSet<&str> = targets
                .iter()
                .filter_map(|target| {
                    target
                        .as_mapping()
                        .and_then(|target| mapping_value(target, "name"))
                        .and_then(Value::as_str)
                })
                .collect();
            let expected: HashSet<_> = MEMORY_WORKER_NAMES.into_iter().collect();
            if remaining != expected {
                return Err(registration_error(format!(
                    "disabled FunctionSet {name} must retain exactly the three memory targets"
                )));
            }
        }
    }
    Ok(())
}

fn mapping_value<'a>(mapping: &'a Mapping, key: &str) -> Option<&'a Value> {
    mapping.get(Value::String(key.to_owned()))
}

fn mapping_value_mut<'a>(mapping: &'a mut Mapping, key: &str) -> Option<&'a mut Value> {
    mapping.get_mut(Value::String(key.to_owned()))
}

fn registration_error(message: impl Into<String>) -> AppError {
    AppError::WorkerRegistration(message.into())
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::fs;
    use std::path::{Path, PathBuf};

    use serde_yaml::Value;

    use super::*;

    const CANONICAL_FUNCTION_SETS: &str = include_str!("../../../../workers/function-sets.yaml");
    const CHAT_WORKERS: &str = include_str!("../../../../workers/lookback-issue-chat-workers.yaml");
    const GATEWAY_WORKERS: &str =
        include_str!("../../../../workers/lookback-issue-gateway-workers.yaml");
    const EXTRACT_WORKERS: &str =
        include_str!("../../../../workers/lookback-issue-extract-workers.yaml");

    fn test_paths() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().expect("create test directory");
        let source = temp.path().join("source.yaml");
        let data_root = temp.path().join("data");
        fs::write(&source, CANONICAL_FUNCTION_SETS).expect("write source manifest");
        (temp, source, data_root)
    }

    fn stage_text(text: &str, enabled: bool) -> Result<(tempfile::TempDir, PathBuf), AppError> {
        let temp = tempfile::tempdir().expect("create test directory");
        let source = temp.path().join("source.yaml");
        let data_root = temp.path().join("data");
        fs::write(&source, text).expect("write source manifest");
        let staged = stage_function_set_manifest(&source, &data_root, enabled)?;
        Ok((temp, staged))
    }

    fn function_set<'a>(manifest: &'a Value, name: &str) -> &'a Value {
        manifest["function_sets"]
            .as_sequence()
            .expect("function_sets sequence")
            .iter()
            .find(|set| set["name"].as_str() == Some(name))
            .unwrap_or_else(|| panic!("missing FunctionSet {name}"))
    }

    fn function_set_mut<'a>(manifest: &'a mut Value, name: &str) -> &'a mut Value {
        manifest["function_sets"]
            .as_sequence_mut()
            .expect("function_sets sequence")
            .iter_mut()
            .find(|set| set["name"].as_str() == Some(name))
            .unwrap_or_else(|| panic!("missing FunctionSet {name}"))
    }

    fn target_names(set: &Value) -> Vec<&str> {
        set["targets"]
            .as_sequence()
            .expect("targets sequence")
            .iter()
            .map(|target| target["name"].as_str().expect("target name"))
            .collect()
    }

    fn parse(path: &Path) -> Value {
        serde_yaml::from_slice(&fs::read(path).expect("read staged manifest"))
            .expect("parse staged manifest")
    }

    #[test]
    fn enabled_stage_retains_the_canonical_issue_tools() {
        let (_temp, source, data_root) = test_paths();
        let staged = stage_function_set_manifest(&source, &data_root, true).expect("stage enabled");
        let output = parse(&staged);
        let original: Value = serde_yaml::from_str(CANONICAL_FUNCTION_SETS).unwrap();

        for set_name in ["lookback-chat", "lookback-mcp", "lookback-rag"] {
            assert_eq!(
                function_set(&output, set_name),
                function_set(&original, set_name)
            );
        }
        assert_eq!(
            target_names(function_set(&output, "lookback-chat")).len(),
            10
        );
        assert_eq!(
            target_names(function_set(&output, "lookback-mcp")).len(),
            10
        );
    }

    #[test]
    fn delegation_tools_are_available_in_chat_and_mcp_but_never_execute_or_approve() {
        let (_temp, source, data_root) = test_paths();
        let staged = stage_function_set_manifest(&source, &data_root, true).unwrap();
        let output = parse(&staged);
        for set in ["lookback-chat", "lookback-mcp"] {
            let names = target_names(function_set(&output, set));
            for tool in ["lookback_issue_agent_plan", "lookback_issue_agent_get"] {
                assert!(names.contains(&tool), "{set}: {tool}");
            }
            assert!(!names.contains(&"lookback_issue_agent_start"));
            assert!(!names.contains(&"lookback_issue_agent_approve"));
        }
        let staged = stage_function_set_manifest(&source, &data_root, false).unwrap();
        let output = parse(&staged);
        for set in ["lookback-chat", "lookback-mcp"] {
            assert!(
                target_names(function_set(&output, set))
                    .iter()
                    .all(|name| !name.starts_with("lookback_issue_"))
            );
        }
    }

    #[test]
    fn enabled_stage_rejects_missing_issue_target() {
        let missing_chat_target = CANONICAL_FUNCTION_SETS.replace(
            "      - type: WORKER\n        name: lookback_issue_cancel\n        using: unary\n",
            "",
        );
        assert!(stage_text(&missing_chat_target, true).is_err());
        let missing_mcp_target = CANONICAL_FUNCTION_SETS.replace(
            "      - type: WORKER\n        name: lookback_issue_release\n        using: unary\n",
            "",
        );
        assert!(stage_text(&missing_mcp_target, true).is_err());
    }

    #[test]
    fn disabled_stage_removes_issue_targets_and_keeps_three_memory_targets() {
        let (_temp, source, data_root) = test_paths();
        let initially_enabled =
            stage_function_set_manifest(&source, &data_root, true).expect("stage enabled");
        let staged =
            stage_function_set_manifest(&source, &data_root, false).expect("stage disabled");
        assert_eq!(staged, initially_enabled, "staging path is deterministic");
        let output = parse(&staged);

        for set_name in ["lookback-chat", "lookback-mcp"] {
            let names = target_names(function_set(&output, set_name));
            assert_eq!(
                names,
                [
                    "lookback_recall",
                    "lookback_get_context",
                    "lookback_get_reflections"
                ]
            );
        }
        for set_name in ["lookback-chat", "lookback-mcp", "lookback-rag"] {
            assert!(
                target_names(function_set(&output, set_name))
                    .iter()
                    .all(|name| !ISSUE_WORKER_NAMES.contains(name))
            );
        }
    }

    #[test]
    fn malformed_yaml_and_missing_required_sets_are_rejected() {
        assert!(matches!(
            stage_text("function_sets: [", false),
            Err(AppError::WorkerRegistration(_))
        ));
        assert!(matches!(
            stage_text("function_sets: []", false),
            Err(AppError::WorkerRegistration(_))
        ));
    }

    #[test]
    fn empty_or_malformed_targets_are_rejected() {
        let mut empty_targets: Value = serde_yaml::from_str(CANONICAL_FUNCTION_SETS).unwrap();
        function_set_mut(&mut empty_targets, "lookback-rag")["targets"] =
            Value::Sequence(Vec::new());
        let empty_targets = serde_yaml::to_string(&empty_targets).unwrap();
        assert!(matches!(
            stage_text(&empty_targets, false),
            Err(AppError::WorkerRegistration(_))
        ));

        let mut absent_targets: Value = serde_yaml::from_str(CANONICAL_FUNCTION_SETS).unwrap();
        function_set_mut(&mut absent_targets, "lookback-chat")
            .as_mapping_mut()
            .unwrap()
            .remove(Value::String("targets".into()));
        let absent_targets = serde_yaml::to_string(&absent_targets).unwrap();
        assert!(stage_text(&absent_targets, true).is_err());

        let mut nameless_target: Value = serde_yaml::from_str(CANONICAL_FUNCTION_SETS).unwrap();
        let target = function_set_mut(&mut nameless_target, "lookback-chat")["targets"]
            .as_sequence_mut()
            .unwrap()
            .iter_mut()
            .find(|target| target["name"].as_str() == Some("lookback_issue_list"))
            .unwrap();
        target["name"] = Value::Null;
        let nameless_target = serde_yaml::to_string(&nameless_target).unwrap();
        assert!(stage_text(&nameless_target, true).is_err());

        let missing_target_type = CANONICAL_FUNCTION_SETS.replace(
            "      - type: WORKER\n        name: lookback_issue_list\n",
            "      - name: lookback_issue_list\n",
        );
        assert!(stage_text(&missing_target_type, true).is_err());
    }

    #[test]
    fn duplicate_sets_and_target_names_are_rejected() {
        let duplicate_set =
            format!("{CANONICAL_FUNCTION_SETS}\n  - name: lookback-chat\n    targets: []\n");
        assert!(stage_text(&duplicate_set, true).is_err());

        let duplicate_target = CANONICAL_FUNCTION_SETS.replace(
            "      - type: WORKER\n        name: lookback_issue_list\n        using: unary\n",
            "      - type: WORKER\n        name: lookback_issue_list\n        using: unary\n      - type: WORKER\n        name: lookback_issue_list\n        using: unary\n",
        );
        assert!(stage_text(&duplicate_target, false).is_err());
    }

    #[test]
    fn issue_targets_outside_chat_and_mcp_are_rejected() {
        let issue_in_rag = CANONICAL_FUNCTION_SETS.replace(
            "      - type: WORKER\n        name: lookback_get_reflections\n        using: run\n",
            "      - type: WORKER\n        name: lookback_get_reflections\n        using: run\n      - type: WORKER\n        name: lookback_issue_list\n        using: unary\n",
        );
        assert!(stage_text(&issue_in_rag, false).is_err());
        let unknown_issue_target = CANONICAL_FUNCTION_SETS.replace(
            "        name: lookback_issue_list\n",
            "        name: lookback_issue_unowned\n",
        );
        assert!(stage_text(&unknown_issue_target, false).is_err());
        let unknown_in_other_set = format!(
            "{CANONICAL_FUNCTION_SETS}\n  - name: extra-tools\n    targets:\n      - type: WORKER\n        name: lookback_issue_unowned\n        using: unary\n"
        );
        assert!(stage_text(&unknown_in_other_set, false).is_err());
    }

    #[test]
    fn issue_agent_tools_are_the_agent_workers_exposed_in_both_function_sets() {
        assert_eq!(
            issue_agent_tool_names(),
            ["lookback_issue_agent_plan", "lookback_issue_agent_get"]
        );
    }

    #[test]
    fn issue_worker_names_match_the_three_owned_worker_manifests() {
        fn names(yaml: &str) -> HashSet<String> {
            // Registration expands `%{VAR:-default}` before parsing.
            let yaml = crate::data::paths::tests::expand_env_defaults(yaml);
            let value: Value = serde_yaml::from_str(&yaml).expect("parse worker manifest");
            value["workers"]
                .as_sequence()
                .expect("workers sequence")
                .iter()
                .map(|worker| worker["name"].as_str().expect("worker name").to_owned())
                .collect()
        }

        let yaml_names: HashSet<_> = [CHAT_WORKERS, GATEWAY_WORKERS, EXTRACT_WORKERS]
            .into_iter()
            .flat_map(names)
            .collect();
        let constant_names: HashSet<_> = ISSUE_WORKER_NAMES
            .iter()
            .map(|name| (*name).to_owned())
            .collect();
        assert_eq!(yaml_names, constant_names);
    }

    #[test]
    fn failed_off_stage_does_not_fall_back_to_an_existing_staged_manifest() {
        let (_temp, source, data_root) = test_paths();
        let staged = stage_function_set_manifest(&source, &data_root, true).expect("initial stage");
        fs::write(&staged, "stale manifest").expect("write stale stage");
        fs::write(&source, "function_sets: []").expect("make source invalid");

        assert!(stage_function_set_manifest(&source, &data_root, false).is_err());
        assert!(
            !staged.exists(),
            "failed stage must not leave stale output available"
        );
    }
}
