use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::git_context::{GitContext, strings};
use crate::model::{ExecutorError, ExecutorErrorCode};

const MAX_SCANNED_BLOB_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SecretScanReport {
    /// Fixed detector identifiers only. Secret values and repository paths are
    /// deliberately omitted from reports and diagnostics.
    pub findings: Vec<String>,
}

impl SecretScanReport {
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }
}

pub fn scan_bytes(contents: &[u8], known_secrets: &[Vec<u8>]) -> SecretScanReport {
    let mut findings = BTreeSet::new();
    let text = String::from_utf8_lossy(contents);
    if contains_private_key_marker(&text) {
        findings.insert("private_key".to_owned());
    }
    if has_token_prefix(
        &text,
        &[
            ("ghp_", 20),
            ("gho_", 20),
            ("ghu_", 20),
            ("ghs_", 20),
            ("ghr_", 20),
            ("github_pat_", 20),
        ],
    ) {
        findings.insert("github_token".to_owned());
    }
    if has_token_prefix(&text, &[("glpat-", 20)]) {
        findings.insert("gitlab_token".to_owned());
    }
    if has_token_prefix(&text, &[("npm_", 30)]) {
        findings.insert("npm_token".to_owned());
    }
    if has_token_prefix(&text, &[("AKIA", 16)]) {
        findings.insert("aws_access_key".to_owned());
    }
    if has_token_prefix(
        &text,
        &[
            ("xoxb-", 20),
            ("xoxp-", 20),
            ("xoxa-", 20),
            ("xoxr-", 20),
            ("xoxs-", 20),
        ],
    ) {
        findings.insert("slack_token".to_owned());
    }
    if has_token_prefix(&text, &[("sk-", 20), ("sk_live_", 20), ("rk_live_", 20)]) {
        findings.insert("provider_api_key".to_owned());
    }
    if has_token_prefix(&text, &[("AIza", 30)]) {
        findings.insert("google_api_key".to_owned());
    }
    if known_secrets
        .iter()
        .any(|secret| contains_bytes(contents, secret))
    {
        findings.insert("configured_credential".to_owned());
    }
    SecretScanReport {
        findings: findings.into_iter().collect(),
    }
}

pub(crate) fn scan_frozen_history(
    git: &GitContext,
    repository: &Path,
    base_commit: &str,
    frozen_commit: &str,
    known_secrets: &[Vec<u8>],
) -> Result<(), ExecutorError> {
    let revision_range = format!("{base_commit}..{frozen_commit}");
    let commits = git
        .checked(
            repository,
            &strings(&["rev-list", "--reverse", &revision_range]),
            None,
        )
        .map_err(|_| ExecutorError::new(ExecutorErrorCode::SecretScanFailed))?;
    let commits = std::str::from_utf8(&commits)
        .map_err(|_| ExecutorError::new(ExecutorErrorCode::SecretScanFailed))?;
    let mut saw_commit = false;
    for commit in commits.lines().filter(|line| !line.is_empty()) {
        saw_commit = true;
        let raw_commit = git
            .checked_limited(
                repository,
                &strings(&["cat-file", "commit", commit]),
                None,
                MAX_SCANNED_BLOB_BYTES,
            )
            .map_err(|_| ExecutorError::new(ExecutorErrorCode::SecretScanFailed))?;
        if raw_commit.len() > MAX_SCANNED_BLOB_BYTES {
            return Err(ExecutorError::new(ExecutorErrorCode::SecretScanFailed));
        }
        ensure_clean(&raw_commit, known_secrets)?;

        let parents_line = git
            .checked(
                repository,
                &strings(&["rev-list", "--parents", "-n", "1", commit]),
                None,
            )
            .map_err(|_| ExecutorError::new(ExecutorErrorCode::SecretScanFailed))?;
        let parents_line = std::str::from_utf8(&parents_line)
            .map_err(|_| ExecutorError::new(ExecutorErrorCode::SecretScanFailed))?;
        let mut fields = parents_line.split_ascii_whitespace();
        let _self_id = fields.next().ok_or_else(scan_failure)?;
        let parents: Vec<_> = fields.collect();
        if parents.is_empty() {
            // The approved base is an ancestor, so a new root commit cannot be
            // a valid frozen result.
            return Err(scan_failure());
        }
        for parent in parents {
            let output = git
                .checked(repository, &raw_diff_args(parent, commit), None)
                .map_err(|_| scan_failure())?;
            scan_raw_diff(git, repository, &output, known_secrets)?;
        }
    }
    if !saw_commit {
        return Err(ExecutorError::new(ExecutorErrorCode::NoChangesToPublish));
    }
    Ok(())
}

fn raw_diff_args(parent: &str, commit: &str) -> Vec<OsString> {
    [
        "diff-tree",
        "--raw",
        "-r",
        "-z",
        "--no-abbrev",
        "--no-renames",
        "--no-ext-diff",
        "--no-textconv",
        parent,
        commit,
    ]
    .iter()
    .map(OsString::from)
    .collect()
}

fn scan_raw_diff(
    git: &GitContext,
    repository: &Path,
    output: &[u8],
    known_secrets: &[Vec<u8>],
) -> Result<(), ExecutorError> {
    let fields: Vec<&[u8]> = output
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .collect();
    let mut index = 0;
    while index < fields.len() {
        let header = fields[index];
        index += 1;
        let path = fields.get(index).ok_or_else(scan_failure)?;
        index += 1;
        ensure_clean(path, known_secrets)?;
        let header = std::str::from_utf8(header).map_err(|_| scan_failure())?;
        let pieces: Vec<_> = header.split_ascii_whitespace().collect();
        if pieces.len() != 5 || !pieces[0].starts_with(':') {
            return Err(scan_failure());
        }
        let new_mode = pieces[1];
        let new_object = pieces[3];
        if new_mode == "000000" || new_object.bytes().all(|byte| byte == b'0') {
            continue;
        }
        if new_mode == "160000" {
            // Submodule commits are not ordinary blobs and could reference an
            // independently controlled object graph; do not claim they passed
            // the implemented scanner.
            return Err(scan_failure());
        }
        if !new_object
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(scan_failure());
        }
        let contents = git
            .checked_limited(
                repository,
                &strings(&["cat-file", "blob", new_object]),
                None,
                MAX_SCANNED_BLOB_BYTES,
            )
            .map_err(|_| scan_failure())?;
        if contents.len() > MAX_SCANNED_BLOB_BYTES {
            return Err(scan_failure());
        }
        ensure_clean(&contents, known_secrets)?;
    }
    Ok(())
}

fn ensure_clean(contents: &[u8], known_secrets: &[Vec<u8>]) -> Result<(), ExecutorError> {
    if scan_bytes(contents, known_secrets).is_clean() {
        Ok(())
    } else {
        Err(ExecutorError::new(ExecutorErrorCode::SecretDetected))
    }
}

fn contains_private_key_marker(text: &str) -> bool {
    [
        "-----BEGIN PRIVATE KEY-----",
        "-----BEGIN RSA PRIVATE KEY-----",
        "-----BEGIN EC PRIVATE KEY-----",
        "-----BEGIN OPENSSH PRIVATE KEY-----",
        "-----BEGIN DSA PRIVATE KEY-----",
        "-----BEGIN ENCRYPTED PRIVATE KEY-----",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

fn has_token_prefix(text: &str, prefixes: &[(&str, usize)]) -> bool {
    prefixes.iter().any(|(prefix, minimum_length)| {
        text.match_indices(prefix).any(|(start, _)| {
            let suffix = &text[start + prefix.len()..];
            let length = suffix
                .bytes()
                .take_while(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                .count();
            length >= *minimum_length
        })
    })
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn scan_failure() -> ExecutorError {
    ExecutorError::new(ExecutorErrorCode::SecretScanFailed)
}

#[cfg(test)]
mod tests {
    use super::scan_bytes;

    #[test]
    fn reports_secret_categories_without_exposing_values() {
        let secret = [
            &b"ghp_"[..],
            &b"abcdefghijklmnopqrstuvwxyz"[..],
            &b"0123456789"[..],
        ]
        .concat();
        let report = scan_bytes(&secret, &[]);
        assert_eq!(report.findings, ["github_token"]);
        assert!(!format!("{report:?}").contains(std::str::from_utf8(&secret).unwrap()));
        assert!(
            scan_bytes(b"private: -----BEGIN OPENSSH PRIVATE KEY-----", &[])
                .findings
                .contains(&"private_key".to_owned())
        );
        assert!(
            scan_bytes(b"key=AKIA1234567890ABCDEF", &[])
                .findings
                .contains(&"aws_access_key".to_owned())
        );
        assert!(
            scan_bytes(b"api=AIza123456789012345678901234567890", &[])
                .findings
                .contains(&"google_api_key".to_owned())
        );
        // Split like the GitHub token above: a contiguous Stripe-shaped literal
        // is rejected by GitHub push protection when the source is published.
        let stripe_key = [&b"stripe=sk_live_"[..], &b"abcdefghijklmnopqrstuvwxyz"[..]].concat();
        assert!(
            scan_bytes(&stripe_key, &[])
                .findings
                .contains(&"provider_api_key".to_owned())
        );
        assert!(
            scan_bytes(b"fixture token", &[b"fixture token".to_vec()])
                .findings
                .contains(&"configured_credential".to_owned())
        );
    }
}
