use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

use super::DEFAULT_EXTRACT_CAP;

pub const EXTRACT_SCHEMA: &str = "lookback-issue-extract.v2";
pub const INPUT_WINDOW_CHAR_LIMIT: usize = 200_000;

const MIN_EXTRACT_CAP: u32 = 1;
const MAX_EXTRACT_CAP: u32 = 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllowedMessage {
    pub memory_id: String,
    pub content: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PreparedExtractCandidate {
    pub title: String,
    pub description: String,
    pub kind: String,
    pub suggested_priority: String,
    pub suggested_state: String,
    pub unfinished_reason: String,
    pub completion: String,
    pub detection: String,
    pub confidence: f64,
    pub locator_memory_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PreparedCandidates {
    pub candidates: Vec<PreparedExtractCandidate>,
    pub mixed: bool,
    pub truncated_total: Option<usize>,
    pub adopted_order: Vec<usize>,
    pub fatal: bool,
}

pub fn content_digest(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    format!("sha256:{digest:x}")
}

pub fn snapshot_digest(summary_ref: &str, messages: &[AllowedMessage]) -> String {
    let entries: Vec<serde_json::Value> = messages
        .iter()
        .map(|message| {
            serde_json::json!({
                "id": message.memory_id,
                "content_digest": content_digest(&message.content),
            })
        })
        .collect();
    let payload = serde_json::json!({
        "summary_ref": summary_ref,
        "messages": entries,
    });
    content_digest(&payload.to_string())
}

/// Matches IssueTrackerServer's canonical extraction snapshot identity.
/// The complete permitted source and the LLM input window are both relevant:
/// a change outside the window must still invalidate evidence derived from the
/// prior extraction.
pub fn canonical_extraction_snapshot_digest(
    thread_id: &str,
    messages: &[AllowedMessage],
) -> String {
    let mut allowed_messages: Vec<(String, String)> = messages
        .iter()
        .map(|message| (message.memory_id.clone(), content_digest(&message.content)))
        .collect();
    allowed_messages.sort_unstable();
    let (input_message_ids, _) = newest_first_window(messages, INPUT_WINDOW_CHAR_LIMIT);
    let payload = serde_json::json!({
        "version": 1,
        "summary_ref": format!("summary:{thread_id}"),
        "input_message_ids": input_message_ids,
        "allowed_messages": allowed_messages,
    });
    content_digest(&payload.to_string())
}

/// Keep newest messages until `limit` characters. `messages` is oldest-first.
pub fn newest_first_window(messages: &[AllowedMessage], limit: usize) -> (Vec<String>, usize) {
    let total: usize = messages
        .iter()
        .map(|message| message.content.chars().count())
        .sum();
    if total <= limit {
        return (
            messages
                .iter()
                .map(|message| message.memory_id.clone())
                .collect(),
            0,
        );
    }
    let mut kept = Vec::new();
    let mut used = 0usize;
    for message in messages.iter().rev() {
        let next = used.saturating_add(message.content.chars().count());
        if !kept.is_empty() && next > limit {
            break;
        }
        used = next;
        kept.push(message.memory_id.clone());
    }
    kept.reverse();
    let omitted = messages.len().saturating_sub(kept.len());
    (kept, omitted)
}

pub fn should_start_extract(summary_json: &serde_json::Value) -> bool {
    summary_json
        .get("has_unfinished_issue_candidates")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

pub fn clamp_extract_cap(raw: u32) -> usize {
    raw.clamp(MIN_EXTRACT_CAP, MAX_EXTRACT_CAP) as usize
}

pub fn extract_cap_from_policy_json(policy_json: &str) -> usize {
    let parsed = serde_json::from_str::<serde_json::Value>(policy_json).ok();
    let cap = parsed
        .as_ref()
        .and_then(|value| {
            value
                .pointer("/policy/extract_cap")
                .or_else(|| value.get("extract_cap"))
        })
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(DEFAULT_EXTRACT_CAP as u64);
    clamp_extract_cap(cap.min(u64::from(u32::MAX)) as u32)
}

pub fn parse_llm_candidates(raw: &str, window_ids: &[String], cap: usize) -> PreparedCandidates {
    let parsed = match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(value) => value,
        Err(_) => return fatal_candidates(),
    };
    let Some(items) = parsed
        .get("candidates")
        .and_then(serde_json::Value::as_array)
    else {
        return fatal_candidates();
    };

    let window: std::collections::HashSet<&str> = window_ids.iter().map(String::as_str).collect();
    let mut kept = Vec::new();
    let mut dropped = 0usize;
    for (index, item) in items.iter().enumerate() {
        match prepare_one(item, &window) {
            Some(candidate) => kept.push((index, candidate)),
            None => dropped += 1,
        }
    }
    let mixed = dropped > 0;
    kept.sort_by(|left, right| compare_candidates(&left.1, &right.1));
    let truncated_total = (kept.len() > cap).then_some(kept.len());
    kept.truncate(cap);
    let adopted_order = kept.iter().map(|(index, _)| *index).collect();
    PreparedCandidates {
        candidates: kept.into_iter().map(|(_, candidate)| candidate).collect(),
        mixed,
        truncated_total,
        adopted_order,
        fatal: false,
    }
}

fn fatal_candidates() -> PreparedCandidates {
    PreparedCandidates {
        candidates: Vec::new(),
        mixed: false,
        truncated_total: None,
        adopted_order: Vec::new(),
        fatal: true,
    }
}

fn prepare_one(
    item: &serde_json::Value,
    window: &std::collections::HashSet<&str>,
) -> Option<PreparedExtractCandidate> {
    let detection = required_string(item, "detection")?;
    if detection == "STRONG" {
        return None;
    }
    if detection != "EXPLICIT" && detection != "INFERRED" {
        return None;
    }
    let locators = item
        .get("locator_memory_ids")
        .and_then(serde_json::Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if locators.is_empty() || locators.iter().any(|id| !window.contains(id.as_str())) {
        return None;
    }
    Some(PreparedExtractCandidate {
        title: required_string(item, "title")?,
        description: required_string(item, "description")?,
        kind: required_string(item, "kind")?,
        suggested_priority: required_string(item, "suggested_priority")?,
        suggested_state: required_string(item, "suggested_state")?,
        unfinished_reason: required_string(item, "unfinished_reason")?,
        completion: required_string(item, "completion")?,
        detection,
        confidence: item.get("confidence").and_then(serde_json::Value::as_f64)?,
        locator_memory_ids: locators,
    })
}

fn required_string(item: &serde_json::Value, key: &str) -> Option<String> {
    item.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn compare_candidates(
    left: &PreparedExtractCandidate,
    right: &PreparedExtractCandidate,
) -> std::cmp::Ordering {
    explicit_rank(&left.detection)
        .cmp(&explicit_rank(&right.detection))
        .then(
            right
                .confidence
                .partial_cmp(&left.confidence)
                .unwrap_or(std::cmp::Ordering::Equal),
        )
        .then_with(|| sort_key(left).cmp(&sort_key(right)))
}

fn explicit_rank(detection: &str) -> u8 {
    if detection == "EXPLICIT" { 0 } else { 1 }
}

fn sort_key(candidate: &PreparedExtractCandidate) -> String {
    format!(
        "{}|{}|{}",
        candidate.kind,
        normalize_key(&candidate.title),
        normalize_key(&candidate.completion)
    )
}

fn normalize_key(value: &str) -> String {
    value
        .nfkc()
        .collect::<String>()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::data::paths::tests::{evaluate_workflow_jq, find_workflow_task};

    fn msg(id: &str, content: &str) -> AllowedMessage {
        AllowedMessage {
            memory_id: id.into(),
            content: content.into(),
        }
    }

    pub(in crate::issue_extract) fn extract_yaml() -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../workers/workflows/issue-extract/issue-extract.yaml");
        std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!("read {}: {e}", path.display());
        })
    }

    #[test]
    fn extract_workflow_pins_schema_and_window_limit() {
        let yaml = extract_yaml();
        assert!(
            yaml.contains(EXTRACT_SCHEMA),
            "extract workflow must use {EXTRACT_SCHEMA}"
        );
        assert_eq!(
            yaml.matches(&format!("extract_schema: \"{EXTRACT_SCHEMA}\""))
                .count(),
            1,
            "the shared ApplyExtraction payload must use {EXTRACT_SCHEMA}"
        );
        assert!(
            yaml.contains(&INPUT_WINDOW_CHAR_LIMIT.to_string()),
            "extract workflow must use window limit {INPUT_WINDOW_CHAR_LIMIT}"
        );
    }

    #[test]
    fn extract_workflow_requires_later_resolution_to_exclude_a_candidate() {
        let yaml = extract_yaml();
        assert!(
            yaml.contains("Evaluate the full conversation in chronological order."),
            "extract prompt must evaluate later messages before returning a candidate"
        );
        assert!(
            yaml.contains("clearly show that the same work was resolved, completed,"),
            "extract prompt must exclude work resolved later in the thread"
        );
        assert!(
            yaml.contains("Do not infer resolution from progress-only"),
            "extract prompt must preserve unresolved candidates when completion is ambiguous"
        );
        assert_eq!(
            yaml.matches("prompt_version: \"lookback-issue-extract.v2\"")
                .count(),
            1,
            "the shared ApplyExtraction payload must identify the strengthened prompt"
        );
    }

    #[test]
    fn extract_workflow_snapshots_labels_from_thread_data() {
        let yaml = extract_yaml();
        assert!(
            yaml.contains("source_thread_data: ${ .jsonBody | fromjson | .data.data }"),
            "ThreadService/Find wraps ThreadData in the outer Thread.data field"
        );
        assert!(
            yaml.contains("if: \"${ $source_thread_data == null }\""),
            "a missing inner ThreadData must prevent an empty label snapshot"
        );
        assert!(
            yaml.contains("source_labels: ($source_thread_data.labels // [])"),
            "ApplyExtraction must persist labels from ThreadData"
        );
    }

    #[test]
    fn the_tracker_tls_follows_the_given_tracker() {
        let workflow: serde_yaml::Value = serde_yaml::from_str(&extract_yaml()).unwrap();
        let task = find_workflow_task(&workflow, "resolveTracker").expect("resolveTracker task");
        let input = serde_json::json!({
            "issue_tracker_grpc_host": "tracker.example",
            "issue_tracker_grpc_port": 443,
            "issue_tracker_grpc_tls": true
        });
        let context = std::collections::BTreeMap::from([(
            "workflow".to_string(),
            serde_json::json!({ "input": input }).into(),
        )]);
        assert_eq!(
            evaluate_workflow_jq(&task["set"]["tracker_tls"], &context).unwrap(),
            serde_json::json!(true)
        );
    }

    #[test]
    fn an_empty_llm_answer_is_shaped_as_corrupt_json() {
        let workflow: serde_yaml::Value = serde_yaml::from_str(&extract_yaml()).unwrap();
        let task = find_workflow_task(&workflow, "shapeCandidates").expect("shapeCandidates task");
        for llm_text in ["", "  \n"] {
            let context = std::collections::BTreeMap::from([
                ("llm_text".to_string(), serde_json::json!(llm_text).into()),
                (
                    "windowed".to_string(),
                    serde_json::json!({ "messages": [] }).into(),
                ),
            ]);
            let shaped = evaluate_workflow_jq(&task["set"]["shaped"], &context).unwrap();
            assert_eq!(
                shaped,
                serde_json::json!({ "candidates": [], "mixed": false, "fatal": true }),
                "{llm_text:?}"
            );
        }
    }

    #[test]
    fn extract_workflow_shapes_llm_output_before_apply() {
        let yaml = extract_yaml();
        let shape = yaml
            .find("shapeCandidates:")
            .expect("shapeCandidates must exist");
        let generate = yaml
            .find("generateCandidates:")
            .expect("generateCandidates");
        let apply = yaml.find("applyExtraction:").expect("applyExtraction");
        assert!(generate < shape, "shape after generate");
        assert!(shape < apply, "shape before apply");
        let shape_block = &yaml[shape..apply];
        assert!(
            !shape_block.contains("extract_cap"),
            "candidate cap is applied by IssueTrackerServer, not the workflow"
        );
        assert!(yaml.contains("json_corrupt"));
        assert!(!shape_block.contains(".kept"));
        assert!(!shape_block.contains(".dropped"));
        assert!(yaml.contains("locator_memory_ids"));
        assert!(
            yaml.contains("candidates: ($shaped.candidates")
                || yaml.contains("candidates: ($shaped.candidates // [])")
        );
        assert!(yaml.contains("mixed: ($shaped.mixed") || yaml.contains("mixed: $shaped.mixed"));
    }

    #[test]
    fn content_digest_is_sha256_hex() {
        assert_eq!(
            content_digest("abc"),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn snapshot_digest_changes_when_message_body_changes() {
        let first = snapshot_digest("summary:1", &[msg("m1", "hello")]);
        let second = snapshot_digest("summary:1", &[msg("m1", "hello!")]);
        let same_ref = snapshot_digest("summary:2", &[msg("m1", "hello")]);
        assert_ne!(first, second);
        assert_ne!(first, same_ref);
    }

    #[test]
    fn newest_first_window_keeps_all_under_limit() {
        let messages = [msg("a", "aa"), msg("b", "bb")];
        let (ids, omitted) = newest_first_window(&messages, 100);
        assert_eq!(ids, ["a", "b"]);
        assert_eq!(omitted, 0);
    }

    #[test]
    fn newest_first_window_drops_old_messages() {
        let messages = [msg("old", "xxxx"), msg("mid", "yyyy"), msg("new", "zz")];
        let (ids, omitted) = newest_first_window(&messages, 6);
        assert_eq!(ids, ["mid", "new"]);
        assert_eq!(omitted, 1);
    }

    #[test]
    fn newest_first_window_counts_unicode_characters_not_bytes() {
        let messages = [msg("old", "ab"), msg("new", "あ")];
        let (ids, omitted) = newest_first_window(&messages, 3);
        assert_eq!(ids, ["old", "new"]);
        assert_eq!(omitted, 0);
    }

    #[test]
    fn newest_first_window_keeps_single_message_over_limit() {
        let huge = "x".repeat(INPUT_WINDOW_CHAR_LIMIT + 1);
        let messages = [msg("huge", &huge)];
        let (ids, omitted) = newest_first_window(&messages, INPUT_WINDOW_CHAR_LIMIT);
        assert_eq!(ids, ["huge"]);
        assert_eq!(omitted, 0);
    }

    #[test]
    fn parse_orders_nfkc_fullwidth_title_with_ascii() {
        let raw = serde_json::json!({
            "candidates": [
                candidate_json("Ａ", "EXPLICIT", 0.9, "in"),
                candidate_json("b", "EXPLICIT", 0.9, "in")
            ]
        })
        .to_string();
        let prepared = parse_llm_candidates(&raw, &["in".into()], 5);
        assert_eq!(prepared.candidates[0].title, "Ａ");
        assert_eq!(prepared.candidates[1].title, "b");
        assert_eq!(prepared.adopted_order, vec![0, 1]);
    }

    #[test]
    fn missing_or_false_signal_does_not_start_extract() {
        assert!(!should_start_extract(&serde_json::json!({})));
        assert!(!should_start_extract(&serde_json::json!({
            "has_unfinished_issue_candidates": false
        })));
        assert!(should_start_extract(&serde_json::json!({
            "has_unfinished_issue_candidates": true
        })));
    }

    #[test]
    fn extract_cap_defaults_and_clamps() {
        assert_eq!(
            extract_cap_from_policy_json(""),
            DEFAULT_EXTRACT_CAP as usize
        );
        assert_eq!(
            extract_cap_from_policy_json(r#"{"policy":{"extract_cap":7}}"#),
            7
        );
        assert_eq!(
            extract_cap_from_policy_json(r#"{"policy":{"extract_cap":0}}"#),
            1
        );
        assert_eq!(
            extract_cap_from_policy_json(r#"{"policy":{"extract_cap":99}}"#),
            20
        );
    }

    fn candidate_json(
        title: &str,
        detection: &str,
        confidence: f64,
        locator: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "title": title,
            "description": "desc",
            "kind": "TASK",
            "suggested_priority": "P2",
            "suggested_state": "OPEN",
            "unfinished_reason": "still open",
            "completion": "done",
            "detection": detection,
            "confidence": confidence,
            "locator_memory_ids": [locator]
        })
    }

    #[test]
    fn parse_drops_strong_and_out_of_window_locators_and_sets_mixed() {
        let raw = serde_json::json!({
            "candidates": [
                candidate_json("keep", "EXPLICIT", 0.9, "in"),
                candidate_json("strong", "STRONG", 0.99, "in"),
                candidate_json("outside", "INFERRED", 0.8, "out")
            ]
        })
        .to_string();
        let prepared = parse_llm_candidates(&raw, &["in".into()], 5);
        assert!(!prepared.fatal);
        assert!(prepared.mixed);
        assert_eq!(prepared.candidates.len(), 1);
        assert_eq!(prepared.candidates[0].title, "keep");
    }

    #[test]
    fn parse_broken_json_is_fatal_and_empty() {
        let prepared = parse_llm_candidates("{nope", &["in".into()], 5);
        assert!(prepared.fatal);
        assert!(prepared.candidates.is_empty());
    }

    #[test]
    fn parse_prefers_explicit_then_confidence_and_caps() {
        let raw = serde_json::json!({
            "candidates": [
                candidate_json("low-inferred", "INFERRED", 0.99, "in"),
                candidate_json("high-explicit", "EXPLICIT", 0.5, "in"),
                candidate_json("higher-explicit", "EXPLICIT", 0.8, "in")
            ]
        })
        .to_string();
        let prepared = parse_llm_candidates(&raw, &["in".into()], 2);
        assert_eq!(prepared.candidates.len(), 2);
        assert_eq!(prepared.candidates[0].title, "higher-explicit");
        assert_eq!(prepared.candidates[1].title, "high-explicit");
        assert_eq!(prepared.truncated_total, Some(3));
        assert_eq!(prepared.adopted_order, vec![2, 1]);
    }
}
