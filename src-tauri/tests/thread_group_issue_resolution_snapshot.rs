use std::{collections::BTreeMap, sync::Arc};

use command_utils::util::jq::execute_jq;
use serde_json::{Value as JsonValue, json};

type JqContext = BTreeMap<String, Arc<JsonValue>>;

/// The group workflow followed by the shared workers it calls, searched in
/// that order so a task name the group itself defines wins.
fn workflow() -> serde_yaml::Value {
    let sources = [
        include_str!(
            "../../workers/workflows/thread-group-issue-resolution/thread-group-issue-resolution-batch.yaml"
        ),
        include_str!("../../workers/workflows/issue-resolution-shared/judge.yaml"),
    ];
    serde_yaml::Value::Sequence(
        sources
            .iter()
            .map(|source| serde_yaml::from_str(source).expect("issue-resolution workflow YAML"))
            .collect(),
    )
}

fn find_task<'a>(value: &'a serde_yaml::Value, name: &str) -> Option<&'a serde_yaml::Value> {
    match value {
        serde_yaml::Value::Sequence(items) => items.iter().find_map(|item| find_task(item, name)),
        serde_yaml::Value::Mapping(mapping) => {
            if let Some(task) = mapping.get(serde_yaml::Value::String(name.to_string())) {
                return Some(task);
            }
            mapping.values().find_map(|item| find_task(item, name))
        }
        _ => None,
    }
}

fn task<'a>(workflow: &'a serde_yaml::Value, name: &str) -> &'a serde_yaml::Value {
    find_task(workflow, name).unwrap_or_else(|| panic!("missing workflow task {name}"))
}

fn jq_filter(value: &serde_yaml::Value) -> &str {
    let raw = value.as_str().expect("workflow expression string").trim();
    raw.strip_prefix("${")
        .and_then(|expression| expression.strip_suffix('}'))
        .unwrap_or(raw)
        .trim()
}

fn evaluate(filter: &str, context: &JqContext) -> JsonValue {
    execute_jq(json!({}), filter, context)
        .unwrap_or_else(|error| panic!("workflow jq expression `{filter}`: {error}"))
}

fn context_for_input(input: JsonValue) -> JqContext {
    BTreeMap::from([("workflow".into(), Arc::new(json!({ "input": input })))])
}

fn resolve_snapshot(workflow: &serde_yaml::Value, input: JsonValue) -> JqContext {
    let mut context = context_for_input(input);
    let set = &task(workflow, "resolveExpectedSnapshot")["set"];

    for name in [
        "expected_membership_digest",
        "expected_snapshot_inputs_valid",
        "expected_snapshot_aliases_match",
    ] {
        let value = evaluate(jq_filter(&set[name]), &context);
        context.insert(name.to_string(), Arc::new(value));
    }
    context
}

fn snapshot_is_rejected(workflow: &serde_yaml::Value, context: &JqContext) -> bool {
    evaluate(
        jq_filter(&task(workflow, "validateExpectedSnapshot")["if"]),
        context,
    )
    .as_bool()
    .expect("snapshot-validation predicate")
}

#[test]
fn snapshot_digest_aliases_resolve_unambiguously_and_reject_invalid_inputs() {
    let workflow = workflow();
    let valid_inputs = [
        json!({"expected_membership_snapshot_digest":"digest-a"}),
        json!({"expected_membership_digest":"digest-a"}),
        json!({
            "expected_membership_snapshot_digest":"digest-a",
            "expected_membership_digest":"digest-a"
        }),
    ];

    for input in valid_inputs {
        let context = resolve_snapshot(&workflow, input);
        assert_eq!(
            context["expected_membership_digest"].as_str(),
            Some("digest-a")
        );
        assert!(!snapshot_is_rejected(&workflow, &context));
    }

    let invalid_inputs = [
        json!({
            "expected_membership_snapshot_digest":"canonical",
            "expected_membership_digest":"different"
        }),
        json!({
            "expected_membership_snapshot_digest":"canonical",
            "expected_membership_digest":""
        }),
        json!({
            "expected_membership_snapshot_digest":"",
            "expected_membership_digest":"alias"
        }),
        json!({"expected_membership_snapshot_digest":7}),
        json!({"expected_membership_digest":[]}),
        json!({}),
    ];

    for input in invalid_inputs {
        let context = resolve_snapshot(&workflow, input);
        assert!(snapshot_is_rejected(&workflow, &context));
    }
}

#[test]
fn changed_snapshots_stop_initial_and_prewrite_resolution() {
    let workflow = workflow();
    let mut context = resolve_snapshot(
        &workflow,
        json!({"expected_membership_digest":"snapshot-expected"}),
    );
    context.insert(
        "lineage".into(),
        Arc::new(json!({"group":{"membershipSnapshotDigest":"snapshot-stale"}})),
    );
    assert!(
        evaluate(
            jq_filter(&task(&workflow, "validateInitialMembership")["if"]),
            &context,
        )
        .as_bool()
        .expect("initial membership predicate")
    );

    context.insert("candidate_outcome_recorded".into(), Arc::new(json!(false)));
    context.insert("decision_payload".into(), Arc::new(json!({"apply":true})));
    context.insert(
        "fresh_membership_digest".into(),
        Arc::new(json!("snapshot-stale")),
    );
    assert!(
        evaluate(
            jq_filter(&task(&workflow, "markGroupChanged")["if"]),
            &context,
        )
        .as_bool()
        .expect("prewrite membership predicate")
    );

    let changed_set = &task(&workflow, "markGroupChanged")["set"];
    for name in ["candidate_outcome_recorded", "decision_payload"] {
        let value = evaluate(jq_filter(&changed_set[name]), &context);
        context.insert(name.to_string(), Arc::new(value));
    }
    assert!(
        !evaluate(
            jq_filter(&task(&workflow, "applyResolution")["if"]),
            &context,
        )
        .as_bool()
        .expect("apply-resolution predicate")
    );
}

/// The jq CLI tests cover the matching rules; this runs the same expression in
/// the engine's jaq, whose string slicing and regex support differ from jq.
#[test]
fn project_context_normalizes_repository_labels_in_the_engine() {
    let workflow = workflow();
    let filter = jq_filter(&task(&workflow, "validateProjectContext")["set"]["project_context"]);
    let canonical = "repo:git.example.com/jobworkerp-rs/Lookback-app";
    let context_for = |member: &str| -> JsonValue {
        let context: JqContext = BTreeMap::from([
            (
                "candidate_status".into(),
                Arc::new(json!("CANDIDATE_STATUS_AUTO_REGISTERED")),
            ),
            ("candidate_labels".into(), Arc::new(json!([canonical]))),
            ("issue_labels".into(), Arc::new(json!([canonical]))),
            (
                "member_labels".into(),
                Arc::new(json!([{ "thread_id": "m1", "labels": [member] }])),
            ),
        ]);
        evaluate(filter, &context)
    };

    for member in [
        canonical,
        "repo:git@git.example.com:jobworkerp-rs/Lookback-app.git",
        "repo:https://u@Git.example.com:443/jobworkerp-rs/Lookback-app.git",
    ] {
        assert_eq!(
            context_for(member),
            json!({
                "unknown": false,
                "member_match": true,
                "matching_member_thread_ids": ["m1"]
            }),
            "{member}"
        );
    }
    assert_eq!(
        context_for("repo:git.example.com/jobworkerp-rs/LLM-Memories")["member_match"],
        json!(false)
    );
    // Labels naming no host or no path identify no repository, so a member
    // carrying only those has no context. With an empty host, jaq's
    // `split("@") | last` yields null where the jq CLI raises, so only the
    // engine shows whether these labels are dropped or abort the evaluation.
    for member in ["repo:", "repo:file:///home/x/repo", "repo:https://"] {
        assert_eq!(context_for(member)["unknown"], json!(true), "{member}");
    }
}

/// In the engine, `fromjson` on blank text yields no value and a valueless
/// expression becomes a truthy `[]` (jobworkerp-rs #357); the jq CLI used by
/// the TypeScript tests raises instead, so only this test can catch it.
#[test]
fn blank_llm_output_is_rejected_in_the_engine() {
    let workflow = workflow();
    for (parse_task, text_var, json_var, valid_task, valid_key) in [
        (
            "parsePrimarySelection",
            "primary_llm_text",
            "primary_llm_json",
            "shapePrimarySelection",
            "primary_selection_valid",
        ),
        (
            "parseResolution",
            "llm_text",
            "llm_json",
            "shapeResolution",
            "llm_parse_valid",
        ),
    ] {
        for text in ["", "   ", "not json", "[1]"] {
            let mut context: JqContext = BTreeMap::from([(text_var.into(), Arc::new(json!(text)))]);
            let parsed = evaluate(
                jq_filter(&task(&workflow, parse_task)["set"][json_var]),
                &context,
            );
            assert_eq!(parsed, json!(null), "{parse_task} {text:?}");
            context.insert(json_var.into(), Arc::new(parsed));
            assert_eq!(
                evaluate(
                    jq_filter(&task(&workflow, valid_task)["set"][valid_key]),
                    &context
                ),
                json!(false),
                "{valid_task} {text:?}"
            );
        }
    }
}
