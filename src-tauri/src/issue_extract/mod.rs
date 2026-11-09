//! Phase 4 extract input shaping. Evaluation (fingerprint register, Jaccard,
//! STRONG) belongs on issue-tracker-server.

mod policy;
// The complete extractor shape is shared with YAML and remains test-owned;
// production reconciliation uses only its canonical snapshot identity.
#[allow(dead_code)]
mod shape;

pub(crate) use policy::snapshot_json;
pub use policy::{
    AutoIssuePolicy, AutoRegisterPriority, AutoRegisterPriorityRange, DEFAULT_EXTRACT_CAP,
    DEFAULT_MIN_CONFIDENCE, PolicyRegistry, PolicySnapshot,
};

#[cfg(test)]
pub use shape::{
    AllowedMessage, EXTRACT_SCHEMA, INPUT_WINDOW_CHAR_LIMIT, PreparedCandidates,
    PreparedExtractCandidate, clamp_extract_cap, content_digest, extract_cap_from_policy_json,
    newest_first_window, parse_llm_candidates, should_start_extract, snapshot_digest,
};
