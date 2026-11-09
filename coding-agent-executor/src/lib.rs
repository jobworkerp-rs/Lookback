mod artifact;
mod diagnostics;
mod git_context;
mod model;
mod publication;
mod secret_scan;
mod source;
mod workspace;

use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

pub use model::{
    BaseResolutionReceipt, CapabilityDocument, CaptureGuarantee, CaptureReceipt, CaptureRequest,
    CheckRepositoryChecks, CheckRepositoryRequest, CreatePullRequestRequest,
    CredentialSourceConfig, EXECUTOR_VERSION, ExecutorError, ExecutorErrorCode, ExecutorRequest,
    ExecutorResult, ForgeConfig, ForgeKind, MAX_SOURCE_RESPONSE_BYTES, Operation, PROTOCOL_VERSION,
    PrepareRequest, PreparedWorkspace, PullRequestReceipt, PullRequestRequest, PushReceipt,
    PushRequest, RepositoryCheckResult, RepositoryCheckStage, RepositoryConfig, RepositoryId,
    ResolveBaseRequest, ResolveRequest, ResolvedArtifact, SealRequest, SealedArtifactReceipt,
    SecretScanStatus, SourceFile, SourceInspection, TrustedConfig, WorkspacePushRequest,
};

#[derive(Clone)]
pub struct Executor {
    config: ValidatedConfig,
}

/// Entry point used when Git re-executes this binary as its configured askpass
/// program. Normal callers should use the JSON API instead.
pub fn run_askpass_mode() -> Option<i32> {
    git_context::run_askpass_mode()
}

#[derive(Clone)]
pub(crate) struct ValidatedConfig {
    pub(crate) workspace_root: PathBuf,
    pub(crate) publisher_root: PathBuf,
    pub(crate) state_root: PathBuf,
    pub(crate) credential_root: PathBuf,
    pub(crate) max_import_bytes: u64,
    pub(crate) trusted_config_digest: String,
    repositories: Vec<RepositoryConfig>,
    credential_sources: Vec<CredentialSourceConfig>,
}

#[derive(Clone)]
pub(crate) struct ResolvedCredentialSource {
    pub(crate) username: String,
    pub(crate) token_file: PathBuf,
}

struct PublicationRequestRef<'a> {
    run_id: &'a str,
    attempt_id: &'a str,
    repository: &'a RepositoryId,
    branch: &'a str,
    base_branch: &'a str,
    base_commit: &'a str,
    artifact_digest: &'a str,
    frozen_commit: &'a str,
}

impl Executor {
    pub fn new(config: TrustedConfig) -> Result<Self, ExecutorError> {
        let serialized = serde_json::to_vec(&config)
            .map_err(|_| ExecutorError::new(ExecutorErrorCode::InvalidConfiguration))?;
        let trusted_config_digest = sha256(&serialized);
        let config = validate_config(config)?;
        let home = config.state_root.join("git-home");
        workspace::ensure_private_dir(&config.state_root)?;
        workspace::ensure_private_dir(&config.publisher_root)?;
        workspace::ensure_private_dir(&config.credential_root)?;
        workspace::ensure_private_dir(&home)?;
        Ok(Self {
            config: ValidatedConfig {
                trusted_config_digest,
                ..config
            },
        })
    }

    pub fn from_config_json(bytes: &[u8]) -> Result<Self, ExecutorError> {
        let config: TrustedConfig = serde_json::from_slice(bytes)
            .map_err(|_| ExecutorError::new(ExecutorErrorCode::InvalidConfiguration))?;
        let mut executor = Self::new(config)?;
        executor.config.trusted_config_digest = sha256(bytes);
        Ok(executor)
    }

    pub async fn execute(&self, request: ExecutorRequest) -> Result<ExecutorResult, ExecutorError> {
        if request.protocol_version != PROTOCOL_VERSION {
            return Err(ExecutorError::new(ExecutorErrorCode::IncompatibleProtocol));
        }
        let lookup_only = matches!(
            &request.operation,
            Operation::LookupPush(_) | Operation::LookupPullRequest(_)
        );
        match request.operation {
            Operation::CheckRepository(request) => {
                let repository = self.config.repository(&request.repository)?.clone();
                let result = diagnostics::check_repository(
                    &self.config,
                    &repository,
                    request.checks.as_ref(),
                )
                .await;
                Ok(ExecutorResult::RepositoryChecked(result))
            }
            Operation::ResolveBase(request) => {
                validate_run_id(&request.run_id)?;
                validate_run_id(&request.attempt_id)?;
                validate_run_id(&request.operation_id)?;
                let repository = self.config.repository(&request.repository)?;
                if !request.base_branch.is_empty() {
                    self.config
                        .validate_base_branch(repository, &request.base_branch)?;
                }
                let config = self.config.clone();
                let receipt =
                    tokio::task::spawn_blocking(move || workspace::resolve_base(&config, &request))
                        .await
                        .map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))??;
                Ok(ExecutorResult::BaseResolved(receipt))
            }
            Operation::Prepare(request) => {
                validate_run_id(&request.run_id)?;
                validate_run_id(&request.attempt_id)?;
                validate_full_commit(&request.base_commit)?;
                let repository = self.config.repository(&request.repository)?;
                self.config
                    .validate_base_branch(repository, &request.base_branch)?;
                let config = self.config.clone();
                let prepared = tokio::task::spawn_blocking(move || {
                    workspace::WorkspaceManager::new(&config)?.prepare(&request)
                })
                .await
                .map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))??;
                Ok(ExecutorResult::Prepared(prepared))
            }
            Operation::InspectSource(request) => {
                validate_run_id(&request.run_id)?;
                validate_run_id(&request.attempt_id)?;
                validate_full_commit(&request.base_commit)?;
                let repository = self.config.repository(&request.repository)?;
                self.config
                    .validate_base_branch(repository, &request.base_branch)?;
                let config = self.config.clone();
                let inspection =
                    tokio::task::spawn_blocking(move || source::inspect(&config, &request))
                        .await
                        .map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))??;
                Ok(ExecutorResult::SourceInspected(inspection))
            }
            Operation::Capture(request) => {
                validate_run_id(&request.run_id)?;
                validate_run_id(&request.attempt_id)?;
                validate_full_commit(&request.base_commit)?;
                let repository = self.config.repository(&request.repository)?;
                self.config
                    .validate_base_branch(repository, &request.base_branch)?;
                let config = self.config.clone();
                let receipt =
                    tokio::task::spawn_blocking(move || artifact::capture(&config, &request))
                        .await
                        .map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))??;
                Ok(ExecutorResult::Captured(receipt))
            }
            Operation::Seal(request) => {
                validate_run_id(&request.run_id)?;
                validate_run_id(&request.attempt_id)?;
                let config = self.config.clone();
                let receipt =
                    tokio::task::spawn_blocking(move || artifact::seal(&config, &request))
                        .await
                        .map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))??;
                Ok(ExecutorResult::Sealed(receipt))
            }
            Operation::Resolve(request) => {
                validate_run_id(&request.run_id)?;
                validate_run_id(&request.attempt_id)?;
                let config = self.config.clone();
                let artifact =
                    tokio::task::spawn_blocking(move || artifact::resolve(&config, &request))
                        .await
                        .map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))??;
                Ok(ExecutorResult::Resolved(artifact))
            }
            Operation::Push(request) | Operation::LookupPush(request) => {
                validate_publication_request(
                    &self.config,
                    PublicationRequestRef {
                        run_id: &request.run_id,
                        attempt_id: &request.attempt_id,
                        repository: &request.repository,
                        branch: &request.branch,
                        base_branch: &request.base_branch,
                        base_commit: &request.base_commit,
                        artifact_digest: &request.artifact_digest,
                        frozen_commit: &request.frozen_commit,
                    },
                )?;
                let config = self.config.clone();
                let receipt = tokio::task::spawn_blocking(move || {
                    if lookup_only {
                        publication::lookup_push(&config, &request)
                    } else {
                        publication::push(&config, &request)
                    }
                })
                .await
                .map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))??;
                Ok(ExecutorResult::Pushed(receipt))
            }
            Operation::PushWorkspace(request) => {
                validate_run_id(&request.source.run_id)?;
                validate_run_id(&request.source.attempt_id)?;
                validate_full_commit(&request.source.base_commit)?;
                validate_full_commit(&request.expected_commit)?;
                let repository = self.config.repository(&request.source.repository)?;
                self.config
                    .validate_base_branch(repository, &request.source.base_branch)?;
                if request.source_digest.len() != 64
                    || !request
                        .source_digest
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                {
                    return Err(ExecutorError::new(ExecutorErrorCode::InvalidRequest));
                }
                let config = self.config.clone();
                let receipt = tokio::task::spawn_blocking(move || {
                    publication::push_workspace(&config, &request)
                })
                .await
                .map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))??;
                Ok(ExecutorResult::Pushed(receipt))
            }
            Operation::CreatePullRequest(request) | Operation::LookupPullRequest(request) => {
                validate_publication_request(
                    &self.config,
                    PublicationRequestRef {
                        run_id: &request.run_id,
                        attempt_id: &request.attempt_id,
                        repository: &request.repository,
                        branch: &request.branch,
                        base_branch: &request.base_branch,
                        base_commit: &request.base_commit,
                        artifact_digest: &request.artifact_digest,
                        frozen_commit: &request.frozen_commit,
                    },
                )?;
                validate_pull_request(&request)?;
                publication::create_pull_request(&self.config, &request, lookup_only)
                    .await
                    .map(ExecutorResult::PullRequestCreated)
            }
        }
    }

    pub async fn handle_json(&self, bytes: &[u8]) -> Vec<u8> {
        if bytes.len() > 1024 * 1024 {
            return encode_error_response(ExecutorError::new(ExecutorErrorCode::ResponseTooLarge));
        }
        let response = match serde_json::from_slice::<ExecutorRequest>(bytes) {
            Ok(request) => match self.execute(request).await {
                Ok(result) => model::ExecutorResponse {
                    protocol_version: PROTOCOL_VERSION,
                    outcome: model::ResponseOutcome::Ok {
                        result: Box::new(result),
                    },
                },
                Err(error) => model::ExecutorResponse {
                    protocol_version: PROTOCOL_VERSION,
                    outcome: model::ResponseOutcome::Error {
                        error: error.into(),
                    },
                },
            },
            Err(_) => model::ExecutorResponse {
                protocol_version: PROTOCOL_VERSION,
                outcome: model::ResponseOutcome::Error {
                    error: ExecutorError::new(ExecutorErrorCode::InvalidRequest).into(),
                },
            },
        };
        serde_json::to_vec(&response).unwrap_or_else(|_| {
            br#"{"protocol_version":2,"status":"error","error":{"code":"invalid_request","message":"The request is invalid.","retryable":false}}"#.to_vec()
        })
    }
}

pub fn encode_error_response(error: ExecutorError) -> Vec<u8> {
    let response = model::ExecutorResponse {
        protocol_version: PROTOCOL_VERSION,
        outcome: model::ResponseOutcome::Error {
            error: error.into(),
        },
    };
    serde_json::to_vec(&response).unwrap_or_else(|_| {
        br#"{"protocol_version":2,"status":"error","error":{"code":"invalid_request","message":"The request is invalid.","retryable":false}}"#.to_vec()
    })
}

fn validate_config(config: TrustedConfig) -> Result<ValidatedConfig, ExecutorError> {
    if config.protocol_version != PROTOCOL_VERSION {
        return Err(ExecutorError::new(ExecutorErrorCode::IncompatibleProtocol));
    }
    if config.expected_executor_version != EXECUTOR_VERSION {
        return Err(ExecutorError::new(ExecutorErrorCode::IncompatibleExecutor));
    }
    if config.max_import_bytes == 0 || config.max_import_bytes > 1024 * 1024 * 1024 {
        return Err(model::invalid_configuration());
    }

    let workspace_root = canonical_existing_dir(&config.workspace_root)?;
    let publisher_root = canonical_existing_dir(&config.publisher_root)?;
    let state_root = canonical_existing_dir(&config.state_root)?;
    let credential_root = canonical_existing_dir(&config.credential_root)?;
    let roots = [
        &workspace_root,
        &publisher_root,
        &state_root,
        &credential_root,
    ];
    for left in 0..roots.len() {
        for right in left + 1..roots.len() {
            if roots[left].starts_with(roots[right]) || roots[right].starts_with(roots[left]) {
                return Err(model::invalid_configuration());
            }
        }
    }
    #[cfg(unix)]
    for root in [&publisher_root, &state_root, &credential_root] {
        use std::os::unix::fs::PermissionsExt;
        let metadata = fs::metadata(root).map_err(|_| model::invalid_configuration())?;
        if metadata.permissions().mode() & 0o022 != 0 {
            return Err(model::invalid_configuration());
        }
    }

    if config.repositories.is_empty() {
        return Err(model::invalid_configuration());
    }
    let mut repo_ids = Vec::new();
    for repository in &config.repositories {
        validate_repository_id(&repository.id).map_err(|_| model::invalid_configuration())?;
        if repo_ids.contains(&repository.id) {
            return Err(model::invalid_configuration());
        }
        repo_ids.push(repository.id.clone());
        validate_clone_url(&repository.clone_url).map_err(|_| model::invalid_configuration())?;
        let uses_ssh = repository.clone_url.starts_with("ssh:");
        if uses_ssh != repository.ssh_directory.is_some() {
            return Err(model::invalid_configuration());
        }
        if let Some(directory) = &repository.ssh_directory {
            let directory = validate_ssh_directory(directory)?;
            if directory.starts_with(&workspace_root)
                || workspace_root.starts_with(&directory)
                || directory.starts_with(&publisher_root)
                || publisher_root.starts_with(&directory)
            {
                return Err(model::invalid_configuration());
            }
        }
        if repository.allowed_base_branches.is_empty()
            || repository
                .allowed_base_branches
                .iter()
                .any(|branch| validate_ref_name(branch).is_err() || branch.starts_with('-'))
        {
            return Err(model::invalid_configuration());
        }
        if let Some(forge) = &repository.forge {
            validate_forge_url(&forge.api_base_url).map_err(|_| model::invalid_configuration())?;
            validate_forge_url(&forge.web_base_url).map_err(|_| model::invalid_configuration())?;
        }
    }

    let mut credential_ids = Vec::new();
    for source in &config.credential_sources {
        if !valid_identifier(&source.id)
            || credential_ids.iter().any(|id| id == &source.id)
            || source.username.is_empty()
            || source.username.chars().any(char::is_control)
            || !safe_relative_path(&source.token_file)
        {
            return Err(model::invalid_configuration());
        }
        credential_ids.push(source.id.clone());
        let token_path = validate_credential_path(&credential_root, &source.token_file)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let metadata = fs::metadata(&token_path).map_err(|_| model::invalid_configuration())?;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(model::invalid_configuration());
            }
        }
    }
    for repository in &config.repositories {
        for source in [
            repository.read_credential_source.as_ref(),
            repository.publish_credential_source.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            if !credential_ids.contains(source) {
                return Err(model::invalid_configuration());
            }
        }
    }

    Ok(ValidatedConfig {
        workspace_root,
        publisher_root,
        state_root,
        credential_root,
        max_import_bytes: config.max_import_bytes,
        trusted_config_digest: String::new(),
        repositories: config.repositories,
        credential_sources: config.credential_sources,
    })
}

impl ValidatedConfig {
    pub(crate) fn repository(&self, id: &RepositoryId) -> Result<&RepositoryConfig, ExecutorError> {
        self.repositories
            .iter()
            .find(|repository| &repository.id == id)
            .ok_or_else(|| ExecutorError::new(ExecutorErrorCode::RepositoryNotApproved))
    }

    pub(crate) fn validate_base_branch(
        &self,
        repository: &RepositoryConfig,
        branch: &str,
    ) -> Result<(), ExecutorError> {
        if !repository
            .allowed_base_branches
            .iter()
            .any(|item| item == branch)
        {
            return Err(ExecutorError::new(ExecutorErrorCode::BaseBranchNotApproved));
        }
        validate_ref_name(branch)?;
        if branch.starts_with('-') {
            return Err(ExecutorError::new(ExecutorErrorCode::InvalidBranch));
        }
        Ok(())
    }

    pub(crate) fn credential_source(
        &self,
        id: &str,
    ) -> Result<ResolvedCredentialSource, ExecutorError> {
        let source = self
            .credential_sources
            .iter()
            .find(|source| source.id == id)
            .ok_or_else(model::invalid_configuration)?;
        let token_file = validate_credential_path(&self.credential_root, &source.token_file)?;
        Ok(ResolvedCredentialSource {
            username: source.username.clone(),
            token_file,
        })
    }

    pub(crate) fn read_credential(&self, id: &str) -> Result<(String, String), ExecutorError> {
        let source = self.credential_source(id)?;
        let file = open_read_nofollow(&source.token_file)
            .map_err(|_| ExecutorError::new(ExecutorErrorCode::InvalidConfiguration))?;
        if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
            return Err(ExecutorError::new(ExecutorErrorCode::InvalidConfiguration));
        }
        let mut token = Vec::new();
        file.take(64 * 1024 + 1)
            .read_to_end(&mut token)
            .map_err(|_| ExecutorError::new(ExecutorErrorCode::InvalidConfiguration))?;
        if token.len() > 64 * 1024 {
            return Err(ExecutorError::new(ExecutorErrorCode::InvalidConfiguration));
        }
        while matches!(token.last(), Some(b'\n' | b'\r')) {
            token.pop();
        }
        if token.is_empty()
            || token
                .iter()
                .any(|byte| *byte == b'\0' || *byte == b'\n' || *byte == b'\r')
        {
            return Err(ExecutorError::new(ExecutorErrorCode::InvalidConfiguration));
        }
        let token = String::from_utf8(token)
            .map_err(|_| ExecutorError::new(ExecutorErrorCode::InvalidConfiguration))?;
        Ok((source.username, token))
    }

    pub(crate) fn known_secrets(
        &self,
        repository: &RepositoryConfig,
    ) -> Result<Vec<Vec<u8>>, ExecutorError> {
        let mut result = Vec::new();
        for id in [
            repository.read_credential_source.as_deref(),
            repository.publish_credential_source.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            let (_, token) = self.read_credential(id)?;
            result.push(token.into_bytes());
        }
        Ok(result)
    }
}

fn validate_publication_request(
    config: &ValidatedConfig,
    request: PublicationRequestRef<'_>,
) -> Result<(), ExecutorError> {
    validate_run_id(request.run_id)?;
    validate_run_id(request.attempt_id)?;
    validate_full_commit(request.base_commit)?;
    validate_full_commit(request.frozen_commit)?;
    if request.artifact_digest.len() != 64
        || !request
            .artifact_digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(ExecutorError::new(ExecutorErrorCode::InvalidRequest));
    }
    let approved_repository = config.repository(request.repository)?;
    config.validate_base_branch(approved_repository, request.base_branch)?;
    validate_ref_name(request.branch)?;
    if !branch_belongs_to_run(request.branch, request.run_id) {
        return Err(ExecutorError::new(ExecutorErrorCode::BranchNotApproved));
    }
    Ok(())
}

fn validate_pull_request(request: &CreatePullRequestRequest) -> Result<(), ExecutorError> {
    if request.pull_request.title.is_empty()
        || request.pull_request.title.len() > 500
        || request.pull_request.body.len() > 64 * 1024
        || request
            .pull_request
            .title
            .chars()
            .chain(request.pull_request.body.chars())
            .any(|character| {
                character == '\0'
                    || (character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
            })
    {
        return Err(ExecutorError::new(ExecutorErrorCode::InvalidRequest));
    }
    Ok(())
}

pub(crate) fn validate_run_id(run_id: &str) -> Result<(), ExecutorError> {
    if run_id.is_empty()
        || run_id.len() > 80
        || !run_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(ExecutorError::new(ExecutorErrorCode::InvalidRunId));
    }
    Ok(())
}

pub(crate) fn validate_full_commit(commit: &str) -> Result<(), ExecutorError> {
    if !matches!(commit.len(), 40 | 64)
        || !commit
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(ExecutorError::new(ExecutorErrorCode::InvalidCommit));
    }
    Ok(())
}

/// Check the execution suffix without allowing another run or an arbitrary Git ref.
pub fn branch_belongs_to_run(branch: &str, run_id: &str) -> bool {
    if branch == expected_branch(run_id) {
        return true;
    }
    branch
        .strip_prefix("agent/issue-")
        .and_then(|tail| tail.strip_suffix(&format!("-{run_id}")))
        .is_some_and(|issue| {
            !issue.is_empty()
                && issue.len() <= 64
                && issue
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                && !issue.starts_with(['-', '_'])
                && !issue.ends_with(['-', '_'])
        })
}

pub(crate) fn expected_branch(run_id: &str) -> String {
    format!("agent/{run_id}")
}

fn validate_repository_id(id: &RepositoryId) -> Result<(), ExecutorError> {
    if id.host.is_empty()
        || id.host.len() > 255
        || id.host.contains('/')
        || id.host.contains('@')
        || id.host.chars().any(char::is_control)
        || !safe_segment(&id.owner)
        || !safe_segment(&id.name)
    {
        return Err(model::invalid_configuration());
    }
    let host_url = url::Url::parse(&format!("https://{}", id.host))
        .map_err(|_| model::invalid_configuration())?;
    if host_url.host_str().is_none()
        || host_url.path() != "/"
        || host_url.port().is_none() && id.host.contains(':')
    {
        return Err(model::invalid_configuration());
    }
    Ok(())
}

fn safe_segment(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value.len() <= 100
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn validate_ref_name(name: &str) -> Result<(), ExecutorError> {
    if name.is_empty()
        || name.starts_with('/')
        || name.ends_with('/')
        || name.contains("//")
        || name.contains("..")
        || name.contains("@{")
        || name.bytes().any(|byte| {
            !(byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.'))
        })
        || name.split('/').any(|part| {
            part.is_empty()
                || part.starts_with('.')
                || part.ends_with('.')
                || part.ends_with(".lock")
        })
    {
        return Err(ExecutorError::new(ExecutorErrorCode::InvalidBranch));
    }
    Ok(())
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn validate_clone_url(value: &str) -> Result<(), ExecutorError> {
    let parsed = url::Url::parse(value).map_err(|_| model::invalid_configuration())?;
    if (!parsed.username().is_empty()
        && !(parsed.scheme() == "ssh"
            && parsed
                .username()
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))))
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(model::invalid_configuration());
    }
    match parsed.scheme() {
        "ssh"
            if parsed.host_str().is_some_and(|host| !host.starts_with('-'))
                && parsed
                    .path()
                    .split('/')
                    .filter(|part| !part.is_empty())
                    .count()
                    >= 2 =>
        {
            Ok(())
        }
        "https" if parsed.host_str().is_some() => Ok(()),
        "http" if parsed.host_str().is_some_and(is_loopback_host) => Ok(()),
        // file:// is useful for isolated local executor tests and admin-managed
        // mirrors; the value comes from trusted host configuration only.
        "file" if parsed.host_str().is_none() || parsed.host_str() == Some("localhost") => Ok(()),
        _ => Err(model::invalid_configuration()),
    }
}

fn validate_forge_url(value: &str) -> Result<(), ExecutorError> {
    let parsed = url::Url::parse(value).map_err(|_| model::invalid_configuration())?;
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(model::invalid_configuration());
    }
    match parsed.scheme() {
        "https" if parsed.host_str().is_some() => Ok(()),
        "http" if parsed.host_str().is_some_and(is_loopback_host) => Ok(()),
        _ => Err(model::invalid_configuration()),
    }
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn safe_relative_path(path: &Path) -> bool {
    path.is_relative()
        && !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn canonical_existing_dir(path: &Path) -> Result<PathBuf, ExecutorError> {
    if !path.is_absolute() {
        return Err(model::invalid_configuration());
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| model::invalid_configuration())?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(model::invalid_configuration());
    }
    let canonical = fs::canonicalize(path).map_err(|_| model::invalid_configuration())?;
    if canonical != path {
        return Err(model::invalid_configuration());
    }
    Ok(canonical)
}

/// Validate an explicitly selected SSH directory without reading its secrets.
pub fn validate_ssh_directory(path: &Path) -> Result<PathBuf, ExecutorError> {
    let directory = canonical_existing_dir(path)?;
    if directory.to_str().is_none_or(|path| {
        path.chars()
            .any(|ch| ch.is_control() || matches!(ch, '%' | '$' | '\'' | '"' | '\\'))
    }) {
        return Err(model::invalid_configuration());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let metadata = fs::metadata(&directory).map_err(|_| model::invalid_configuration())?;
        if metadata.permissions().mode() & 0o022 != 0
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            return Err(model::invalid_configuration());
        }
    }
    Ok(directory)
}

fn validate_credential_path(root: &Path, relative: &Path) -> Result<PathBuf, ExecutorError> {
    if !safe_relative_path(relative) {
        return Err(model::invalid_configuration());
    }
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err(model::invalid_configuration());
        };
        current.push(part);
        let metadata =
            fs::symlink_metadata(&current).map_err(|_| model::invalid_configuration())?;
        if metadata.file_type().is_symlink() {
            return Err(model::invalid_configuration());
        }
    }
    let canonical = fs::canonicalize(&current).map_err(|_| model::invalid_configuration())?;
    if !canonical.starts_with(root)
        || !fs::metadata(&canonical).is_ok_and(|metadata| metadata.is_file())
    {
        return Err(model::invalid_configuration());
    }
    Ok(canonical)
}

fn open_read_nofollow(path: &Path) -> std::io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        OpenOptions::new().read(true).open(path)
    }
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::{validate_full_commit, validate_ref_name, validate_run_id};
    use crate::ExecutorErrorCode;

    #[test]
    fn identifiers_and_refs_reject_git_option_and_ref_syntax() {
        assert!(validate_run_id("run-123_a").is_ok());
        assert_eq!(
            validate_run_id("../escape").unwrap_err().code,
            ExecutorErrorCode::InvalidRunId
        );
        assert_eq!(
            validate_ref_name("agent/../../main").unwrap_err().code,
            ExecutorErrorCode::InvalidBranch
        );
        assert_eq!(validate_ref_name("agent/ok").unwrap(), ());
        assert_eq!(validate_full_commit(&"a".repeat(40)).unwrap(), ());
        assert_eq!(
            validate_full_commit("deadbeef").unwrap_err().code,
            ExecutorErrorCode::InvalidCommit
        );
    }
}
