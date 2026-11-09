use std::collections::BTreeMap;
use std::net::Ipv6Addr;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CredentialReference(String);

impl CredentialReference {
    pub fn new(value: impl Into<String>) -> Result<Self, RepositoryError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 200
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b":._-".contains(&byte))
        {
            return Err(RepositoryError::InvalidInput(
                "credential references must be opaque identifiers".into(),
            ));
        }
        Ok(Self(value))
    }

    pub fn validate(&self) -> Result<(), RepositoryError> {
        Self::new(self.0.clone()).map(|_| ())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForgeKind {
    GitHub,
    Gitea,
}

/// A transport is configured explicitly and is never derived from the port in
/// a repository label. SSH's endpoint and port are separate from the API URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CloneTransport {
    Https {
        base_url: String,
    },
    Ssh {
        host: String,
        user: String,
        port: Option<u16>,
        #[serde(default)]
        ssh_directory: Option<std::path::PathBuf>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryHostConfig {
    /// Canonical host label (lower-case DNS name / IP, with an explicit port
    /// preserved when configured by the source label convention).
    pub label_host: String,
    pub forge: ForgeKind,
    pub api_base_url: String,
    pub clone_transport: CloneTransport,
    pub read_credential_ref: Option<CredentialReference>,
    pub publication_credential_ref: Option<CredentialReference>,
}

impl RepositoryHostConfig {
    pub fn validate(&self) -> Result<(), RepositoryError> {
        if normalize_label_host(&self.label_host).as_deref() != Some(self.label_host.as_str()) {
            return Err(RepositoryError::InvalidInput(
                "host configuration label must be canonical".into(),
            ));
        }
        validate_https_base_url(&self.api_base_url)?;
        match &self.clone_transport {
            CloneTransport::Https { base_url } => {
                validate_https_base_url(base_url)?;
            }
            CloneTransport::Ssh {
                host, user, port, ..
            } => {
                let host_is_canonical_without_port = normalize_label_host(host)
                    .is_some_and(|normalized| normalized == *host)
                    && ((host.starts_with('[') && host.ends_with(']')) || !host.contains(':'));
                if !host_is_canonical_without_port
                    || user.is_empty()
                    || !user
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
                    || *port == Some(0)
                {
                    return Err(RepositoryError::InvalidInput(
                        "invalid configured SSH transport".into(),
                    ));
                }
            }
        }
        Ok(())
    }

    fn clone_url(&self, identity: &RepositoryIdentity) -> Result<String, RepositoryError> {
        self.validate()?;
        match &self.clone_transport {
            CloneTransport::Https { base_url } => {
                let mut url = Url::parse(base_url)
                    .map_err(|_| RepositoryError::InvalidInput("invalid clone base URL".into()))?;
                append_repository_path(&mut url, identity)?;
                Ok(url.to_string())
            }
            CloneTransport::Ssh {
                host, user, port, ..
            } => {
                let normalized_host = normalize_label_host(host).ok_or_else(|| {
                    RepositoryError::InvalidInput("invalid configured SSH host".into())
                })?;
                let ssh_host = match port {
                    Some(port) => format!("{normalized_host}:{port}"),
                    None => normalized_host,
                };
                let mut url = Url::parse(&format!("ssh://{user}@{ssh_host}/"))
                    .map_err(|_| RepositoryError::InvalidInput("invalid SSH transport".into()))?;
                append_repository_path(&mut url, identity)?;
                Ok(url.to_string())
            }
        }
    }

    pub(crate) fn clone_url_for(
        &self,
        identity: &RepositoryIdentity,
    ) -> Result<String, RepositoryError> {
        self.clone_url(identity)
    }

    pub(crate) fn ssh_directory(&self) -> Option<&std::path::Path> {
        match &self.clone_transport {
            CloneTransport::Ssh { ssh_directory, .. } => ssh_directory.as_deref(),
            CloneTransport::Https { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RepositoryIdentity {
    pub host: String,
    pub organization: String,
    pub repository: String,
}

impl RepositoryIdentity {
    pub fn new(
        host: impl AsRef<str>,
        organization: impl Into<String>,
        repository: impl Into<String>,
    ) -> Result<Self, RepositoryError> {
        let host = normalize_label_host(host.as_ref()).ok_or_else(|| {
            RepositoryError::InvalidInput("repository host is not a host label".into())
        })?;
        let organization = organization.into();
        let repository = repository.into();
        if !valid_repository_segment(&organization) || !valid_repository_segment(&repository) {
            return Err(RepositoryError::InvalidInput(
                "repository path contains an unsafe segment".into(),
            ));
        }
        Ok(Self {
            host,
            organization,
            repository,
        })
    }

    pub fn validate(&self) -> Result<(), RepositoryError> {
        let canonical = Self::new(
            &self.host,
            self.organization.clone(),
            self.repository.clone(),
        )?;
        if canonical != *self {
            return Err(RepositoryError::InvalidInput(
                "repository identity is not canonical".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositorySelection {
    SourceLabel,
    ManualApproved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryProvenance {
    /// Retain the source snapshot verbatim; a manual selection must not rewrite
    /// its provenance to make the chosen repository look label-derived.
    pub source_labels: Vec<String>,
    pub selection: RepositorySelection,
    pub approved_by: Option<String>,
    pub approved_at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedRepository {
    pub identity: RepositoryIdentity,
    pub forge: ForgeKind,
    pub api_base_url: String,
    pub clone_url: String,
    pub read_credential_ref: Option<CredentialReference>,
    pub publication_credential_ref: Option<CredentialReference>,
    pub provenance: RepositoryProvenance,
}

impl ResolvedRepository {
    pub fn validate(&self) -> Result<(), RepositoryError> {
        self.identity.validate()?;
        validate_https_base_url(&self.api_base_url)?;
        let clone_url = Url::parse(&self.clone_url)
            .map_err(|_| RepositoryError::InvalidInput("invalid repository clone URL".into()))?;
        if !matches!(clone_url.scheme(), "https" | "ssh")
            || clone_url.host_str().is_none()
            || clone_url.password().is_some()
            || clone_url.query().is_some()
            || clone_url.fragment().is_some()
            || clone_url.path_segments().is_some_and(|mut segments| {
                segments.any(|segment| segment == "." || segment == "..")
            })
            || (clone_url.scheme() == "https" && !clone_url.username().is_empty())
            || (clone_url.scheme() == "ssh"
                && (clone_url.username().is_empty()
                    || !clone_url
                        .username()
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))))
        {
            return Err(RepositoryError::InvalidInput(
                "clone URL must use an explicit credential-free transport".into(),
            ));
        }
        if let Some(reference) = &self.read_credential_ref {
            reference.validate()?;
        }
        if let Some(reference) = &self.publication_credential_ref {
            reference.validate()?;
        }
        if self
            .provenance
            .source_labels
            .iter()
            .any(|label| label.len() > 512 || label.chars().any(char::is_control))
        {
            return Err(RepositoryError::InvalidInput(
                "source label provenance is invalid".into(),
            ));
        }
        match self.provenance.selection {
            RepositorySelection::SourceLabel
                if self.provenance.approved_by.is_some()
                    || self.provenance.approved_at_ms.is_some() =>
            {
                return Err(RepositoryError::InvalidInput(
                    "label-derived repository cannot contain manual approval".into(),
                ));
            }
            RepositorySelection::ManualApproved
                if self
                    .provenance
                    .approved_by
                    .as_deref()
                    .is_none_or(str::is_empty)
                    || self.provenance.approved_by.as_deref().is_some_and(|actor| {
                        actor.len() > 200 || actor.chars().any(char::is_control)
                    })
                    || self.provenance.approved_at_ms.is_none_or(|time| time == 0) =>
            {
                return Err(RepositoryError::InvalidInput(
                    "manual repository selection requires approval provenance".into(),
                ));
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepositorySelectionReason {
    MissingLabel,
    InvalidLabel,
    UnconfiguredHost,
    Ambiguous,
    ContextConflict,
    InvalidHostConfiguration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepositoryResolution {
    Ready(ResolvedRepository),
    RequiresSelection {
        reason: RepositorySelectionReason,
        source_labels: Vec<String>,
        candidates: Vec<RepositoryIdentity>,
    },
}

#[derive(Debug, Error)]
pub enum RepositoryError {
    #[error("invalid repository input: {0}")]
    InvalidInput(String),
    #[error("no matching approved host configuration")]
    UnconfiguredHost,
}

/// Resolve only explicit `repo:` provenance. Invalid or unknown repo labels
/// poison automatic resolution rather than being skipped in favour of a
/// neighbouring valid candidate.
pub fn resolve_repository(
    source_labels: &[String],
    host_configs: &[RepositoryHostConfig],
    repository_context: Option<&RepositoryIdentity>,
) -> RepositoryResolution {
    let source_labels = source_labels.to_vec();
    let configs = match validated_host_map(host_configs) {
        Ok(configs) => configs,
        Err(()) => {
            return RepositoryResolution::RequiresSelection {
                reason: RepositorySelectionReason::InvalidHostConfiguration,
                source_labels,
                candidates: Vec::new(),
            };
        }
    };
    let repo_labels = source_labels
        .iter()
        .filter(|label| label.starts_with("repo:"))
        .collect::<Vec<_>>();
    if repo_labels.is_empty() {
        return RepositoryResolution::RequiresSelection {
            reason: RepositorySelectionReason::MissingLabel,
            source_labels,
            candidates: Vec::new(),
        };
    }

    let mut candidates = BTreeMap::new();
    let mut issue = None;
    let mut seen_labels = std::collections::BTreeSet::new();
    for label in repo_labels {
        if !seen_labels.insert(label.as_str()) {
            continue;
        }
        let Some(identity) = parse_repo_label(label) else {
            issue.get_or_insert(RepositorySelectionReason::InvalidLabel);
            continue;
        };
        let Some(config) = configs.get(&identity.host) else {
            issue.get_or_insert(RepositorySelectionReason::UnconfiguredHost);
            continue;
        };
        candidates.insert(identity.clone(), config);
    }

    let candidate_ids = candidates.keys().cloned().collect::<Vec<_>>();
    let reason = if let Some(reason) = issue {
        Some(reason)
    } else if candidates.len() > 1 {
        Some(RepositorySelectionReason::Ambiguous)
    } else if candidate_ids.is_empty() {
        Some(RepositorySelectionReason::InvalidLabel)
    } else if repository_context.is_some_and(|context| Some(context) != candidate_ids.first()) {
        Some(RepositorySelectionReason::ContextConflict)
    } else {
        None
    };
    if let Some(reason) = reason {
        return RepositoryResolution::RequiresSelection {
            reason,
            source_labels,
            candidates: candidate_ids,
        };
    }

    let identity = candidate_ids
        .into_iter()
        .next()
        .expect("non-empty candidate checked above");
    let config = candidates
        .get(&identity)
        .expect("candidate config inserted with identity");
    let original_source_labels = source_labels.clone();
    match materialize_repository(
        &identity,
        config,
        RepositoryProvenance {
            source_labels,
            selection: RepositorySelection::SourceLabel,
            approved_by: None,
            approved_at_ms: None,
        },
    ) {
        Ok(repository) => RepositoryResolution::Ready(repository),
        Err(_) => RepositoryResolution::RequiresSelection {
            reason: RepositorySelectionReason::InvalidHostConfiguration,
            source_labels: original_source_labels,
            candidates: vec![identity],
        },
    }
}

/// Turn a human-approved fallback into an immutable repository snapshot while
/// retaining the original label snapshot exactly as received.
pub fn approve_manual_repository(
    identity: RepositoryIdentity,
    source_labels: &[String],
    host_configs: &[RepositoryHostConfig],
    approved_by: &str,
    approved_at_ms: u64,
) -> Result<ResolvedRepository, RepositoryError> {
    identity.validate()?;
    if approved_by.is_empty()
        || approved_by.len() > 200
        || approved_by.chars().any(char::is_control)
        || approved_at_ms == 0
    {
        return Err(RepositoryError::InvalidInput(
            "manual repository approval requires a valid actor and timestamp".into(),
        ));
    }
    let configs = validated_host_map(host_configs).map_err(|()| {
        RepositoryError::InvalidInput("invalid approved host configuration".into())
    })?;
    let config = configs
        .get(&identity.host)
        .ok_or(RepositoryError::UnconfiguredHost)?;
    materialize_repository(
        &identity,
        config,
        RepositoryProvenance {
            source_labels: source_labels.to_vec(),
            selection: RepositorySelection::ManualApproved,
            approved_by: Some(approved_by.to_string()),
            approved_at_ms: Some(approved_at_ms),
        },
    )
}

fn materialize_repository(
    identity: &RepositoryIdentity,
    config: &RepositoryHostConfig,
    provenance: RepositoryProvenance,
) -> Result<ResolvedRepository, RepositoryError> {
    identity.validate()?;
    if config.label_host != identity.host {
        return Err(RepositoryError::InvalidInput(
            "repository does not match configured host".into(),
        ));
    }
    let repository = ResolvedRepository {
        identity: identity.clone(),
        forge: config.forge.clone(),
        api_base_url: config.api_base_url.clone(),
        clone_url: config.clone_url(identity)?,
        read_credential_ref: config.read_credential_ref.clone(),
        publication_credential_ref: config.publication_credential_ref.clone(),
        provenance,
    };
    repository.validate()?;
    Ok(repository)
}

fn validated_host_map(
    configs: &[RepositoryHostConfig],
) -> Result<BTreeMap<String, &RepositoryHostConfig>, ()> {
    let mut map = BTreeMap::new();
    for config in configs {
        config.validate().map_err(|_| ())?;
        if map.insert(config.label_host.clone(), config).is_some() {
            return Err(());
        }
    }
    Ok(map)
}

fn parse_repo_label(label: &str) -> Option<RepositoryIdentity> {
    let body = label.strip_prefix("repo:")?;
    let mut segments = body.split('/');
    let host = segments.next()?;
    let organization = segments.next()?;
    let repository = segments.next()?;
    if segments.next().is_some() {
        return None;
    }
    RepositoryIdentity::new(host, organization, repository).ok()
}

fn valid_repository_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && segment.len() <= 255
        && !segment.chars().any(|char| {
            char.is_control()
                || char.is_whitespace()
                || matches!(
                    char,
                    '/' | '\\'
                        | ':'
                        | '@'
                        | '?'
                        | '#'
                        | '%'
                        | '$'
                        | ';'
                        | '&'
                        | '|'
                        | '<'
                        | '>'
                        | '`'
                        | '"'
                        | '\''
                        | '('
                        | ')'
                        | '!'
                )
        })
}

fn normalize_label_host(input: &str) -> Option<String> {
    if input.is_empty()
        || input.len() > 260
        || input
            .chars()
            .any(|char| char.is_control() || char.is_whitespace() || "/\\@?#%".contains(char))
    {
        return None;
    }

    let (host, port) = if let Some(bracketed) = input.strip_prefix('[') {
        let close = bracketed.find(']')?;
        let address = bracketed[..close].parse::<Ipv6Addr>().ok()?;
        let suffix = &bracketed[close + 1..];
        let port = if suffix.is_empty() {
            None
        } else {
            Some(suffix.strip_prefix(':')?.parse::<u16>().ok()?)
        };
        (format!("[{}]", address), port)
    } else {
        let (host, port) = match input.rsplit_once(':') {
            Some((host, port)) if !host.contains(':') => (host, Some(port.parse::<u16>().ok()?)),
            Some(_) => return None,
            None => (input, None),
        };
        if host.is_empty() {
            return None;
        }
        if host.parse::<std::net::Ipv4Addr>().is_err()
            && (host.len() > 253
                || host.split('.').any(|label| {
                    label.is_empty()
                        || label.len() > 63
                        || label.starts_with('-')
                        || label.ends_with('-')
                        || !label
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                }))
        {
            return None;
        }
        (host.to_ascii_lowercase(), port)
    };
    if port == Some(0) {
        return None;
    }
    Some(match port {
        Some(port) => format!("{host}:{port}"),
        None => host,
    })
}

fn validate_https_base_url(value: &str) -> Result<Url, RepositoryError> {
    let url = Url::parse(value)
        .map_err(|_| RepositoryError::InvalidInput("invalid configured HTTPS URL".into()))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url
            .path_segments()
            .is_some_and(|mut segments| segments.any(|segment| segment == "." || segment == ".."))
    {
        return Err(RepositoryError::InvalidInput(
            "configured URL must be credential-free HTTPS without query or traversal".into(),
        ));
    }
    Ok(url)
}

fn append_repository_path(
    url: &mut Url,
    identity: &RepositoryIdentity,
) -> Result<(), RepositoryError> {
    identity.validate()?;
    let mut segments = url.path_segments_mut().map_err(|_| {
        RepositoryError::InvalidInput("configured clone URL cannot contain path segments".into())
    })?;
    segments.pop_if_empty();
    segments
        .push(&identity.organization)
        .push(&identity.repository);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host_config() -> RepositoryHostConfig {
        RepositoryHostConfig {
            label_host: "forge.example:3001".into(),
            forge: ForgeKind::Gitea,
            api_base_url: "https://api.example:9443/v1".into(),
            clone_transport: CloneTransport::Https {
                base_url: "https://clone.example:8443/git".into(),
            },
            read_credential_ref: Some(CredentialReference::new("keychain:repo-read").unwrap()),
            publication_credential_ref: Some(
                CredentialReference::new("keychain:repo-publish").unwrap(),
            ),
        }
    }

    #[test]
    fn one_duplicate_label_resolves_without_guessing_transport_or_path_case() {
        let labels = vec![
            "repo:FORGE.Example:3001/Team/Lookback".into(),
            "repo:FORGE.Example:3001/Team/Lookback".into(),
        ];

        let resolution = resolve_repository(&labels, &[host_config()], None);

        let RepositoryResolution::Ready(repository) = resolution else {
            panic!("one canonical repository should resolve")
        };
        assert_eq!(repository.identity.host, "forge.example:3001");
        assert_eq!(repository.identity.organization, "Team");
        assert_eq!(repository.identity.repository, "Lookback");
        assert_eq!(repository.api_base_url, "https://api.example:9443/v1");
        assert_eq!(
            repository.clone_url,
            "https://clone.example:8443/git/Team/Lookback"
        );
        assert_eq!(repository.provenance.source_labels, labels);
    }

    #[test]
    fn distinct_candidates_and_context_conflicts_require_confirmation() {
        let config = host_config();
        let labels = vec![
            "repo:forge.example:3001/Team/Lookback".into(),
            "repo:forge.example:3001/Other/Tool".into(),
        ];

        assert!(matches!(
            resolve_repository(&labels, std::slice::from_ref(&config), None),
            RepositoryResolution::RequiresSelection {
                reason: RepositorySelectionReason::Ambiguous,
                ..
            }
        ));

        let context = RepositoryIdentity::new("forge.example:3001", "Other", "Tool").unwrap();
        let labels = vec!["repo:forge.example:3001/Team/Lookback".into()];
        assert!(matches!(
            resolve_repository(&labels, &[config], Some(&context)),
            RepositoryResolution::RequiresSelection {
                reason: RepositorySelectionReason::ContextConflict,
                ..
            }
        ));
    }

    #[test]
    fn ssh_clone_transport_uses_its_configured_endpoint_not_the_label_port() {
        let mut config = host_config();
        config.clone_transport = CloneTransport::Ssh {
            host: "ssh.example".into(),
            user: "git".into(),
            port: Some(22),
            ssh_directory: None,
        };
        let labels = vec!["repo:forge.example:3001/Team/Lookback".into()];

        let RepositoryResolution::Ready(repository) = resolve_repository(&labels, &[config], None)
        else {
            panic!("configured SSH host should resolve")
        };
        assert_eq!(
            repository.clone_url,
            "ssh://git@ssh.example:22/Team/Lookback"
        );
        assert!(!repository.clone_url.contains("3001"));
    }

    #[test]
    fn unsafe_or_unconfigured_labels_never_resolve_to_transport_urls() {
        for label in [
            "repo:https://forge.example/Team/Lookback",
            "repo:user@forge.example/Team/Lookback",
            "repo:forge.example/../Lookback",
            "repo:forge.example/Team/Lookback;touch-pwned",
        ] {
            let resolution = resolve_repository(&[label.into()], &[host_config()], None);
            assert!(matches!(
                resolution,
                RepositoryResolution::RequiresSelection {
                    reason: RepositorySelectionReason::InvalidLabel,
                    ..
                }
            ));
        }

        let resolution = resolve_repository(
            &["repo:unconfigured.example:444/Team/Tool".into()],
            &[host_config()],
            None,
        );
        assert!(matches!(
            resolution,
            RepositoryResolution::RequiresSelection {
                reason: RepositorySelectionReason::UnconfiguredHost,
                ..
            }
        ));
    }

    #[test]
    fn manual_approval_preserves_source_label_provenance_verbatim() {
        let labels = vec![
            "repo:forge.example:3001/old/Repo.git/".into(),
            "custom:retained".into(),
        ];
        let identity = RepositoryIdentity::new("forge.example:3001", "Team", "Repo.git").unwrap();
        let approved =
            approve_manual_repository(identity, &labels, &[host_config()], "human:local-user", 123)
                .unwrap();

        assert_eq!(approved.provenance.source_labels, labels);
        assert_eq!(
            approved.provenance.selection,
            RepositorySelection::ManualApproved
        );
        assert_eq!(approved.identity.repository, "Repo.git");
        assert_eq!(
            approved.provenance.approved_by.as_deref(),
            Some("human:local-user")
        );
    }
}
