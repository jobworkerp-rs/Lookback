//! Runs the cross-thread resolution's decision expressions in the engine's
//! jaq. The TypeScript tests use the jq CLI, which differs on null handling,
//! empty results and string slicing.

use std::{collections::BTreeMap, sync::Arc};

use command_utils::util::jq::execute_jq;
use serde_json::{Value as JsonValue, json};

type JqContext = BTreeMap<String, Arc<JsonValue>>;

fn parse(source: &str) -> serde_yaml::Value {
    serde_yaml::from_str(source).expect("cross-thread workflow YAML")
}

fn parent() -> serde_yaml::Value {
    parse(include_str!(
        "../../workers/workflows/cross-thread-issue-resolution/cross-thread-issue-resolution.yaml"
    ))
}

fn candidate() -> serde_yaml::Value {
    parse(include_str!(
        "../../workers/workflows/cross-thread-issue-resolution/cross-thread-candidate.yaml"
    ))
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

fn shared_apply() -> serde_yaml::Value {
    parse(include_str!(
        "../../workers/workflows/issue-resolution-shared/apply.yaml"
    ))
}

fn shared_prepare() -> serde_yaml::Value {
    parse(include_str!(
        "../../workers/workflows/issue-resolution-shared/prepare.yaml"
    ))
}

fn task<'a>(workflow: &'a serde_yaml::Value, name: &str) -> &'a serde_yaml::Value {
    find_task(workflow, name).unwrap_or_else(|| panic!("missing workflow task {name}"))
}

fn set_expression<'a>(workflow: &'a serde_yaml::Value, name: &str, key: &str) -> &'a str {
    unwrap_expression(&task(workflow, name)["set"][key], name, key)
}

fn if_expression<'a>(workflow: &'a serde_yaml::Value, name: &str) -> &'a str {
    unwrap_expression(&task(workflow, name)["if"], name, "if")
}

fn unwrap_expression<'a>(value: &'a serde_yaml::Value, task: &str, key: &str) -> &'a str {
    let raw = value
        .as_str()
        .unwrap_or_else(|| panic!("{task}.{key} is not an expression"))
        .trim();
    raw.strip_prefix("${")
        .and_then(|expression| expression.strip_suffix('}'))
        .unwrap_or(raw)
        .trim()
}

fn evaluate(filter: &str, bindings: JsonValue) -> JsonValue {
    let context: JqContext = bindings
        .as_object()
        .expect("bindings object")
        .iter()
        .map(|(name, value)| (name.clone(), Arc::new(value.clone())))
        .collect();
    execute_jq(json!({}), filter, &context)
        .unwrap_or_else(|error| panic!("workflow jq expression `{filter}`: {error}"))
}

fn ready(thread_id: &str, decision: &str, memory_id: &str) -> JsonValue {
    json!({
        "thread_id": thread_id,
        "kind": "terminal",
        "decision": decision,
        "ready": true,
        "evidence_thread_ids": [thread_id],
        "evidence_memory_ids": [memory_id],
        "decision_audit": { "rationale": "fixed rationale" },
        "decision_payload": { "reject": {} }
    })
}

#[test]
fn verdicts_aggregate_in_the_engine() {
    let workflow = candidate();
    let filter = set_expression(&workflow, "aggregateVerdicts", "aggregate");
    let aggregate = |verdicts: JsonValue| evaluate(filter, json!({ "verdicts": verdicts }));

    let again = |mut verdict: JsonValue| {
        verdict["round"] = json!(2);
        verdict
    };
    // One confident completion completes the candidate, whatever the
    // other threads answered.
    let applied = aggregate(json!([
        ready("t1", "COMPLETE", "m1"),
        { "thread_id": "t2", "kind": "keep" },
        ready("t3", "COMPLETE", "m3")
    ]));
    assert_eq!(applied["kind"], "apply");
    assert_eq!(applied["decision"], "COMPLETE");
    assert_eq!(
        applied["audit_reason"],
        "fixed rationale [evidence_thread_ids=t1,t3] [evidence_memory_ids=m1,m3]"
    );
    for other in [
        ready("t2", "CANCEL", "m2"),
        json!({ "thread_id": "t2", "kind": "transient", "reason": "x" }),
        json!({ "thread_id": "t2", "kind": "unverifiable", "reason": "y" }),
    ] {
        assert_eq!(
            aggregate(json!([ready("t1", "COMPLETE", "m1"), other]))["decision"],
            "COMPLETE"
        );
    }
    // A cancellation is written only when it repeats and nothing contradicts it.
    let cancel = || {
        json!([
            ready("t1", "CANCEL", "m1"),
            again(ready("t1", "CANCEL", "m1"))
        ])
    };
    assert_eq!(aggregate(cancel())["decision"], "CANCEL");
    assert_eq!(
        aggregate(json!([ready("t1", "CANCEL", "m1")]))["reason"],
        "judgement_not_repeated"
    );
    let with = |other: JsonValue| {
        let mut verdicts = cancel();
        verdicts.as_array_mut().unwrap().push(other);
        aggregate(verdicts)["kind"].clone()
    };
    assert_eq!(
        with(
            json!({ "thread_id": "t2", "kind": "terminal", "decision": "COMPLETE", "ready": false })
        ),
        "contradicted"
    );
    assert_eq!(
        with(json!({ "thread_id": "t2", "kind": "transient", "reason": "x" })),
        "transient"
    );
    assert_eq!(
        with(json!({ "thread_id": "t2", "kind": "unverifiable", "reason": "y" })),
        "unverifiable"
    );
    // jaq's `first` on an empty list and `unique` on nothing must not turn a
    // candidate without judgements into a write.
    assert_eq!(aggregate(json!([]))["kind"], "keep");
}

#[test]
fn judge_outcomes_are_classified_in_the_engine() {
    let workflow = candidate();
    let filter = set_expression(&workflow, "classifyVerdict", "verdicts");
    let classify = |judged: JsonValue| {
        evaluate(
            filter,
            json!({
                "verdicts": [],
                "judge_thread_id": "t1",
                "judge_index": 0,
                "judge_targets": ["t1"],
                "judged": judged
            }),
        )[0]
        .clone()
    };

    assert_eq!(
        classify(json!({ "skipped_candidates": [{ "reason": "no_resolution" }] }))["kind"],
        "keep"
    );
    assert_eq!(
        classify(json!({
            "skipped_candidates": [{ "reason": "confidence_below_threshold" }],
            "llm_outcome": { "decision": "CANCEL" }
        }))["decision"],
        "CANCEL"
    );
    assert_eq!(
        classify(json!({ "failed_candidates": [{ "reason": "invalid resolution LLM response" }] }))
            ["kind"],
        "unverifiable"
    );
    assert_eq!(
        classify(json!({ "transient_failure": true, "failed_candidates": [] }))["kind"],
        "transient"
    );
    // A judgement that reported nothing at all is not a silent "keep".
    assert_eq!(classify(json!({}))["kind"], "unverifiable");
}

#[test]
fn candidate_scope_matches_projects_in_the_engine() {
    let workflow = parent();
    let filter = set_expression(&workflow, "scopeCandidates", "scoped_rows");
    let row = |id: &str, labels: JsonValue, updated_at: &str| {
        json!({
            "candidate": {
                "candidateId": id,
                "metadata": [{
                    "key": "lookback.source_label_snapshots_json",
                    "value": json!([{ "thread_id": "s1", "extraction_run_id": "r", "labels": labels }]).to_string()
                }],
                "sourceEvidence": [{ "threadId": "s1" }],
                "updatedAt": { "unixEpochMillis": updated_at }
            }
        })
    };
    let scoped = evaluate(
        filter,
        json!({
            "since_ms": 1000,
            "workflow": { "input": {} },
            "review_listing": { "review_rows": [
                row("legacy", json!(["repo:git@git.example.com:jobworkerp-rs/Lookback-app.git"]), "1"),
                row("other", json!(["repo:x.org/a/b"]), "1"),
                row("new", json!(["path:/w/app"]), "1000"),
                row("empty", json!(["repo:"]), "5000")
            ]},
            "updated_threads": [
                { "thread_id": "t2", "labels": ["repo:git.example.com/jobworkerp-rs/Lookback-app"] },
                { "thread_id": "t3", "labels": ["repo:"] }
            ]
        }),
    );
    let ids: Vec<_> = scoped
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["review_row"]["candidate"]["candidateId"].clone())
        .collect();
    assert_eq!(ids, vec![json!("legacy"), json!("new")]);
    assert_eq!(scoped[0]["updated_thread_ids"], json!(["t2"]));
    assert_eq!(scoped[1]["candidate_side"], json!(true));
}

#[test]
fn run_range_starts_from_the_last_watermark_in_the_engine() {
    let workflow = parent();
    let filter = set_expression(&workflow, "computeSince", "since_ms");
    let since = |mode: &str, last: JsonValue| {
        evaluate(
            filter,
            json!({
                "mode": mode,
                "workflow": { "input": { "clock_skew_ms": 300000, "initial_lookback_ms": 1000000 } },
                "last_watermark_at": last,
                "run_started_at": 10000000
            }),
        )
    };
    assert_eq!(since("auto", json!(5000000)), json!(4700000));
    assert_eq!(since("auto", JsonValue::Null), json!(9000000));
    assert_eq!(since("manual", json!(5000000)), json!(0));
}

/// The audit reason is shortened with a named-capture regex, whose support
/// differs between jaq and the jq CLI.
#[test]
fn long_audit_reasons_are_fitted_in_the_engine() {
    let apply = shared_apply();
    let filter = unwrap_expression(
        &task(&apply, "invokeResolveCandidate")["run"]["function"]["arguments"]["json_body"],
        "invokeResolveCandidate",
        "json_body",
    );
    let ids: Vec<String> = (0..80).map(|i| format!("7500000000000{i:06}")).collect();
    let reason = format!(
        "fixed [evidence_thread_ids=t1,t2] [evidence_memory_ids={}]",
        ids.join(",")
    );
    let body = evaluate(
        filter,
        json!({
            "candidate_id": "c1",
            "candidate_revision": "2",
            "decision_payload": { "reject": {} },
            "manager_contract": {},
            "manager_store_identity": {},
            "workflow": { "input": {
                "idempotency_key": "k", "actor_id": "a", "audit_reason": reason, "correlation_prefix": "p"
            } }
        }),
    );
    let body: JsonValue = serde_json::from_str(body.as_str().expect("json body string")).unwrap();
    let sent = body["context"]["actor_audit"]["reason"].as_str().unwrap();
    assert!(sent.chars().count() <= 1024, "{}", sent.len());
    assert!(sent.contains("[evidence_thread_ids=t1,t2]"));
    assert!(sent.ends_with(" more]"), "{sent}");
    assert_eq!(body["reject"], json!({}));
}

#[test]
fn only_retryable_failures_hold_the_watermark_in_the_engine() {
    let workflow = parent();
    let filter = if_expression(&workflow, "writeWatermark");
    let writes = |failed: JsonValue| {
        evaluate(
            filter,
            json!({ "mode": "auto", "dry_run": false, "failed_candidates": failed }),
        )
    };
    assert_eq!(writes(json!([])), json!(true));
    assert_eq!(writes(json!([{ "permanent": true }])), json!(true));
    assert_eq!(
        writes(json!([{ "permanent": true }, { "permanent": false }])),
        json!(false)
    );
    assert_eq!(writes(json!([{ "candidate_id": "c1" }])), json!(false));
}

#[test]
fn embedding_failures_are_told_apart_in_the_engine() {
    let workflow = candidate();
    let filter = set_expression(&workflow, "classifyEmbeddingError", "embedding_unusable");
    let unusable = |error: JsonValue| evaluate(filter, json!({ "embedding_error": error }));
    let typed = |kind: &str| json!({ "type": format!("https://serverlessworkflow.io/spec/1.0.0/errors/{kind}") });
    for kind in ["configuration", "validation"] {
        assert_eq!(unusable(typed(kind)), json!(true), "{kind}");
    }
    for kind in ["timeout", "runtime"] {
        assert_eq!(unusable(typed(kind)), json!(false), "{kind}");
    }
    // An error without a type is retried rather than silently degraded.
    assert_eq!(unusable(json!("boom")), json!(false));
}

#[test]
fn permanent_failures_are_marked_in_the_engine() {
    let first_permanent = |workflow: &serde_yaml::Value, name: &str, bindings: JsonValue| {
        let mut bindings = bindings;
        bindings["failed_candidates"] = json!([]);
        bindings["candidate_id"] = json!("c1");
        evaluate(
            set_expression(workflow, name, "failed_candidates"),
            bindings,
        )[0]["permanent"]
            .clone()
    };

    let apply = shared_apply();
    for (error, permanent) in [
        (json!({ "code": "PRECONDITION_FAILED" }), true),
        (json!({ "code": "CANDIDATE_NOT_FOUND" }), true),
        (json!({ "errorCode": "INVALID_STATE_TRANSITION" }), true),
        (json!({ "code": "BACKEND_UNAVAILABLE" }), false),
    ] {
        let bindings =
            json!({ "apply_failure_text": "rejected", "resolution_response": { "error": error } });
        assert_eq!(
            first_permanent(&apply, "recordApplyFailure", bindings),
            json!(permanent),
            "{error}"
        );
    }
    let no_response = json!({ "apply_failure_text": "timeout", "resolution_response": null });
    assert_eq!(
        first_permanent(&apply, "recordApplyFailure", no_response),
        json!(false)
    );

    let prepare = shared_prepare();
    let history_page =
        json!({ "type": "https://lookback.local/errors/issue-resolution/history-page" });
    for (name, key) in [
        ("recordHistoryFailure", "history_error_value"),
        (
            "recordAssociatedIssueHistoryFailure",
            "issue_history_error_value",
        ),
    ] {
        assert_eq!(
            first_permanent(&prepare, name, json!({ key: history_page.clone() })),
            json!(true),
            "{name}"
        );
        assert_eq!(
            first_permanent(&prepare, name, json!({ key: "timeout" })),
            json!(false),
            "{name}"
        );
    }
}
