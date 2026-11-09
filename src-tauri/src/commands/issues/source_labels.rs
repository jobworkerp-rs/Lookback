use std::collections::BTreeSet;

use serde::Deserialize;

use crate::grpc::proto::issue_tracker::{manager::v1 as manager, v1};

pub(super) const SOURCE_LABEL_SNAPSHOTS_KEY: &str = "lookback.source_label_snapshots_json";
const LEGACY_SOURCE_CONTEXT_SNAPSHOTS_KEY: &str = "lookback.source_context_snapshots_json";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceLabelSnapshot {
    thread_id: String,
    extraction_run_id: String,
    labels: Vec<String>,
}

/// Pre-label-snapshot provenance also carried project contexts.  Only labels
/// are relevant here, so unknown legacy fields must remain forward-compatible.
#[derive(Debug, Deserialize)]
struct LegacySourceContextSnapshot {
    #[serde(default)]
    labels: Vec<String>,
}

pub(super) fn source_labels_from_candidate(candidate: &manager::Candidate) -> Vec<String> {
    source_labels_from_metadata(
        candidate
            .metadata
            .iter()
            .map(|entry| (entry.key.as_str(), entry.value.as_str())),
    )
}

pub(super) fn source_labels_from_issue(issue: &v1::Issue) -> Vec<String> {
    source_labels_from_metadata(
        issue
            .provenance_metadata
            .iter()
            .map(|entry| (entry.key.as_str(), entry.value.as_str())),
    )
}

fn source_labels_from_metadata<'a>(
    entries: impl Iterator<Item = (&'a str, &'a str)>,
) -> Vec<String> {
    let entries = entries.collect::<Vec<_>>();
    let current = entries
        .iter()
        .filter(|(key, _)| *key == SOURCE_LABEL_SNAPSHOTS_KEY)
        .map(|(_, value)| *value)
        .collect::<Vec<_>>();
    if current.len() == 1 {
        let Ok(snapshots) = serde_json::from_str::<Vec<SourceLabelSnapshot>>(current[0]) else {
            return Vec::new();
        };
        if snapshots
            .iter()
            .any(|snapshot| snapshot.thread_id.is_empty() || snapshot.extraction_run_id.is_empty())
        {
            return Vec::new();
        }
        return flatten_labels(snapshots.into_iter().flat_map(|snapshot| snapshot.labels));
    }
    if !current.is_empty() {
        return Vec::new();
    }

    let legacy = entries
        .iter()
        .filter(|(key, _)| *key == LEGACY_SOURCE_CONTEXT_SNAPSHOTS_KEY)
        .map(|(_, value)| *value)
        .collect::<Vec<_>>();
    if legacy.len() != 1 {
        return Vec::new();
    }
    let Ok(snapshots) = serde_json::from_str::<Vec<LegacySourceContextSnapshot>>(legacy[0]) else {
        return Vec::new();
    };
    flatten_labels(snapshots.into_iter().flat_map(|snapshot| snapshot.labels))
}

fn flatten_labels(labels: impl Iterator<Item = String>) -> Vec<String> {
    labels
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_labels_are_flattened_without_interpreting_their_text() {
        let candidate = manager::Candidate {
            metadata: vec![manager::MetadataEntry {
                key: SOURCE_LABEL_SNAPSHOTS_KEY.into(),
                value: r#"[{"thread_id":"thread-1","extraction_run_id":"run-1","labels":["dir:repo","team:platform"]},{"thread_id":"thread-2","extraction_run_id":"run-2","labels":["path:/repo","team:platform"]}]"#.into(),
            }],
            ..Default::default()
        };

        assert_eq!(
            source_labels_from_candidate(&candidate),
            vec!["dir:repo", "path:/repo", "team:platform"]
        );
    }

    #[test]
    fn malformed_or_ambiguous_provenance_is_not_displayed() {
        let issue = v1::Issue {
            provenance_metadata: vec![
                v1::MetadataEntry {
                    key: SOURCE_LABEL_SNAPSHOTS_KEY.into(),
                    value: "[]".into(),
                },
                v1::MetadataEntry {
                    key: SOURCE_LABEL_SNAPSHOTS_KEY.into(),
                    value: "[]".into(),
                },
            ],
            ..Default::default()
        };

        assert!(source_labels_from_issue(&issue).is_empty());
    }

    #[test]
    fn legacy_context_snapshots_keep_their_labels() {
        let candidate = manager::Candidate {
            metadata: vec![manager::MetadataEntry {
                key: LEGACY_SOURCE_CONTEXT_SNAPSHOTS_KEY.into(),
                value: r#"[{"thread_id":"thread-1","extraction_run_id":"run-1","labels":["path:/repo","team:platform"],"contexts":[{"source_kind":"path","value":"/repo"}]}]"#.into(),
            }],
            ..Default::default()
        };

        assert_eq!(
            source_labels_from_candidate(&candidate),
            vec!["path:/repo", "team:platform"]
        );
    }
}
