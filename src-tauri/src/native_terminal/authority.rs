use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;
use serde::Deserialize;

use crate::{
    coding_agent::{
        HostResourceLimits, RepositoryIdentity, RepositoryResolution, resolve_repository,
        settings::{CodingAgentSettings, CodingAgentSettingsStore},
    },
    commands::IssueTrackerAvailability,
    grpc::{
        issue_tracker::IssueTrackerGrpcService,
        proto::issue_tracker::{manager::v1 as manager, v1},
    },
    sidecar::Sidecars,
};

use super::{
    CanonicalIssueSnapshot, NativeAuthorityError, NativeAuthoritySnapshot, NativeSessionAuthority,
    NativeSessionContext,
};

const AUTHORITY_CORRELATION_ID: &str = "native-terminal-authority-read";
const SOURCE_LABEL_SNAPSHOTS_KEY: &str = "lookback.source_label_snapshots_json";
const LEGACY_SOURCE_CONTEXT_SNAPSHOTS_KEY: &str = "lookback.source_context_snapshots_json";
const MAX_AUTHORITY_ISSUE_ID_BYTES: usize = 512;

#[async_trait]
#[allow(clippy::double_must_use)]
trait NativeAuthorityIssueTrackerClient: Send + Sync {
    async fn contract_probe(&self, correlation_id: &str) -> Result<(), NativeAuthorityError>;

    async fn get_issue_with_sync(
        &self,
        correlation_id: &str,
        issue_id: &str,
    ) -> Result<Option<v1::Issue>, NativeAuthorityError>;
}

struct ManagedIssueTrackerClient(IssueTrackerGrpcService);

#[async_trait]
impl NativeAuthorityIssueTrackerClient for ManagedIssueTrackerClient {
    async fn contract_probe(&self, correlation_id: &str) -> Result<(), NativeAuthorityError> {
        self.0
            .contract_probe(correlation_id)
            .await
            .map(|_| ())
            .map_err(|_| NativeAuthorityError::Unavailable)
    }

    async fn get_issue_with_sync(
        &self,
        correlation_id: &str,
        issue_id: &str,
    ) -> Result<Option<v1::Issue>, NativeAuthorityError> {
        self.0
            .get_issue_with_sync(
                correlation_id,
                manager::GetIssueWithSyncArgs {
                    issue_id: issue_id.to_owned(),
                    ..Default::default()
                },
            )
            .await
            .map(|row| row.issue)
            .map_err(|_| NativeAuthorityError::Unavailable)
    }
}

/// Reads current canonical Issue and native profile authority from the
/// application's managed local services. It never accepts a renderer-selected
/// tracker endpoint or falls back to a remote Issue browsing connection.
#[derive(Clone)]
pub struct ManagedNativeSessionAuthority {
    manual_repository: Option<crate::coding_agent::ResolvedRepository>,
    sidecars: Arc<Sidecars>,
    issue_tracker_availability: Arc<IssueTrackerAvailability>,
    settings: Arc<CodingAgentSettingsStore>,
    image_resolver: Arc<dyn crate::coding_agent::image::ImageDigestResolver>,
    #[cfg(test)]
    test_issue_tracker_client: Option<Arc<dyn NativeAuthorityIssueTrackerClient>>,
}

impl ManagedNativeSessionAuthority {
    pub fn new(
        sidecars: Arc<Sidecars>,
        issue_tracker_availability: Arc<IssueTrackerAvailability>,
        settings: Arc<CodingAgentSettingsStore>,
    ) -> Self {
        Self {
            manual_repository: None,
            sidecars,
            issue_tracker_availability,
            settings,
            image_resolver: Arc::new(crate::coding_agent::image::OciImageDigestResolver::default()),
            #[cfg(test)]
            test_issue_tracker_client: None,
        }
    }

    pub(crate) fn with_manual_repository(
        &self,
        repository: Option<crate::coding_agent::ResolvedRepository>,
    ) -> Self {
        let mut authority = self.clone();
        authority.manual_repository = repository;
        authority
    }

    /// Read current host Issue, repository, profile, and settings authority for
    /// native preparation, before a preview-bound session context exists.
    ///
    /// `validated_host_limits` must come from trusted native host policy, not
    /// renderer input and not an SDK/runtime readiness observation. It is
    /// retained in the immutable prepared context because
    /// `CodingAgentSettingsStore` has no independently versioned host-limit
    /// policy. If such a mutable policy is introduced, its current snapshot and
    /// revision must be reread here as well.
    pub async fn read_for_preparation(
        &self,
        issue_id: &str,
        profile_id: &str,
        validated_host_limits: HostResourceLimits,
    ) -> Result<NativeAuthoritySnapshot, NativeAuthorityError> {
        self.read_for_preparation_inner(issue_id, profile_id, validated_host_limits, None)
            .await
    }

    pub(crate) async fn read_for_automatic_execution(
        &self,
        expected: &NativeAuthoritySnapshot,
    ) -> Result<NativeAuthoritySnapshot, NativeAuthorityError> {
        self.read_for_preparation_inner(
            expected.canonical_issue.issue_id(),
            &expected.profile.profile_id,
            expected.profile.validated_host_limits,
            Some((&expected.profile.image_ref, expected.settings_revision)),
        )
        .await
    }

    async fn read_for_preparation_inner(
        &self,
        issue_id: &str,
        profile_id: &str,
        validated_host_limits: HostResourceLimits,
        frozen_image: Option<(&str, u64)>,
    ) -> Result<NativeAuthoritySnapshot, NativeAuthorityError> {
        if issue_id.is_empty()
            || issue_id.len() > MAX_AUTHORITY_ISSUE_ID_BYTES
            || issue_id.chars().any(char::is_control)
        {
            return Err(NativeAuthorityError::Unavailable);
        }
        let client = self.issue_tracker_client().await?;
        client.contract_probe(AUTHORITY_CORRELATION_ID).await?;
        let issue = client
            .get_issue_with_sync(AUTHORITY_CORRELATION_ID, issue_id)
            .await?
            .ok_or(NativeAuthorityError::Unavailable)?;
        if issue.issue_id != issue_id {
            return Err(NativeAuthorityError::Unavailable);
        }
        let mut settings = self
            .settings
            .load()
            .map_err(|_| NativeAuthorityError::Unavailable)?;
        let revision = settings.revision;
        let profile = settings
            .profiles
            .iter_mut()
            .find(|profile| profile.profile_id == profile_id)
            .ok_or(NativeAuthorityError::Unavailable)?;
        profile.image_ref = if let Some((reference, expected_revision)) = frozen_image {
            if revision != expected_revision {
                return Err(NativeAuthorityError::Unavailable);
            }
            reference.to_owned()
        } else {
            crate::coding_agent::image::resolve_image_reference(
                &profile.image_ref,
                self.image_resolver.as_ref(),
            )
            .await
            .map_err(|_| NativeAuthorityError::Unavailable)?
            .reference
        };
        profile.allowed_image_refs = vec![profile.image_ref.clone()];
        let managed_home_root = self
            .settings
            .managed_profile_home_root()
            .map_err(|_| NativeAuthorityError::Unavailable)?;

        snapshot_from_current_issue_with_selection(
            &issue,
            &settings,
            profile_id,
            validated_host_limits,
            self.manual_repository.as_ref(),
            Some(&managed_home_root),
        )
    }

    async fn issue_tracker_client(
        &self,
    ) -> Result<Arc<dyn NativeAuthorityIssueTrackerClient>, NativeAuthorityError> {
        self.issue_tracker_availability
            .require_endpoint_connected()
            .map_err(|_| NativeAuthorityError::Unavailable)?;
        #[cfg(test)]
        if let Some(client) = self.test_issue_tracker_client.as_ref() {
            return Ok(Arc::clone(client));
        }
        let endpoint = self
            .sidecars
            .issue_tracker_url()
            .ok_or(NativeAuthorityError::Unavailable)?;
        Ok(Arc::new(ManagedIssueTrackerClient(
            IssueTrackerGrpcService::new(endpoint),
        )))
    }

    #[cfg(test)]
    fn with_test_issue_tracker_client(
        mut self,
        client: Arc<dyn NativeAuthorityIssueTrackerClient>,
    ) -> Self {
        self.test_issue_tracker_client = Some(client);
        self
    }
}

#[async_trait]
impl NativeSessionAuthority for ManagedNativeSessionAuthority {
    async fn read_current(
        &self,
        expected: &NativeSessionContext,
    ) -> Result<NativeAuthoritySnapshot, NativeAuthorityError> {
        self.read_for_preparation_inner(
            expected.canonical_issue().issue_id(),
            &expected.profile().profile_id,
            expected.profile().validated_host_limits,
            Some((
                &expected.profile().image_ref,
                expected.preview().expected_settings_revision(),
            )),
        )
        .await
    }
}

#[cfg(test)]
fn snapshot_from_current_issue(
    issue: &v1::Issue,
    settings: &CodingAgentSettings,
    profile_id: &str,
    limits: HostResourceLimits,
) -> Result<NativeAuthoritySnapshot, NativeAuthorityError> {
    snapshot_from_current_issue_with_selection(issue, settings, profile_id, limits, None, None)
}

fn snapshot_from_current_issue_with_selection(
    issue: &v1::Issue,
    settings: &CodingAgentSettings,
    profile_id: &str,
    validated_host_limits: HostResourceLimits,
    selection: Option<&crate::coding_agent::ResolvedRepository>,
    managed_home_root: Option<&std::path::Path>,
) -> Result<NativeAuthoritySnapshot, NativeAuthorityError> {
    let canonical_issue = CanonicalIssueSnapshot::new(
        issue.issue_id.clone(),
        issue.revision,
        issue.title.clone(),
        issue.description.clone(),
        issue.acceptance_criteria.clone(),
    )
    .map_err(|_| NativeAuthorityError::Unavailable)?;

    let source_labels = source_labels_from_issue(issue);
    let repository_context = issue
        .contexts
        .iter()
        .find(|context| v1::ContextKind::try_from(context.kind) == Ok(v1::ContextKind::Repository))
        .and_then(|context| parse_repository_context(&context.value));
    let host_configs = settings
        .repository_hosts_core()
        .map_err(|_| NativeAuthorityError::Unavailable)?;
    let repository = if let Some(selected) = selection {
        if selected.provenance.selection != crate::coding_agent::RepositorySelection::ManualApproved
            || selected.provenance.source_labels != source_labels
        {
            return Err(NativeAuthorityError::Unavailable);
        }
        selected
            .validate()
            .map_err(|_| NativeAuthorityError::Unavailable)?;
        let current = crate::coding_agent::approve_manual_repository(
            selected.identity.clone(),
            &source_labels,
            &host_configs,
            selected
                .provenance
                .approved_by
                .as_deref()
                .ok_or(NativeAuthorityError::Unavailable)?,
            selected
                .provenance
                .approved_at_ms
                .ok_or(NativeAuthorityError::Unavailable)?,
        )
        .map_err(|_| NativeAuthorityError::Unavailable)?;
        if current != *selected {
            return Err(NativeAuthorityError::Unavailable);
        }
        current
    } else {
        match resolve_repository(&source_labels, &host_configs, repository_context.as_ref()) {
            RepositoryResolution::Ready(repository) => repository,
            RepositoryResolution::RequiresSelection { .. } => {
                return Err(NativeAuthorityError::Unavailable);
            }
        }
    };

    let matching_profiles = settings
        .profiles
        .iter()
        .filter(|profile| profile.profile_id == profile_id)
        .collect::<Vec<_>>();
    let [profile_settings] = matching_profiles.as_slice() else {
        return Err(NativeAuthorityError::Unavailable);
    };
    let profile = match managed_home_root {
        Some(root) => profile_settings.to_agent_profile_with_managed_home_root(root),
        None => profile_settings.to_agent_profile(),
    }
    .map_err(|_| NativeAuthorityError::Unavailable)?
    .validate_and_snapshot(&validated_host_limits)
    .map_err(|_| NativeAuthorityError::Unavailable)?;
    if !profile
        .allowed_repositories
        .iter()
        .any(|allowed| allowed == &repository.identity)
    {
        return Err(NativeAuthorityError::Unavailable);
    }

    Ok(NativeAuthoritySnapshot::from_host(
        canonical_issue,
        repository,
        profile,
        settings.revision,
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceLabelSnapshot {
    thread_id: String,
    extraction_run_id: String,
    labels: Vec<String>,
}

/// The current label-snapshot metadata is canonical provenance. Keep its
/// existing legacy fallback for old Issues, but never treat malformed or
/// ambiguous metadata as a repository selection.
fn source_labels_from_issue(issue: &v1::Issue) -> Vec<String> {
    let entries = issue
        .provenance_metadata
        .iter()
        .map(|entry| (entry.key.as_str(), entry.value.as_str()))
        .collect::<Vec<_>>();
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
        return flatten_source_labels(snapshots.into_iter().flat_map(|snapshot| snapshot.labels));
    }
    if !current.is_empty() {
        return Vec::new();
    }

    #[derive(Deserialize)]
    struct LegacySourceContextSnapshot {
        #[serde(default)]
        labels: Vec<String>,
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
    flatten_source_labels(snapshots.into_iter().flat_map(|snapshot| snapshot.labels))
}

fn flatten_source_labels(labels: impl Iterator<Item = String>) -> Vec<String> {
    labels.collect::<BTreeSet<_>>().into_iter().collect()
}

fn parse_repository_context(value: &str) -> Option<RepositoryIdentity> {
    let value = value.strip_prefix("repo:").unwrap_or(value);
    let mut parts = value.split('/');
    let identity = RepositoryIdentity::new(parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    identity.ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coding_agent::{
        ForgeKind, RepositoryProvenance, RepositorySelection,
        settings::{
            AgentProfileSettings, CloneTransportSettings, RepositoryHostSettings,
            RepositoryIdentitySettings, SandboxResourcesSettings,
        },
    };
    use crate::{
        commands::IssueTrackerAvailability,
        data::DataPaths,
        sidecar::{SidecarConfig, Sidecars},
    };

    mod native_terminal_authority_rpc {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/native_terminal_authority_rpc.rs"
        ));
    }
    use native_terminal_authority_rpc::NativeAuthorityRpcFixture;

    const ISSUE_ID: &str = "issue-42";
    const PROFILE_ID: &str = "native-opencode";
    const IMAGE: &str = "registry.example/agents/opencode@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn settings(allowed_repository: &str) -> CodingAgentSettings {
        CodingAgentSettings {
            format_version: 1,
            revision: 37,
            active_profile_id: Some(PROFILE_ID.into()),
            semantic_judge_enabled: true,
            semantic_judge_policy: Default::default(),
            auto_done_enabled: false,
            profiles: vec![AgentProfileSettings {
                profile_id: PROFILE_ID.into(),
                version: 8,
                display_name: "Native OpenCode".into(),
                image_ref: IMAGE.into(),
                allowed_image_refs: vec![IMAGE.into()],
                command: vec!["opencode".into(), "run".into()],
                container_home: "/home/agent".into(),
                host_configuration: None,
                runtime_environment: Default::default(),
                network_access: Default::default(),
                resources: SandboxResourcesSettings {
                    cpu_millis: 1000,
                    memory_mib: 2048,
                    root_disk_mib: 8192,
                    max_parallel_runs: 1,
                    use_static: false,
                },
                mount_allowed_roots: Vec::new(),
                persistent_mounts: Vec::new(),
                allowed_repositories: vec![identity_settings(allowed_repository)],
                credential_reference: Some("native.read".into()),
            }],
            repository_hosts: vec![RepositoryHostSettings {
                label_host: "forge.example".into(),
                forge: ForgeKind::Gitea,
                api_base_url: "https://forge.example/api/v1".into(),
                clone_transport: CloneTransportSettings::Https {
                    base_url: "https://forge.example/git".into(),
                },
                read_credential_ref: Some("repo.read".into()),
                publication_credential_ref: Some("repo.publish".into()),
            }],
            native_repository_approvals: None,
            native_backend_package_fingerprint: None,
        }
    }

    fn identity_settings(repository: &str) -> RepositoryIdentitySettings {
        RepositoryIdentitySettings {
            host: "forge.example".into(),
            organization: "team".into(),
            repository: repository.into(),
        }
    }

    fn issue(revision: u64, labels: &[&str]) -> v1::Issue {
        let label_snapshot = serde_json::json!([{
            "thread_id": "42",
            "extraction_run_id": "extract-9",
            "labels": labels,
        }]);
        v1::Issue {
            issue_id: ISSUE_ID.into(),
            title: "Issue #42 — preserve the canonical title".into(),
            description: "First paragraph.\n\nSecond paragraph.".into(),
            acceptance_criteria: vec![
                "first criterion\nwith its original newline".into(),
                "second criterion".into(),
            ],
            contexts: vec![v1::ContextRef {
                kind: v1::ContextKind::Repository as i32,
                value: "forge.example/team/project".into(),
                uri: None,
            }],
            revision,
            provenance_metadata: vec![v1::MetadataEntry {
                key: SOURCE_LABEL_SNAPSHOTS_KEY.into(),
                value: label_snapshot.to_string(),
            }],
            ..Default::default()
        }
    }

    fn host_limits() -> crate::coding_agent::HostResourceLimits {
        crate::coding_agent::HostResourceLimits {
            cpu_millis: 4000,
            memory_mib: 8192,
            root_disk_mib: 32768,
            max_parallel_runs: 4,
        }
    }

    #[test]
    fn owned_claim_revision_does_not_authorize_changed_task_repository_or_profile() {
        let settings = settings("project");
        let issue = issue(7, &["repo:forge.example/team/project"]);
        let expected =
            snapshot_from_current_issue(&issue, &settings, PROFILE_ID, host_limits()).unwrap();
        let mut advanced = issue.clone();
        advanced.revision = 8;
        let current =
            snapshot_from_current_issue(&advanced, &settings, PROFILE_ID, host_limits()).unwrap();
        assert!(super::super::preparation::same_authority_at_owned_revision(
            &expected, &current, 8
        ));
        assert!(
            !super::super::preparation::same_authority_at_owned_revision(&expected, &current, 9)
        );
        advanced.description.push_str(" changed");
        let changed =
            snapshot_from_current_issue(&advanced, &settings, PROFILE_ID, host_limits()).unwrap();
        assert!(
            !super::super::preparation::same_authority_at_owned_revision(&expected, &changed, 8)
        );
        let mutations: [fn(&mut NativeAuthoritySnapshot); 3] = [
            |snapshot: &mut NativeAuthoritySnapshot| snapshot.settings_revision += 1,
            |snapshot: &mut NativeAuthoritySnapshot| snapshot.profile.version += 1,
            |snapshot: &mut NativeAuthoritySnapshot| {
                snapshot.repository.clone_url.push_str("-other")
            },
        ];
        for mutate in mutations {
            let mut changed = current.clone();
            mutate(&mut changed);
            assert!(
                !super::super::preparation::same_authority_at_owned_revision(
                    &expected, &changed, 8
                )
            );
        }
    }

    fn test_sidecars(root: &std::path::Path) -> Arc<Sidecars> {
        let data = DataPaths::with_root(root.join("app-data"));
        let lance_language_model_home = data.lance_language_model_home();
        Arc::new(Sidecars::new(SidecarConfig {
            jobworkerp_bin: "/bin/true".into(),
            memories_bin: "/bin/true".into(),
            memories_db_migrate_bin: "/bin/true".into(),
            startup_database_migration_enabled: false,
            conductor_bin: "/bin/true".into(),
            data,
            worker_yaml_paths: Vec::new(),
            issue_tracker_server_bin: None,
            function_set_yaml_paths: Vec::new(),
            reflection_dispatch_enabled: false,
            auto_embedding_enabled: false,
            workflows_dir: None,
            lance_language_model_home,
            lindera_dict_staged: false,
            llm_model: None,
            llm_hf_repo: None,
            llm_ctx_size: None,
            llm_kv_cache_type: None,
            env_file: None,
        }))
    }

    fn settings_store(root: &std::path::Path) -> Arc<CodingAgentSettingsStore> {
        let path = root.join("coding-agent/settings.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_vec(&settings("project")).unwrap()).unwrap();
        Arc::new(CodingAgentSettingsStore::new(path))
    }

    #[tokio::test]
    async fn started_execution_keeps_its_image_pin_without_resolving_the_tag_again() {
        struct UnavailableResolver;
        #[async_trait]
        impl crate::coding_agent::image::ImageDigestResolver for UnavailableResolver {
            async fn manifest_digest(
                &self,
                _: &str,
            ) -> Result<String, crate::coding_agent::image::ImageResolutionError> {
                panic!("a started execution must not resolve a mutable tag again")
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let configured = settings("project");
        let expected = snapshot_from_current_issue(
            &issue(7, &["repo:forge.example/team/project"]),
            &configured,
            PROFILE_ID,
            host_limits(),
        )
        .unwrap();
        let store = settings_store(temp.path());
        let mut mutable = configured;
        mutable.profiles[0].image_ref = "registry.example/agents/opencode:mutable".into();
        mutable.profiles[0].allowed_image_refs = vec![mutable.profiles[0].image_ref.clone()];
        std::fs::write(
            temp.path().join("coding-agent/settings.json"),
            serde_json::to_vec(&mutable).unwrap(),
        )
        .unwrap();
        let fixture =
            NativeAuthorityRpcFixture::start(issue(8, &["repo:forge.example/team/project"])).await;
        let availability = Arc::new(IssueTrackerAvailability::default());
        availability.mark_endpoint_connected();
        let mut authority =
            ManagedNativeSessionAuthority::new(test_sidecars(temp.path()), availability, store)
                .with_test_issue_tracker_client(Arc::new(ManagedIssueTrackerClient(
                    IssueTrackerGrpcService::new(fixture.endpoint()),
                )));
        authority.image_resolver = Arc::new(UnavailableResolver);
        let current = authority
            .read_for_automatic_execution(&expected)
            .await
            .unwrap();
        assert_eq!(current.profile, expected.profile);
        assert_eq!(current.canonical_issue.revision(), 8);
        mutable.revision += 1;
        std::fs::write(
            temp.path().join("coding-agent/settings.json"),
            serde_json::to_vec(&mutable).unwrap(),
        )
        .unwrap();
        assert!(
            authority
                .read_for_automatic_execution(&expected)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn selected_tag_and_multiple_candidate_tags_use_only_the_resolved_runtime_pin() {
        struct Resolver;
        #[async_trait]
        impl crate::coding_agent::image::ImageDigestResolver for Resolver {
            async fn manifest_digest(
                &self,
                _: &str,
            ) -> Result<String, crate::coding_agent::image::ImageResolutionError> {
                Ok(format!("sha256:{}", "a".repeat(64)))
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let fixture =
            NativeAuthorityRpcFixture::start(issue(23, &["repo:forge.example/team/project"])).await;
        let availability = Arc::new(IssueTrackerAvailability::default());
        availability.mark_endpoint_connected();
        let store = settings_store(temp.path());
        let mut configured = settings("project");
        configured.profiles[0].image_ref = "registry.example/agents/opencode:release".into();
        configured.profiles[0].allowed_image_refs = vec![
            configured.profiles[0].image_ref.clone(),
            "future.registry/org/coder:next".into(),
            IMAGE.into(),
        ];
        std::fs::write(
            temp.path().join("coding-agent/settings.json"),
            serde_json::to_vec(&configured).unwrap(),
        )
        .unwrap();
        let mut authority =
            ManagedNativeSessionAuthority::new(test_sidecars(temp.path()), availability, store)
                .with_test_issue_tracker_client(Arc::new(ManagedIssueTrackerClient(
                    IssueTrackerGrpcService::new(fixture.endpoint()),
                )));
        authority.image_resolver = Arc::new(Resolver);
        let snapshot = authority
            .read_for_preparation(ISSUE_ID, PROFILE_ID, host_limits())
            .await
            .unwrap();
        assert_eq!(snapshot.profile.image_ref, IMAGE);
        snapshot.profile.validate_snapshot().unwrap();
        assert_eq!(snapshot.settings_revision, configured.revision);
        let saved = authority.settings.load().unwrap();
        assert_eq!(
            saved.profiles[0].allowed_image_refs,
            configured.profiles[0].allowed_image_refs
        );
        assert_eq!(
            saved.profiles[0].image_ref,
            configured.profiles[0].image_ref
        );
        fixture.stop().await;
    }

    #[tokio::test]
    async fn preparation_reads_the_requested_canonical_issue_and_current_profile_from_host_services()
     {
        let temp = tempfile::tempdir().unwrap();
        let current_issue = issue(23, &["team:native", "repo:forge.example/team/project"]);
        let fixture = NativeAuthorityRpcFixture::start(current_issue).await;
        let availability = Arc::new(IssueTrackerAvailability::default());
        availability.mark_endpoint_connected();
        let authority = ManagedNativeSessionAuthority::new(
            test_sidecars(temp.path()),
            availability,
            settings_store(temp.path()),
        )
        .with_test_issue_tracker_client(Arc::new(ManagedIssueTrackerClient(
            IssueTrackerGrpcService::new(fixture.endpoint()),
        )));

        let result = authority
            .read_for_preparation(ISSUE_ID, PROFILE_ID, host_limits())
            .await;
        assert!(
            result.is_ok(),
            "host-canonical Issue and approved profile should be ready: {result:?}; RPC: {:?}",
            fixture.observations()
        );
        let snapshot = result.unwrap();

        assert_eq!(snapshot.canonical_issue.issue_id(), ISSUE_ID);
        assert_eq!(snapshot.canonical_issue.revision(), 23);
        assert_eq!(
            snapshot.canonical_issue.title(),
            "Issue #42 — preserve the canonical title"
        );
        assert_eq!(
            snapshot.canonical_issue.description(),
            "First paragraph.\n\nSecond paragraph."
        );
        assert_eq!(
            snapshot.canonical_issue.acceptance_criteria(),
            [
                "first criterion\nwith its original newline",
                "second criterion"
            ]
        );
        assert_eq!(snapshot.settings_revision, 37);
        assert_eq!(snapshot.profile.version, 8);
        assert_eq!(snapshot.profile.image_ref, IMAGE);
        assert_eq!(snapshot.profile.resources.cpu_millis, 1000);
        assert!(!snapshot.profile.resources.use_static);
        assert_eq!(snapshot.repository.identity.repository, "project");
        assert_eq!(
            snapshot.repository.api_base_url,
            "https://forge.example/api/v1"
        );
        assert_eq!(
            snapshot.repository.clone_url,
            "https://forge.example/git/team/project"
        );
        let observations = fixture.observations();
        assert_eq!(observations.contract_probe_count, 1);
        assert_eq!(observations.get_issue_count, 1);
        assert_eq!(
            observations.contract_context.unwrap().correlation_id,
            AUTHORITY_CORRELATION_ID
        );
        let get_args = observations.get_issue_args.unwrap();
        assert_eq!(get_args.issue_id, ISSUE_ID);
        let read_context = get_args.context.unwrap();
        assert_eq!(read_context.correlation_id, AUTHORITY_CORRELATION_ID);
        assert_eq!(read_context.data_root_id, "native-authority-test-root");
        assert_eq!(
            read_context.schema_fingerprint,
            "authority-schema-fingerprint"
        );
        assert_eq!(
            read_context.contract_fingerprint,
            "authority-contract-fingerprint"
        );
        fixture.stop().await;
    }

    #[tokio::test]
    async fn preparation_rejects_an_issue_tracker_response_for_a_different_issue_id() {
        let temp = tempfile::tempdir().unwrap();
        let mut returned_issue = issue(23, &["repo:forge.example/team/project"]);
        returned_issue.issue_id = "issue-elsewhere".into();
        let fixture = NativeAuthorityRpcFixture::start(returned_issue).await;
        let availability = Arc::new(IssueTrackerAvailability::default());
        availability.mark_endpoint_connected();
        let authority = ManagedNativeSessionAuthority::new(
            test_sidecars(temp.path()),
            availability,
            settings_store(temp.path()),
        )
        .with_test_issue_tracker_client(Arc::new(ManagedIssueTrackerClient(
            IssueTrackerGrpcService::new(fixture.endpoint()),
        )));

        assert!(matches!(
            authority
                .read_for_preparation(ISSUE_ID, PROFILE_ID, host_limits())
                .await,
            Err(NativeAuthorityError::Unavailable)
        ));
        fixture.stop().await;
    }

    #[test]
    fn manual_selection_preserves_labels_and_rechecks_current_authority() {
        for labels in [
            vec![],
            vec![
                "repo:forge.example/team/project",
                "repo:forge.example/team/other",
            ],
        ] {
            let original = issue(19, &labels);
            let configured = settings("project");
            let chosen = crate::coding_agent::approve_manual_repository(
                RepositoryIdentity::new("forge.example", "team", "project").unwrap(),
                &source_labels_from_issue(&original),
                &configured.repository_hosts_core().unwrap(),
                "user",
                42,
            )
            .unwrap();
            let selected = snapshot_from_current_issue_with_selection(
                &original,
                &configured,
                PROFILE_ID,
                host_limits(),
                Some(&chosen),
                None,
            )
            .unwrap();
            assert_eq!(selected.repository, chosen);
            assert_eq!(
                selected.repository.provenance.source_labels,
                source_labels_from_issue(&original)
            );
            let changed = issue(20, &["repo:forge.example/team/changed"]);
            assert!(
                snapshot_from_current_issue_with_selection(
                    &changed,
                    &configured,
                    PROFILE_ID,
                    host_limits(),
                    Some(&chosen),
                    None,
                )
                .is_err()
            );
            let disallowed = settings("other");
            assert!(
                snapshot_from_current_issue_with_selection(
                    &original,
                    &disallowed,
                    PROFILE_ID,
                    host_limits(),
                    Some(&chosen),
                    None,
                )
                .is_err()
            );
            let mut changed_host = configured.clone();
            changed_host.repository_hosts[0].api_base_url = "https://changed.example".into();
            assert!(
                snapshot_from_current_issue_with_selection(
                    &original,
                    &changed_host,
                    PROFILE_ID,
                    host_limits(),
                    Some(&chosen),
                    None,
                )
                .is_err()
            );
        }
    }

    #[test]
    fn authority_snapshot_uses_canonical_issue_profile_and_complete_repository_values() {
        let current = issue(19, &["team:native", "repo:forge.example/team/project"]);
        let settings = settings("project");

        let snapshot = snapshot_from_current_issue(&current, &settings, PROFILE_ID, host_limits())
            .expect("a fully configured canonical source is ready");

        assert_eq!(snapshot.canonical_issue.issue_id(), ISSUE_ID);
        assert_eq!(snapshot.canonical_issue.revision(), 19);
        assert_eq!(
            snapshot.canonical_issue.title(),
            "Issue #42 — preserve the canonical title"
        );
        assert_eq!(
            snapshot.canonical_issue.description(),
            "First paragraph.\n\nSecond paragraph."
        );
        assert_eq!(
            snapshot.canonical_issue.acceptance_criteria(),
            [
                "first criterion\nwith its original newline",
                "second criterion"
            ]
        );
        assert_eq!(snapshot.settings_revision, 37);
        assert_eq!(snapshot.profile.profile_id, PROFILE_ID);
        assert_eq!(snapshot.profile.version, 8);
        assert!(!snapshot.profile.resources.use_static);
        assert_eq!(
            snapshot.profile.allowed_repositories[0].repository,
            "project"
        );
        assert_eq!(
            snapshot
                .profile
                .credential_reference
                .as_ref()
                .unwrap()
                .as_str(),
            "native.read"
        );
        assert_eq!(snapshot.repository.identity.repository, "project");
        assert_eq!(
            snapshot.repository.api_base_url,
            "https://forge.example/api/v1"
        );
        assert_eq!(
            snapshot.repository.clone_url,
            "https://forge.example/git/team/project"
        );
        assert_eq!(
            snapshot
                .repository
                .read_credential_ref
                .as_ref()
                .unwrap()
                .as_str(),
            "repo.read"
        );
        assert_eq!(
            snapshot
                .repository
                .publication_credential_ref
                .as_ref()
                .unwrap()
                .as_str(),
            "repo.publish"
        );
        assert_eq!(
            snapshot.repository.provenance,
            RepositoryProvenance {
                source_labels: vec![
                    "repo:forge.example/team/project".into(),
                    "team:native".into()
                ],
                selection: RepositorySelection::SourceLabel,
                approved_by: None,
                approved_at_ms: None,
            }
        );
    }

    #[test]
    fn authority_snapshot_does_not_invent_a_repository_for_unresolvable_source_labels() {
        let malformed = issue(19, &["repo:forge.example/team"]);
        let unknown_host = issue(19, &["repo:unknown.example/team/project"]);
        let ambiguous = issue(
            19,
            &[
                "repo:forge.example/team/project",
                "repo:forge.example/team/other",
            ],
        );
        let settings = settings("project");

        for source in [malformed, unknown_host, ambiguous] {
            assert!(matches!(
                snapshot_from_current_issue(&source, &settings, PROFILE_ID, host_limits()),
                Err(NativeAuthorityError::Unavailable)
            ));
        }
    }

    #[test]
    fn authority_snapshot_rejects_revision_zero_and_profiles_without_exact_repository_approval() {
        let zero_revision_issue = issue(0, &["repo:forge.example/team/project"]);
        let valid_settings = settings("project");
        assert!(matches!(
            snapshot_from_current_issue(
                &zero_revision_issue,
                &valid_settings,
                PROFILE_ID,
                host_limits()
            ),
            Err(NativeAuthorityError::Unavailable)
        ));

        let issue = issue(19, &["repo:forge.example/team/project"]);
        let disallowed_settings = settings("different-project");
        assert!(matches!(
            snapshot_from_current_issue(&issue, &disallowed_settings, PROFILE_ID, host_limits()),
            Err(NativeAuthorityError::Unavailable)
        ));

        let mut duplicate_profile_settings = settings("project");
        duplicate_profile_settings
            .profiles
            .push(duplicate_profile_settings.profiles[0].clone());
        assert!(matches!(
            snapshot_from_current_issue(
                &issue,
                &duplicate_profile_settings,
                PROFILE_ID,
                host_limits()
            ),
            Err(NativeAuthorityError::Unavailable)
        ));
    }
}
