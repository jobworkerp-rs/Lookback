//! Versioned, private persistence for coding-agent profiles and repository policy.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::Mutex,
};

#[cfg(test)]
use std::cell::RefCell;

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{
    AgentProfile, AgentProfileSnapshot, CloneTransport, CredentialReference, ForgeKind,
    HostResourceLimits, ManagedHomeOverlay, ManagedHomeOverlayChild, PersistentMount,
    RepositoryHostConfig, RepositoryIdentity,
};

const SETTINGS_FORMAT_VERSION: u32 = 1;
const MAX_SETTINGS_BYTES: u64 = 1024 * 1024;
const MAX_PROFILES: usize = 64;
const MAX_REPOSITORY_HOSTS: usize = 128;
const MAX_PROFILE_ARGUMENTS: usize = 64;
const MAX_PROFILE_ARGUMENT_BYTES: usize = 16 * 1024;

#[cfg(test)]
struct DirectorySyncFailure {
    path: PathBuf,
    matching_call: usize,
    observed_calls: usize,
}

#[cfg(test)]
std::thread_local! {
    static DIRECTORY_SYNC_FAILURE: RefCell<Option<DirectorySyncFailure>> = const { RefCell::new(None) };
}

#[cfg(test)]
fn with_directory_sync_failure<T>(
    path: &Path,
    matching_call: usize,
    operation: impl FnOnce() -> T,
) -> T {
    assert!(matching_call > 0);
    DIRECTORY_SYNC_FAILURE.with(|failure| {
        *failure.borrow_mut() = Some(DirectorySyncFailure {
            path: path.to_path_buf(),
            matching_call,
            observed_calls: 0,
        });
    });
    struct ResetFailure;
    impl Drop for ResetFailure {
        fn drop(&mut self) {
            DIRECTORY_SYNC_FAILURE.with(|failure| *failure.borrow_mut() = None);
        }
    }
    let _reset = ResetFailure;
    operation()
}

type EffectiveHostConfiguration = (
    Vec<PathBuf>,
    Vec<PersistentMount>,
    BTreeMap<String, String>,
    Option<ManagedHomeOverlay>,
);

/// Frontend and disk representation. `revision` is advanced by the store and
/// is never trusted from an update payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingAgentSettings {
    pub format_version: u32,
    pub revision: u64,
    pub active_profile_id: Option<String>,
    #[serde(default = "semantic_judge_enabled_default")]
    pub semantic_judge_enabled: bool,
    #[serde(default)]
    pub semantic_judge_policy: super::semantic_judge::SemanticJudgePolicy,
    /// This is only a preference. A run snapshot needs a separate explicit
    /// approval before auto-DONE can be enabled.
    pub auto_done_enabled: bool,
    pub profiles: Vec<AgentProfileSettings>,
    pub repository_hosts: Vec<RepositoryHostSettings>,
    /// GUI-owned native approvals. `None` retains the legacy protected-policy
    /// repository list; `Some(empty)` is an explicit deny-all baseline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_repository_approvals: Option<Vec<NativeRepositoryApprovalSettings>>,
    /// Fingerprint of the package explicitly approved by the user. This is
    /// intentionally independent from repository approvals: changing the
    /// repository list must not silently approve a newly installed package.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_backend_package_fingerprint: Option<String>,
}

pub(crate) fn semantic_judge_enabled_default() -> bool {
    true
}

impl Default for CodingAgentSettings {
    fn default() -> Self {
        Self {
            format_version: SETTINGS_FORMAT_VERSION,
            revision: 0,
            active_profile_id: None,
            semantic_judge_enabled: true,
            semantic_judge_policy: Default::default(),
            auto_done_enabled: false,
            profiles: Vec::new(),
            repository_hosts: Vec::new(),
            native_repository_approvals: None,
            native_backend_package_fingerprint: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentProfileSettings {
    pub profile_id: String,
    pub version: u64,
    pub display_name: String,
    pub image_ref: String,
    pub allowed_image_refs: Vec<String>,
    pub command: Vec<String>,
    pub container_home: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_configuration: Option<HostAgentConfigurationSettings>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub runtime_environment: BTreeMap<String, String>,
    #[serde(default)]
    pub resources: SandboxResourcesSettings,
    #[serde(default)]
    pub network_access: super::profile::NetworkAccess,
    pub mount_allowed_roots: Vec<PathBuf>,
    pub persistent_mounts: Vec<PersistentMountSettings>,
    pub allowed_repositories: Vec<RepositoryIdentitySettings>,
    pub credential_reference: Option<String>,
}

/// Explicit host paths whose authentication state is shared with a native
/// agent. The application never scans the host home; every path is selected by
/// the user and validated before it becomes a runtime mount.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostAgentConfigurationSettings {
    #[serde(default)]
    pub codex_directory: Option<PathBuf>,
    #[serde(default)]
    pub claude_directory: Option<PathBuf>,
    #[serde(default)]
    pub claude_settings_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct SandboxResourcesSettings {
    pub cpu_millis: u32,
    pub memory_mib: u64,
    pub root_disk_mib: u64,
    pub max_parallel_runs: u32,
    /// Omitted wire fields retain the library's safe false default. The UI may
    /// explicitly set true after the user chooses a static worker.
    pub use_static: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PersistentMountSettings {
    pub host_path: PathBuf,
    pub guest_path: String,
    pub read_only: bool,
    pub executable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepositoryIdentitySettings {
    pub host: String,
    pub organization: String,
    pub repository: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepositoryHostSettings {
    pub label_host: String,
    pub forge: ForgeKind,
    pub api_base_url: String,
    pub clone_transport: CloneTransportSettings,
    pub read_credential_ref: Option<String>,
    pub publication_credential_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeRepositoryApprovalSettings {
    pub repository: RepositoryIdentitySettings,
    pub allowed_base_branches: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodingAgentProfileMountAvailability {
    pub profile_id: String,
    pub available: bool,
    pub blockers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum CloneTransportSettings {
    Https {
        base_url: String,
    },
    Ssh {
        host: String,
        user: String,
        port: Option<u16>,
        #[serde(default)]
        ssh_directory: Option<PathBuf>,
    },
}

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("coding-agent settings could not be read or written")]
    Io(#[from] std::io::Error),
    #[error("coding-agent settings contain invalid or unsupported data")]
    Invalid(String),
    #[error(
        "coding-agent settings changed in another editor (expected {expected}, actual {actual})"
    )]
    RevisionConflict { expected: u64, actual: u64 },
    #[error("profile is in use by an unresolved run: {0}")]
    ProfileInUse(String),
}

/// Private settings file with an on-disk lock as well as an in-process lock.
/// The disk lock makes compare-and-swap meaningful across app instances.
pub struct CodingAgentSettingsStore {
    path: PathBuf,
    process_lock: Mutex<()>,
}

struct ClaudeSettingsImportRollback {
    destination: PathBuf,
    previous: Option<Vec<u8>>,
}

impl CodingAgentSettingsStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            process_lock: Mutex::new(()),
        }
    }

    pub(crate) fn managed_profile_home_root(&self) -> Result<PathBuf, SettingsError> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| SettingsError::Invalid("settings path has no parent".into()))?;
        Ok(fs::canonicalize(parent)?.join("profile-homes"))
    }

    pub(crate) fn managed_shared_home_root(
        &self,
        profile_id: &str,
    ) -> Result<PathBuf, SettingsError> {
        let profile_homes = self.managed_profile_home_root()?;
        let parent = profile_homes.parent().ok_or_else(|| {
            SettingsError::Invalid("profile home root has no trusted parent".into())
        })?;
        Ok(parent
            .join("profile-shared-homes")
            .join(super::assessment::sha256_hex(profile_id.as_bytes())))
    }

    pub fn load(&self) -> Result<CodingAgentSettings, SettingsError> {
        let _process_guard = self
            .process_lock
            .lock()
            .map_err(|_| SettingsError::Invalid("settings lock is unavailable".into()))?;
        let _file_guard = self.acquire_file_lock()?;
        self.load_locked()
    }

    /// Compare-and-swap one complete settings snapshot. Profile records that
    /// change are versioned here, not by renderer-supplied version numbers.
    pub fn compare_and_swap(
        &self,
        expected_revision: u64,
        proposed: CodingAgentSettings,
        unresolved_profile_ids: &HashSet<String>,
    ) -> Result<CodingAgentSettings, SettingsError> {
        let _process_guard = self
            .process_lock
            .lock()
            .map_err(|_| SettingsError::Invalid("settings lock is unavailable".into()))?;
        let _file_guard = self.acquire_file_lock()?;
        let current = self.load_locked()?;
        let proposed = prepare_settings_update(
            &current,
            expected_revision,
            proposed,
            unresolved_profile_ids,
        )?;
        let (rollback, import_durability_warning) =
            self.import_claude_settings(&current, &proposed)?;
        let settings_durability_warning = match self.write_atomic(&proposed) {
            Ok(warning) => warning,
            Err(error) => {
                rollback_claude_settings(&rollback);
                return Err(error);
            }
        };
        if let Some(error) = settings_durability_warning.or(import_durability_warning) {
            // Both logical commits are already visible. Keep the files aligned
            // and report only the post-commit durability warning to the caller.
            return Err(error);
        }
        Ok(proposed)
    }

    /// Persist the explicit GUI baseline approval. Normal settings saves must
    /// not turn a package observation into backend authority implicitly.
    pub fn compare_and_swap_native_baseline(
        &self,
        expected_revision: u64,
        package_fingerprint: String,
    ) -> Result<CodingAgentSettings, SettingsError> {
        let _process_guard = self
            .process_lock
            .lock()
            .map_err(|_| SettingsError::Invalid("settings lock is unavailable".into()))?;
        let _file_guard = self.acquire_file_lock()?;
        let current = self.load_locked()?;
        let mut proposed = current.clone();
        proposed.native_backend_package_fingerprint = Some(package_fingerprint);
        let proposed = prepare_settings_update_inner(
            &current,
            expected_revision,
            proposed,
            &HashSet::new(),
            true,
        )?;
        if let Some(error) = self.write_atomic(&proposed)? {
            return Err(error);
        }
        Ok(proposed)
    }

    /// Provision an explicitly selected profile home without replacing user mounts.
    pub fn prepare_profile_home(
        &self,
        expected_revision: u64,
        profile_id: &str,
        unresolved_profile_ids: &HashSet<String>,
    ) -> Result<CodingAgentSettings, SettingsError> {
        let _process_guard = self
            .process_lock
            .lock()
            .map_err(|_| SettingsError::Invalid("settings lock is unavailable".into()))?;
        let _file_guard = self.acquire_file_lock()?;
        let current = self.load_locked()?;
        if current.revision != expected_revision {
            return Err(SettingsError::RevisionConflict {
                expected: expected_revision,
                actual: current.revision,
            });
        }
        let index = current
            .profiles
            .iter()
            .position(|profile| profile.profile_id == profile_id)
            .ok_or_else(|| SettingsError::Invalid("profile is not registered".into()))?;
        if unresolved_profile_ids.contains(profile_id) {
            return Err(SettingsError::ProfileInUse(profile_id.into()));
        }
        let parent = self
            .path
            .parent()
            .ok_or_else(|| SettingsError::Invalid("settings path has no parent".into()))?;
        let parent = fs::canonicalize(parent)?;
        let homes = parent.join("profile-homes");
        let home = homes.join(super::assessment::sha256_hex(profile_id.as_bytes()));
        let profile = &current.profiles[index];
        if profile.host_configuration.is_some() {
            return Err(SettingsError::Invalid(
                "profile home preparation is unavailable for host-shared profiles".into(),
            ));
        }
        // Agents install and run their own tools from home (the Codex daemon
        // runs from CODEX_HOME), and only the sandbox VM executes from it.
        let mut desired = PersistentMountSettings {
            host_path: home.clone(),
            guest_path: profile.container_home.clone(),
            read_only: false,
            executable: true,
        };
        if let Some(existing) = profile.persistent_mounts.iter().find(|mount| {
            mount.host_path == home
                && mount.guest_path == profile.container_home
                && !mount.read_only
        }) {
            desired.executable = existing.executable;
        }
        let guest_home = Path::new(&profile.container_home);
        if profile.persistent_mounts.iter().any(|mount| {
            let guest = Path::new(&mount.guest_path);
            (guest.starts_with(guest_home) || guest_home.starts_with(guest)) && mount != &desired
        }) {
            return Err(SettingsError::Invalid(
                "profile home conflicts with a selected mount".into(),
            ));
        }
        let mut proposed = current.clone();
        let target = &mut proposed.profiles[index];
        if !target.persistent_mounts.contains(&desired) {
            target.persistent_mounts.push(desired);
        }
        if !target.mount_allowed_roots.contains(&home) {
            target.mount_allowed_roots.push(home.clone());
        }
        let unchanged = proposed == current;
        let proposed = if unchanged {
            proposed
        } else {
            prepare_settings_update(
                &current,
                expected_revision,
                proposed,
                unresolved_profile_ids,
            )?
        };
        ensure_owned_private_directory(&homes)?;
        ensure_owned_private_directory(&home)?;
        if unchanged {
            return Ok(current);
        }
        if let Some(error) = self.write_atomic(&proposed)? {
            return Err(error);
        }
        Ok(proposed)
    }

    fn load_locked(&self) -> Result<CodingAgentSettings, SettingsError> {
        let metadata = match fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(CodingAgentSettings::default());
            }
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() > MAX_SETTINGS_BYTES
        {
            return Err(SettingsError::Invalid(
                "settings file must be a bounded regular file".into(),
            ));
        }
        let mut bytes = Vec::new();
        open_checked_file(&self.path)?
            .take(MAX_SETTINGS_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_SETTINGS_BYTES {
            return Err(SettingsError::Invalid(
                "settings file must be a bounded regular file".into(),
            ));
        }
        let settings: CodingAgentSettings = serde_json::from_slice(&bytes)
            .map_err(|_| SettingsError::Invalid("settings JSON is malformed".into()))?;
        if settings.format_version != SETTINGS_FORMAT_VERSION {
            return Err(SettingsError::Invalid(
                "settings format version is unsupported".into(),
            ));
        }
        validate_settings(&settings, SettingsValidationMode::Structural)?;
        Ok(settings)
    }

    fn acquire_file_lock(&self) -> Result<File, SettingsError> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| SettingsError::Invalid("settings path has no parent".into()))?;
        create_private_directory(parent)?;
        let lock_path = self.path.with_extension("json.lock");
        let mut options = OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&lock_path)?;
        verify_opened_path(&file, &lock_path)?;
        set_private_file(&lock_path)?;
        file.lock_exclusive()?;
        Ok(file)
    }

    fn write_atomic(
        &self,
        settings: &CodingAgentSettings,
    ) -> Result<Option<SettingsError>, SettingsError> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| SettingsError::Invalid("settings path has no parent".into()))?;
        create_private_directory(parent)?;
        if fs::symlink_metadata(&self.path).is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(SettingsError::Invalid("settings file is a symlink".into()));
        }
        let mut random = [0u8; 12];
        getrandom::fill(&mut random)
            .map_err(|_| SettingsError::Invalid("temporary file name is unavailable".into()))?;
        let suffix = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let temporary = parent.join(format!(".coding-agent-settings-{suffix}.tmp"));
        let bytes = serde_json::to_vec_pretty(settings)
            .map_err(|_| SettingsError::Invalid("settings could not be serialized".into()))?;
        if bytes.len() as u64 > MAX_SETTINGS_BYTES {
            return Err(SettingsError::Invalid(
                "settings exceed the configured size limit".into(),
            ));
        }
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        if let Err(error) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
            let _ = fs::remove_file(&temporary);
            return Err(error.into());
        }
        if let Err(error) = set_private_file(&temporary) {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        drop(file);
        if let Err(error) = fs::rename(&temporary, &self.path) {
            let _ = fs::remove_file(&temporary);
            return Err(error.into());
        }
        // The rename is the logical commit. A later directory-sync failure
        // must not trigger rollback of an import aligned with this snapshot.
        Ok(sync_directory(parent).err())
    }

    fn import_claude_settings(
        &self,
        current: &CodingAgentSettings,
        proposed: &CodingAgentSettings,
    ) -> Result<(Vec<ClaudeSettingsImportRollback>, Option<SettingsError>), SettingsError> {
        let mut rollback = Vec::new();
        let mut durability_warning = None;
        for profile in &proposed.profiles {
            let Some(configuration) = &profile.host_configuration else {
                continue;
            };
            let shared_home = match self.managed_shared_home_root(&profile.profile_id) {
                Ok(path) => path,
                Err(error) => {
                    rollback_claude_settings(&rollback);
                    return Err(error);
                }
            };
            let shared_parent = match shared_home.parent() {
                Some(path) => path,
                None => {
                    rollback_claude_settings(&rollback);
                    return Err(SettingsError::Invalid("shared home has no parent".into()));
                }
            };
            if let Err(error) = ensure_owned_private_directory(shared_parent)
                .and_then(|()| ensure_owned_private_directory(&shared_home))
            {
                rollback_claude_settings(&rollback);
                return Err(error);
            }
            let destination = shared_home.join(".claude.json");
            let previous_source = current
                .profiles
                .iter()
                .find(|previous| previous.profile_id == profile.profile_id)
                .and_then(|previous| previous.host_configuration.as_ref())
                .and_then(|previous| previous.claude_settings_file.as_ref());
            let source_changed = previous_source != configuration.claude_settings_file.as_ref();
            let destination_exists = fs::symlink_metadata(&destination).is_ok();
            let Some(source) = configuration.claude_settings_file.as_deref() else {
                continue;
            };
            if destination_exists && !source_changed {
                if let Err(error) = canonical_private_host_path(&destination, false) {
                    rollback_claude_settings(&rollback);
                    return Err(error);
                }
                continue;
            }
            let source = match canonical_selected_host_file(source) {
                Ok(path) => path,
                Err(error) => {
                    rollback_claude_settings(&rollback);
                    return Err(error);
                }
            };
            let previous = if destination_exists {
                match read_private_import(&destination) {
                    Ok(bytes) => Some(bytes),
                    Err(error) => {
                        rollback_claude_settings(&rollback);
                        return Err(error);
                    }
                }
            } else {
                None
            };
            rollback.push(ClaudeSettingsImportRollback {
                destination: destination.clone(),
                previous,
            });
            match write_private_import(&source, &destination) {
                Ok(Some(error)) => {
                    durability_warning.get_or_insert(error);
                }
                Ok(None) => {}
                Err(error) => {
                    rollback_claude_settings(&rollback);
                    return Err(error);
                }
            }
        }
        Ok((rollback, durability_warning))
    }
}

fn read_private_import(path: &Path) -> Result<Vec<u8>, SettingsError> {
    canonical_private_host_path(path, false)?;
    let mut file = open_checked_file(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn write_private_import(
    source: &Path,
    destination: &Path,
) -> Result<Option<SettingsError>, SettingsError> {
    let mut random = [0u8; 12];
    getrandom::fill(&mut random)
        .map_err(|_| SettingsError::Invalid("temporary file name is unavailable".into()))?;
    let suffix = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let temporary = destination.with_file_name(format!(".claude-json-{suffix}.tmp"));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut target = options.open(&temporary)?;
    let mut source_file = open_checked_file(source)?;
    if let Err(error) = std::io::copy(&mut source_file, &mut target).and_then(|_| target.sync_all())
    {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    if let Err(error) = set_private_file(&temporary) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    drop(target);
    if let Err(error) = fs::rename(&temporary, destination) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    // The settings CAS and this import are protected by the same lock, but a
    // process crash between the two renames can still leave one durable file
    // from the new state. Full cross-file atomicity is intentionally not
    // claimed; the next CAS repairs the destination from its source.
    Ok(sync_directory(
        destination
            .parent()
            .ok_or_else(|| SettingsError::Invalid("import destination has no parent".into()))?,
    )
    .err())
}

fn rollback_claude_settings(rollback: &[ClaudeSettingsImportRollback]) {
    for item in rollback.iter().rev() {
        match &item.previous {
            Some(bytes) => {
                let _ = write_private_bytes(&item.destination, bytes);
            }
            None => {
                let _ = fs::remove_file(&item.destination);
            }
        }
    }
}

fn write_private_bytes(destination: &Path, bytes: &[u8]) -> Result<(), SettingsError> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(destination)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    set_private_file(destination)?;
    Ok(())
}

fn prepare_settings_update(
    current: &CodingAgentSettings,
    expected_revision: u64,
    proposed: CodingAgentSettings,
    unresolved_profile_ids: &HashSet<String>,
) -> Result<CodingAgentSettings, SettingsError> {
    prepare_settings_update_inner(
        current,
        expected_revision,
        proposed,
        unresolved_profile_ids,
        false,
    )
}

fn prepare_settings_update_inner(
    current: &CodingAgentSettings,
    expected_revision: u64,
    mut proposed: CodingAgentSettings,
    unresolved_profile_ids: &HashSet<String>,
    allow_native_baseline: bool,
) -> Result<CodingAgentSettings, SettingsError> {
    if current.revision != expected_revision {
        return Err(SettingsError::RevisionConflict {
            expected: expected_revision,
            actual: current.revision,
        });
    }
    proposed.format_version = SETTINGS_FORMAT_VERSION;
    proposed.revision = expected_revision
        .checked_add(1)
        .ok_or_else(|| SettingsError::Invalid("settings revision is exhausted".into()))?;
    if !allow_native_baseline {
        if proposed.native_backend_package_fingerprint.is_none()
            && current.native_backend_package_fingerprint.is_some()
        {
            // Older renderer payloads omit the internal marker; preserve it
            // during ordinary settings edits instead of revoking authority.
            proposed.native_backend_package_fingerprint =
                current.native_backend_package_fingerprint.clone();
        }
        if proposed.native_backend_package_fingerprint != current.native_backend_package_fingerprint
        {
            return Err(SettingsError::Invalid(
                "native backend package approval requires an explicit GUI action".into(),
            ));
        }
    }
    normalize_profile_versions(current, &mut proposed)?;
    validate_settings(&proposed, SettingsValidationMode::Structural)?;
    validate_changed_host_configuration_directories(current, &proposed)?;
    if !allow_native_baseline {
        validate_changed_ssh_directories(current, &proposed)?;
    }
    for profile in &proposed.profiles {
        profile
            .to_agent_profile_for_mode(SettingsValidationMode::Structural)?
            .resources
            .validate_cpu_allocation()
            .map_err(|_| {
                SettingsError::Invalid("SANDBOX CPU allocation must use whole virtual CPUs".into())
            })?;
    }
    reject_changes_to_unresolved_profiles(current, &proposed, unresolved_profile_ids)?;
    Ok(proposed)
}

fn validate_changed_ssh_directories(
    current: &CodingAgentSettings,
    proposed: &CodingAgentSettings,
) -> Result<(), SettingsError> {
    for host in &proposed.repository_hosts {
        let proposed_path = match &host.clone_transport {
            CloneTransportSettings::Ssh { ssh_directory, .. } => ssh_directory.as_deref(),
            CloneTransportSettings::Https { .. } => None,
        };
        let previous_path = current
            .repository_hosts
            .iter()
            .find(|previous| previous.label_host == host.label_host)
            .and_then(|previous| match &previous.clone_transport {
                CloneTransportSettings::Ssh { ssh_directory, .. } => ssh_directory.as_deref(),
                CloneTransportSettings::Https { .. } => None,
            });
        if proposed_path != previous_path
            && let Some(path) = proposed_path
        {
            validate_ssh_directory(path)?;
        }
    }
    Ok(())
}

fn validate_changed_host_configuration_directories(
    current: &CodingAgentSettings,
    proposed: &CodingAgentSettings,
) -> Result<(), SettingsError> {
    for profile in &proposed.profiles {
        let previous = current
            .profiles
            .iter()
            .find(|previous| previous.profile_id == profile.profile_id)
            .and_then(|previous| previous.host_configuration.as_ref());
        let Some(configuration) = profile.host_configuration.as_ref() else {
            continue;
        };
        let previous_codex =
            previous.and_then(|configuration| configuration.codex_directory.as_ref());
        if configuration.codex_directory.as_ref() != previous_codex
            && let Some(path) = configuration.codex_directory.as_deref()
        {
            canonical_selected_host_directory(path)?;
        }
        let previous_claude =
            previous.and_then(|configuration| configuration.claude_directory.as_ref());
        if configuration.claude_directory.as_ref() != previous_claude
            && let Some(path) = configuration.claude_directory.as_deref()
        {
            canonical_selected_host_directory(path)?;
        }
    }
    Ok(())
}

pub(crate) fn ensure_owned_private_directory(path: &Path) -> Result<(), SettingsError> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => (),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(error) => return Err(error.into()),
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(SettingsError::Invalid(
            "profile home must be a non-symlink directory".into(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // Existing user data is never chmodded or adopted from another owner.
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o777 != 0o700 {
            return Err(SettingsError::Invalid(
                "profile home must be owned by the current user and private".into(),
            ));
        }
    }
    Ok(())
}

impl CodingAgentSettings {
    pub fn repository_hosts_core(&self) -> Result<Vec<RepositoryHostConfig>, SettingsError> {
        self.repository_hosts
            .iter()
            .map(RepositoryHostSettings::to_repository_host_config_for_execution)
            .collect()
    }

    pub fn profile_mount_availability(&self) -> Vec<CodingAgentProfileMountAvailability> {
        self.profiles
            .iter()
            .map(|profile| profile_mount_availability(profile, None))
            .collect()
    }

    pub fn profile_mount_availability_with_managed_home_root(
        &self,
        managed_home_root: &Path,
    ) -> Vec<CodingAgentProfileMountAvailability> {
        self.profiles
            .iter()
            .map(|profile| profile_mount_availability(profile, Some(managed_home_root)))
            .collect()
    }
}

fn normalize_profile_versions(
    current: &CodingAgentSettings,
    proposed: &mut CodingAgentSettings,
) -> Result<(), SettingsError> {
    let current_by_id = current
        .profiles
        .iter()
        .map(|profile| (profile.profile_id.as_str(), profile))
        .collect::<std::collections::HashMap<_, _>>();
    for profile in &mut proposed.profiles {
        if let Some(previous) = current_by_id.get(profile.profile_id.as_str()) {
            let previous_version = previous.version;
            profile.version = previous_version;
            if *previous != profile {
                profile.version = previous_version
                    .checked_add(1)
                    .ok_or_else(|| SettingsError::Invalid("profile version is exhausted".into()))?;
            }
        } else {
            profile.version = 1;
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SettingsValidationMode {
    Structural,
    Execution,
}

fn validate_settings(
    settings: &CodingAgentSettings,
    validation_mode: SettingsValidationMode,
) -> Result<(), SettingsError> {
    settings.semantic_judge_policy.validate().map_err(|_| {
        SettingsError::Invalid("semantic retry count or deadline is invalid".into())
    })?;
    if !settings.semantic_judge_enabled && settings.auto_done_enabled {
        return Err(SettingsError::Invalid(
            "automatic completion requires semantic assessment".into(),
        ));
    }
    if settings.format_version != SETTINGS_FORMAT_VERSION
        || settings.profiles.len() > MAX_PROFILES
        || settings.repository_hosts.len() > MAX_REPOSITORY_HOSTS
    {
        return Err(SettingsError::Invalid(
            "settings version or collection size is invalid".into(),
        ));
    }
    let mut hosts = HashSet::new();
    for host in &settings.repository_hosts {
        let config = match validation_mode {
            SettingsValidationMode::Structural => host.to_repository_host_config()?,
            SettingsValidationMode::Execution => host.to_repository_host_config_for_execution()?,
        };
        config
            .validate()
            .map_err(|_| SettingsError::Invalid("repository host policy is invalid".into()))?;
        if !hosts.insert(config.label_host.clone()) {
            return Err(SettingsError::Invalid(
                "repository host configuration contains duplicates".into(),
            ));
        }
    }
    let host_names = hosts;
    let mut profiles = HashSet::new();
    for profile in &settings.profiles {
        if !profiles.insert(profile.profile_id.as_str())
            || profile.command.len() > MAX_PROFILE_ARGUMENTS
            || profile.command.iter().map(String::len).sum::<usize>() > MAX_PROFILE_ARGUMENT_BYTES
            || profile.allowed_image_refs.len() > MAX_PROFILES
            || profile.mount_allowed_roots.len() > 32
            || profile.persistent_mounts.len() > 64
            || profile.allowed_repositories.len() > 512
        {
            return Err(SettingsError::Invalid(
                "profile identity or field bounds are invalid".into(),
            ));
        }
        let core = profile.to_agent_profile_for_mode(validation_mode)?;
        validate_profile_structure(&core)?;
        for repository in &profile.allowed_repositories {
            let identity = repository.to_repository_identity()?;
            if !host_names.contains(&identity.host) {
                return Err(SettingsError::Invalid(
                    "profile repository host is not configured".into(),
                ));
            }
        }
    }
    if let Some(approvals) = &settings.native_repository_approvals {
        validate_native_repository_approval_list(approvals, &host_names)?;
    }
    if let Some(fingerprint) = &settings.native_backend_package_fingerprint
        && (fingerprint.len() != 64 || !fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return Err(SettingsError::Invalid(
            "native backend package fingerprint is invalid".into(),
        ));
    }
    if settings
        .active_profile_id
        .as_deref()
        .is_some_and(|profile_id| !profiles.contains(profile_id))
    {
        return Err(SettingsError::Invalid(
            "active profile is not present in the profile registry".into(),
        ));
    }
    Ok(())
}

fn validate_native_repository_approval_list(
    approvals: &[NativeRepositoryApprovalSettings],
    configured_hosts: &HashSet<String>,
) -> Result<(), SettingsError> {
    if approvals.len() > 512 {
        return Err(SettingsError::Invalid(
            "native repository approvals exceed the configured size limit".into(),
        ));
    }
    let mut repositories = BTreeSet::new();
    for approval in approvals {
        let repository = approval.repository.to_repository_identity()?;
        if !configured_hosts.contains(&repository.host) {
            return Err(SettingsError::Invalid(
                "native repository approval must reference its configured host".into(),
            ));
        }
        if !repositories.insert(repository) {
            return Err(SettingsError::Invalid(
                "native repository approvals contain duplicates".into(),
            ));
        }
        if approval.allowed_base_branches.is_empty()
            || approval.allowed_base_branches.len() > 128
            || approval
                .allowed_base_branches
                .iter()
                .any(|branch| !super::is_valid_branch_name(branch))
        {
            return Err(SettingsError::Invalid(
                "native repository approval branches are invalid".into(),
            ));
        }
        let mut branches = BTreeSet::new();
        if approval
            .allowed_base_branches
            .iter()
            .any(|branch| !branches.insert(branch))
        {
            return Err(SettingsError::Invalid(
                "native repository approval branches contain duplicates".into(),
            ));
        }
    }
    Ok(())
}

fn validate_profile_structure(profile: &AgentProfile) -> Result<(), SettingsError> {
    super::profile::validate_runtime_environment(&profile.runtime_environment)
        .map_err(|_| SettingsError::Invalid("agent runtime environment is invalid".into()))?;
    let allowed_images = profile
        .allowed_image_refs
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if allowed_images.len() != profile.allowed_image_refs.len()
        || !allowed_images.contains(profile.image_ref.as_str())
    {
        return Err(SettingsError::Invalid(
            "agent image allowlist is invalid".into(),
        ));
    }
    if profile
        .mount_allowed_roots
        .iter()
        .collect::<BTreeSet<_>>()
        .len()
        != profile.mount_allowed_roots.len()
    {
        return Err(SettingsError::Invalid(
            "allowed mount roots contain duplicates".into(),
        ));
    }
    // This temporary snapshot checks structural constraints only. Settings
    // reads never canonicalize or require user-configured paths to exist.
    let structural_limits = HostResourceLimits {
        cpu_millis: u32::MAX,
        memory_mib: u64::MAX,
        root_disk_mib: u64::MAX,
        max_parallel_runs: u32::MAX,
    };
    for image_ref in &profile.allowed_image_refs {
        if !valid_image_reference(image_ref) {
            return Err(SettingsError::Invalid(
                "agent image reference is invalid".into(),
            ));
        }
        let image_digest = image_ref
            .rsplit_once('@')
            .map(|(_, digest)| digest.to_owned())
            .unwrap_or_default();
        let snapshot = AgentProfileSnapshot {
            profile_id: profile.profile_id.clone(),
            version: profile.version,
            display_name: profile.display_name.clone(),
            image_ref: image_ref.clone(),
            image_digest,
            command: profile.command.clone(),
            container_home: profile.container_home.clone(),
            managed_home_overlay: None,
            runtime_environment: profile.runtime_environment.clone(),
            resources: profile.resources,
            network_access: profile.network_access,
            validated_host_limits: structural_limits,
            canonical_mount_allowed_roots: profile.mount_allowed_roots.clone(),
            persistent_mounts: profile.persistent_mounts.clone(),
            allowed_repositories: profile.allowed_repositories.clone(),
            credential_reference: profile.credential_reference.clone(),
        };
        snapshot
            .validate_structure()
            .map_err(|_| SettingsError::Invalid("agent profile is structurally invalid".into()))?;
    }
    Ok(())
}

pub(crate) fn valid_image_reference(reference: &str) -> bool {
    let (name, digest) = reference
        .split_once('@')
        .map_or((reference, None), |(name, digest)| (name, Some(digest)));
    !name.is_empty()
        && name.len() <= 1024
        && !name.contains("://")
        && !name
            .split('/')
            .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._/:-".contains(&byte))
        && digest.is_none_or(|digest| {
            digest.strip_prefix("sha256:").is_some_and(|hash| {
                hash.len() == 64
                    && hash
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
        })
}

fn profile_mount_availability(
    profile: &AgentProfileSettings,
    managed_home_root: Option<&Path>,
) -> CodingAgentProfileMountAvailability {
    let mut blockers = BTreeSet::new();
    if profile.host_configuration.is_some()
        && match managed_home_root {
            Some(root) => profile
                .to_agent_profile_with_managed_home_root(root)
                .is_err(),
            None => profile.to_agent_profile().is_err(),
        }
    {
        blockers.insert("host_shared_configuration_invalid".to_owned());
    }
    if profile.host_configuration.is_some()
        && managed_home_root.is_some_and(|root| {
            shared_profile_home_root(root, &profile.profile_id)
                .and_then(|shared| canonical_private_host_path(&shared, true).map(|_| ()))
                .is_err()
        })
    {
        blockers.insert("shared_home_unavailable".to_owned());
    }
    let mut roots = Vec::new();
    for root in &profile.mount_allowed_roots {
        if profile.host_configuration.is_some()
            && managed_home_root.is_some_and(|home_root| {
                root == &home_root
                    .join(super::assessment::sha256_hex(profile.profile_id.as_bytes()))
            })
        {
            continue;
        }
        match fs::canonicalize(root) {
            Ok(path) if path.is_dir() => roots.push(path),
            _ => {
                blockers.insert("mount_root_unavailable".to_owned());
            }
        }
    }
    if roots.iter().collect::<BTreeSet<_>>().len() != roots.len() {
        blockers.insert("mount_roots_resolve_to_same_path".to_owned());
    }
    let mut mounts = BTreeSet::new();
    for mount in &profile.persistent_mounts {
        if profile.host_configuration.is_some()
            && mount.guest_path == profile.container_home
            && managed_home_root
                .is_some_and(|home_root| is_managed_profile_home_mount(profile, mount, home_root))
        {
            continue;
        }
        match fs::canonicalize(&mount.host_path) {
            Ok(path)
                if (path.is_dir() || path.is_file())
                    && roots.iter().any(|root| path.starts_with(root)) =>
            {
                if mounts.iter().any(|existing: &PathBuf| {
                    existing.starts_with(&path) || path.starts_with(existing)
                }) || !mounts.insert(path)
                {
                    blockers.insert("mount_paths_resolve_to_overlapping_paths".to_owned());
                }
            }
            Ok(_) => {
                blockers.insert("persistent_mount_outside_available_root".to_owned());
            }
            Err(_) => {
                blockers.insert("persistent_mount_unavailable".to_owned());
            }
        }
    }
    CodingAgentProfileMountAvailability {
        profile_id: profile.profile_id.clone(),
        available: blockers.is_empty(),
        blockers: blockers.into_iter().collect(),
    }
}

fn reject_changes_to_unresolved_profiles(
    current: &CodingAgentSettings,
    proposed: &CodingAgentSettings,
    unresolved_profile_ids: &HashSet<String>,
) -> Result<(), SettingsError> {
    let current_profiles = current
        .profiles
        .iter()
        .map(|profile| (profile.profile_id.as_str(), profile))
        .collect::<std::collections::HashMap<_, _>>();
    let proposed_profiles = proposed
        .profiles
        .iter()
        .map(|profile| (profile.profile_id.as_str(), profile))
        .collect::<std::collections::HashMap<_, _>>();
    for profile_id in unresolved_profile_ids {
        let old_profile = current_profiles.get(profile_id.as_str());
        let new_profile = proposed_profiles.get(profile_id.as_str());
        let profile_changed = match (old_profile, new_profile) {
            (Some(old), Some(new)) => {
                let mut old = (*old).clone();
                let mut new = (*new).clone();
                old.version = 0;
                new.version = 0;
                old != new
            }
            _ => true,
        };
        if profile_changed || repositories_use_changed_host(current, proposed, old_profile) {
            return Err(SettingsError::ProfileInUse(profile_id.clone()));
        }
    }
    Ok(())
}

fn repositories_use_changed_host(
    current: &CodingAgentSettings,
    proposed: &CodingAgentSettings,
    profile: Option<&&AgentProfileSettings>,
) -> bool {
    let Some(profile) = profile else {
        return false;
    };
    profile.allowed_repositories.iter().any(|repository| {
        let host = repository.host.as_str();
        current
            .repository_hosts
            .iter()
            .find(|config| config.label_host == host)
            != proposed
                .repository_hosts
                .iter()
                .find(|config| config.label_host == host)
    })
}

impl AgentProfileSettings {
    pub fn to_agent_profile(&self) -> Result<AgentProfile, SettingsError> {
        self.to_agent_profile_with_mode(SettingsValidationMode::Execution, None)
    }

    pub(crate) fn to_agent_profile_for_mode(
        &self,
        validation_mode: SettingsValidationMode,
    ) -> Result<AgentProfile, SettingsError> {
        self.to_agent_profile_with_mode(validation_mode, None)
    }

    /// Build an execution profile while proving the app-managed home against
    /// the settings store's exact `profile-homes` directory.
    pub fn to_agent_profile_with_managed_home_root(
        &self,
        managed_home_root: &Path,
    ) -> Result<AgentProfile, SettingsError> {
        self.to_agent_profile_with_mode(SettingsValidationMode::Execution, Some(managed_home_root))
    }

    fn to_agent_profile_with_mode(
        &self,
        validation_mode: SettingsValidationMode,
        managed_home_root: Option<&Path>,
    ) -> Result<AgentProfile, SettingsError> {
        let allowed_repositories = self
            .allowed_repositories
            .iter()
            .map(RepositoryIdentitySettings::to_repository_identity)
            .collect::<Result<Vec<_>, _>>()?;
        let credential_reference = self
            .credential_reference
            .as_deref()
            .map(CredentialReference::new)
            .transpose()
            .map_err(|_| SettingsError::Invalid("credential reference is invalid".into()))?;
        let (mount_allowed_roots, persistent_mounts, runtime_environment, managed_home_overlay) =
            self.effective_host_configuration(validation_mode, managed_home_root)?;
        Ok(AgentProfile {
            profile_id: self.profile_id.clone(),
            version: self.version,
            display_name: self.display_name.clone(),
            image_ref: self.image_ref.clone(),
            allowed_image_refs: self.allowed_image_refs.clone(),
            command: self.command.clone(),
            container_home: self.container_home.clone(),
            managed_home_overlay,
            runtime_environment,
            resources: self.resources.into(),
            network_access: self.network_access,
            mount_allowed_roots,
            persistent_mounts,
            allowed_repositories,
            credential_reference,
        })
    }

    fn effective_host_configuration(
        &self,
        validation_mode: SettingsValidationMode,
        managed_home_root: Option<&Path>,
    ) -> Result<EffectiveHostConfiguration, SettingsError> {
        let Some(configuration) = &self.host_configuration else {
            return Ok((
                self.mount_allowed_roots.clone(),
                self.persistent_mounts
                    .iter()
                    .map(PersistentMountSettings::to_core)
                    .collect(),
                self.runtime_environment.clone(),
                None,
            ));
        };
        if validation_mode == SettingsValidationMode::Execution && managed_home_root.is_none() {
            return Err(SettingsError::Invalid(
                "host-shared profile requires the exact settings store root".into(),
            ));
        }
        let mut roots = self.mount_allowed_roots.clone();
        let fixed_roots = roots.clone();
        let mut mounts = Vec::with_capacity(self.persistent_mounts.len() + 3);
        for mount in &self.persistent_mounts {
            if mount.guest_path == self.container_home {
                if managed_home_root
                    .is_some_and(|root| is_managed_profile_home_mount(self, mount, root))
                    || (managed_home_root.is_none()
                        && validation_mode == SettingsValidationMode::Structural
                        && is_managed_profile_home_path(self, &mount.host_path))
                {
                    roots.retain(|root| root != &mount.host_path);
                    continue;
                }
                return Err(SettingsError::Invalid(
                    "host-shared profile cannot mount the complete container home".into(),
                ));
            }
            if fixed_roots.is_empty()
                || !fixed_roots
                    .iter()
                    .any(|root| mount.host_path.starts_with(root))
            {
                return Err(SettingsError::Invalid(
                    "persistent mount is outside an allowed mount root".into(),
                ));
            }
            mounts.push(mount.to_core());
        }
        let mut runtime_environment = self.runtime_environment.clone();
        let shared_home = managed_home_root
            .map(|root| shared_profile_home_root(root, &self.profile_id))
            .transpose()?
            .map(|path| path.to_path_buf());
        if validation_mode == SettingsValidationMode::Execution
            && let Some(source) = configuration.claude_settings_file.as_deref()
            && shared_home.as_ref().is_none_or(|home| {
                canonical_private_host_path(&home.join(".claude.json"), false).is_err()
            })
        {
            canonical_selected_host_file(source)?;
        }
        let mappings = [
            (
                configuration.codex_directory.as_ref(),
                ManagedHomeOverlayChild::CodexDirectory {
                    host_path: PathBuf::new(),
                },
            ),
            (
                configuration.claude_directory.as_ref(),
                ManagedHomeOverlayChild::ClaudeDirectory {
                    host_path: PathBuf::new(),
                },
            ),
        ];
        let mut children = Vec::new();
        for (path, mut child) in mappings {
            let Some(path) = path else {
                continue;
            };
            let canonical = match validation_mode {
                SettingsValidationMode::Structural => structural_host_path(path)?,
                SettingsValidationMode::Execution => canonical_selected_host_directory(path)?,
            };
            match &mut child {
                ManagedHomeOverlayChild::CodexDirectory { host_path }
                | ManagedHomeOverlayChild::ClaudeDirectory { host_path } => {
                    *host_path = canonical.clone()
                }
            }
            let guest_path = child.guest_path(&self.container_home);
            if mounts.iter().any(|mount: &PersistentMount| {
                mount.host_path.starts_with(&canonical)
                    || canonical.starts_with(&mount.host_path)
                    || guest_paths_overlap(&mount.guest_path, &guest_path)
            }) {
                return Err(SettingsError::Invalid(
                    "host agent path overlaps another persistent mount".into(),
                ));
            }
            if children.iter().any(|existing: &ManagedHomeOverlayChild| {
                existing.host_path().starts_with(&canonical)
                    || canonical.starts_with(existing.host_path())
                    || guest_paths_overlap(&existing.guest_path(&self.container_home), &guest_path)
            }) {
                return Err(SettingsError::Invalid(
                    "host agent paths overlap one another".into(),
                ));
            }
            let root = canonical.clone();
            if !roots.iter().any(|existing| existing == &root) {
                roots.push(root);
            }
            if matches!(child, ManagedHomeOverlayChild::CodexDirectory { .. }) {
                runtime_environment.insert("CODEX_HOME".into(), guest_path.clone());
            }
            children.push(child);
        }
        let Some(shared_home) = shared_home else {
            return Ok((roots, mounts, runtime_environment, None));
        };
        if !roots.iter().any(|root| root == &shared_home) {
            roots.push(shared_home.clone());
        }
        Ok((
            roots,
            mounts,
            runtime_environment,
            Some(ManagedHomeOverlay {
                parent: PersistentMount {
                    host_path: shared_home,
                    guest_path: self.container_home.clone(),
                    read_only: false,
                    executable: false,
                },
                children,
            }),
        ))
    }
}

fn shared_profile_home_root(
    managed_home_root: &Path,
    profile_id: &str,
) -> Result<PathBuf, SettingsError> {
    let parent = managed_home_root
        .parent()
        .ok_or_else(|| SettingsError::Invalid("profile home root has no trusted parent".into()))?;
    Ok(parent
        .join("profile-shared-homes")
        .join(super::assessment::sha256_hex(profile_id.as_bytes())))
}

fn guest_paths_overlap(left: &str, right: &str) -> bool {
    left == right
        || left
            .strip_prefix(right)
            .is_some_and(|suffix| suffix.starts_with('/'))
        || right
            .strip_prefix(left)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn is_managed_profile_home_mount(
    profile: &AgentProfileSettings,
    mount: &PersistentMountSettings,
    managed_home_root: &Path,
) -> bool {
    mount.guest_path == profile.container_home
        && mount.host_path
            == managed_home_root.join(super::assessment::sha256_hex(profile.profile_id.as_bytes()))
}

fn is_managed_profile_home_path(profile: &AgentProfileSettings, path: &Path) -> bool {
    let expected_name = super::assessment::sha256_hex(profile.profile_id.as_bytes());
    path.file_name()
        .is_some_and(|name| name == expected_name.as_str())
        && path
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == "profile-homes")
}

fn structural_host_path(path: &Path) -> Result<PathBuf, SettingsError> {
    if !path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
    {
        return Err(SettingsError::Invalid(
            "host agent paths must be absolute canonical paths".into(),
        ));
    }
    Ok(path.to_path_buf())
}

#[derive(Clone, Copy)]
enum HostPathPermission {
    ManagedPrivate,
    SelectedDirectory,
    SelectedFile,
}

fn canonical_private_host_path(
    path: &Path,
    expected_directory: bool,
) -> Result<PathBuf, SettingsError> {
    canonical_host_path(path, expected_directory, HostPathPermission::ManagedPrivate)
}

fn canonical_selected_host_directory(path: &Path) -> Result<PathBuf, SettingsError> {
    canonical_host_path(path, true, HostPathPermission::SelectedDirectory)
}

fn canonical_selected_host_file(path: &Path) -> Result<PathBuf, SettingsError> {
    canonical_host_path(path, false, HostPathPermission::SelectedFile)
}

fn canonical_host_path(
    path: &Path,
    expected_directory: bool,
    permission: HostPathPermission,
) -> Result<PathBuf, SettingsError> {
    if !path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
    {
        return Err(SettingsError::Invalid(
            "host agent paths must be absolute canonical paths".into(),
        ));
    }
    let mut current = PathBuf::new();
    let mut components = path.components().peekable();
    while let Some(component) = components.next() {
        match component {
            std::path::Component::RootDir => current.push(component.as_os_str()),
            std::path::Component::Normal(part) => {
                current.push(part);
                let metadata = fs::symlink_metadata(&current).map_err(|error| {
                    if error.kind() == std::io::ErrorKind::NotFound {
                        SettingsError::Invalid("host agent path is missing".into())
                    } else {
                        SettingsError::Io(error)
                    }
                })?;
                if metadata.file_type().is_symlink()
                    || (components.peek().is_some() && !metadata.is_dir())
                {
                    return Err(SettingsError::Invalid(
                        "host agent path contains a symlink or non-directory parent".into(),
                    ));
                }
            }
            _ => {
                return Err(SettingsError::Invalid(
                    "host agent path is not a normalized Unix path".into(),
                ));
            }
        }
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| SettingsError::Invalid("host agent path is missing".into()))?;
    if metadata.file_type().is_symlink()
        || metadata.is_dir() != expected_directory
        || (!metadata.is_dir() && !metadata.is_file())
    {
        return Err(SettingsError::Invalid(
            "host agent path has an unexpected file type".into(),
        ));
    }
    let canonical = fs::canonicalize(path)
        .map_err(|_| SettingsError::Invalid("host agent path is unavailable".into()))?;
    if canonical != path || canonical.parent().is_none() {
        return Err(SettingsError::Invalid(
            "host agent path must not be a protected root or resolve through a symlink".into(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let owner = unsafe { libc::geteuid() };
        let mode = metadata.mode();
        let permissions_are_safe = match permission {
            HostPathPermission::ManagedPrivate => {
                mode & 0o077 == 0
                    && if expected_directory {
                        mode & 0o700 == 0o700
                    } else {
                        mode & 0o600 == 0o600
                    }
            }
            HostPathPermission::SelectedDirectory => mode & 0o700 == 0o700 && mode & 0o022 == 0,
            HostPathPermission::SelectedFile => mode & 0o400 == 0o400 && mode & 0o022 == 0,
        };
        if metadata.uid() != owner || !permissions_are_safe {
            return Err(SettingsError::Invalid(
                "host agent path has unsafe ownership or permissions".into(),
            ));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        return Err(SettingsError::Invalid(
            "host agent path privacy cannot be verified on this platform".into(),
        ));
    }
    Ok(canonical)
}

impl From<SandboxResourcesSettings> for super::SandboxResources {
    fn from(value: SandboxResourcesSettings) -> Self {
        Self {
            cpu_millis: value.cpu_millis,
            memory_mib: value.memory_mib,
            root_disk_mib: value.root_disk_mib,
            max_parallel_runs: value.max_parallel_runs,
            use_static: value.use_static,
        }
    }
}

impl PersistentMountSettings {
    fn to_core(&self) -> PersistentMount {
        PersistentMount {
            host_path: self.host_path.clone(),
            guest_path: self.guest_path.clone(),
            read_only: self.read_only,
            executable: self.executable,
        }
    }
}

impl RepositoryIdentitySettings {
    pub fn to_repository_identity(&self) -> Result<RepositoryIdentity, SettingsError> {
        RepositoryIdentity::new(
            &self.host,
            self.organization.clone(),
            self.repository.clone(),
        )
        .map_err(|_| SettingsError::Invalid("repository identity is invalid".into()))
    }
}

impl RepositoryHostSettings {
    pub fn to_repository_host_config(&self) -> Result<RepositoryHostConfig, SettingsError> {
        self.to_repository_host_config_with_mode(SettingsValidationMode::Structural)
    }

    pub(crate) fn to_repository_host_config_for_execution(
        &self,
    ) -> Result<RepositoryHostConfig, SettingsError> {
        self.to_repository_host_config_with_mode(SettingsValidationMode::Execution)
    }

    fn to_repository_host_config_with_mode(
        &self,
        validation_mode: SettingsValidationMode,
    ) -> Result<RepositoryHostConfig, SettingsError> {
        let credential = |value: &Option<String>| {
            value
                .as_deref()
                .map(CredentialReference::new)
                .transpose()
                .map_err(|_| SettingsError::Invalid("credential reference is invalid".into()))
        };
        Ok(RepositoryHostConfig {
            label_host: self.label_host.clone(),
            forge: self.forge.clone(),
            api_base_url: self.api_base_url.clone(),
            clone_transport: match &self.clone_transport {
                CloneTransportSettings::Https { base_url } => CloneTransport::Https {
                    base_url: base_url.clone(),
                },
                CloneTransportSettings::Ssh {
                    host,
                    user,
                    port,
                    ssh_directory,
                } => CloneTransport::Ssh {
                    host: host.clone(),
                    user: user.clone(),
                    port: *port,
                    ssh_directory: ssh_directory
                        .as_deref()
                        .map(|path| match validation_mode {
                            SettingsValidationMode::Structural => {
                                validate_ssh_directory_structure(path)
                            }
                            SettingsValidationMode::Execution => validate_ssh_directory(path),
                        })
                        .transpose()?,
                },
            },
            read_credential_ref: credential(&self.read_credential_ref)?,
            publication_credential_ref: credential(&self.publication_credential_ref)?,
        })
    }
}

fn validate_ssh_directory_structure(path: &Path) -> Result<PathBuf, SettingsError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(SettingsError::Invalid(
            "SSH directory must be an absolute normalized directory".into(),
        ));
    }
    Ok(path.to_path_buf())
}

fn validate_ssh_directory(path: &Path) -> Result<PathBuf, SettingsError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(SettingsError::Invalid(
            "SSH directory must be an absolute canonical directory".into(),
        ));
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| SettingsError::Invalid("SSH directory is unavailable".into()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(SettingsError::Invalid(
            "SSH directory must be a non-symlink directory".into(),
        ));
    }
    let canonical = fs::canonicalize(path)
        .map_err(|_| SettingsError::Invalid("SSH directory is unavailable".into()))?;
    if canonical != path || canonical.parent().is_none() {
        return Err(SettingsError::Invalid(
            "SSH directory must not be a protected root or symlink".into(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err(SettingsError::Invalid(
                "SSH directory must be owned by the current user and private".into(),
            ));
        }
    }
    Ok(canonical)
}

fn create_private_directory(path: &Path) -> Result<(), SettingsError> {
    fs::create_dir_all(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(SettingsError::Invalid(
            "settings parent must be a non-symlink directory".into(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn set_private_file(path: &Path) -> Result<(), SettingsError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), SettingsError> {
    #[cfg(test)]
    {
        let should_fail = DIRECTORY_SYNC_FAILURE.with(|failure| {
            let mut failure = failure.borrow_mut();
            let Some(failure) = failure.as_mut() else {
                return false;
            };
            if failure.path != path {
                return false;
            }
            failure.observed_calls += 1;
            failure.observed_calls == failure.matching_call
        });
        if should_fail {
            return Err(SettingsError::Io(std::io::Error::other(
                "injected directory sync failure",
            )));
        }
    }
    File::open(path)?.sync_all()?;
    Ok(())
}

fn open_checked_file(path: &Path) -> Result<File, SettingsError> {
    let before = fs::symlink_metadata(path)?;
    if before.file_type().is_symlink() || !before.is_file() {
        return Err(SettingsError::Invalid(
            "settings file must be a non-symlink regular file".into(),
        ));
    }
    let file = File::open(path)?;
    verify_opened_path(&file, path)?;
    Ok(file)
}

fn verify_opened_path(file: &File, path: &Path) -> Result<(), SettingsError> {
    let path_metadata = fs::symlink_metadata(path)?;
    let file_metadata = file.metadata()?;
    if path_metadata.file_type().is_symlink()
        || !path_metadata.is_file()
        || !file_metadata.is_file()
    {
        return Err(SettingsError::Invalid(
            "settings path is not a regular file".into(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if path_metadata.dev() != file_metadata.dev() || path_metadata.ino() != file_metadata.ino()
        {
            return Err(SettingsError::Invalid(
                "settings path changed while it was opened".into(),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod ssh_gui_tests {
    use super::*;

    fn host() -> RepositoryHostSettings {
        RepositoryHostSettings {
            label_host: "github.com".into(),
            forge: ForgeKind::GitHub,
            api_base_url: "https://api.github.com".into(),
            clone_transport: CloneTransportSettings::Ssh {
                host: "github.com".into(),
                user: "git".into(),
                port: Some(22),
                ssh_directory: None,
            },
            read_credential_ref: None,
            publication_credential_ref: None,
        }
    }

    fn approval(repository: &str, branches: &[&str]) -> NativeRepositoryApprovalSettings {
        NativeRepositoryApprovalSettings {
            repository: RepositoryIdentitySettings {
                host: "github.com".into(),
                organization: "openai".into(),
                repository: repository.into(),
            },
            allowed_base_branches: branches.iter().map(|branch| (*branch).into()).collect(),
        }
    }

    fn save(settings: CodingAgentSettings) -> Result<CodingAgentSettings, SettingsError> {
        let root = tempfile::tempdir().unwrap();
        let store = CodingAgentSettingsStore::new(root.path().join("settings.json"));
        store.compare_and_swap(0, settings, &HashSet::new())
    }

    #[cfg(unix)]
    #[test]
    fn post_commit_settings_sync_failure_keeps_settings_and_import_aligned() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let settings_path = temp.path().join("settings.json");
        let store = CodingAgentSettingsStore::new(&settings_path);
        let source_root = temp.path().join("host-home");
        let codex = source_root.join(".codex");
        let source_one = source_root.join("first.json");
        let source_two = source_root.join("second.json");
        let source_three = source_root.join("third.json");
        fs::create_dir_all(&codex).unwrap();
        fs::write(&source_one, b"first-source").unwrap();
        fs::write(&source_two, b"second-source").unwrap();
        fs::write(&source_three, b"third-source").unwrap();
        fs::set_permissions(&source_root, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&codex, fs::Permissions::from_mode(0o700)).unwrap();
        for source in [&source_one, &source_two, &source_three] {
            fs::set_permissions(source, fs::Permissions::from_mode(0o600)).unwrap();
        }

        const IMAGE: &str = "registry.example/agents/opencode:2026@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let profile_id = "sync-failure";
        let mut initial = CodingAgentSettings::default();
        initial.profiles.push(AgentProfileSettings {
            profile_id: profile_id.into(),
            version: 1,
            display_name: "OpenCode".into(),
            image_ref: IMAGE.into(),
            allowed_image_refs: vec![IMAGE.into()],
            command: vec!["opencode".into(), "run".into()],
            container_home: "/home/agent".into(),
            host_configuration: Some(HostAgentConfigurationSettings {
                codex_directory: Some(codex),
                claude_directory: None,
                claude_settings_file: Some(source_one.clone()),
            }),
            runtime_environment: BTreeMap::new(),
            resources: SandboxResourcesSettings {
                cpu_millis: 1000,
                memory_mib: 2048,
                root_disk_mib: 8192,
                max_parallel_runs: 1,
                use_static: false,
            },
            network_access: Default::default(),
            mount_allowed_roots: vec![source_root.clone()],
            persistent_mounts: Vec::new(),
            allowed_repositories: Vec::new(),
            credential_reference: None,
        });
        let saved = store.compare_and_swap(0, initial, &HashSet::new()).unwrap();

        let shared_home = temp
            .path()
            .join("profile-shared-homes")
            .join(super::super::assessment::sha256_hex(profile_id.as_bytes()));
        let imported = shared_home.join(".claude.json");
        assert_eq!(fs::read(&imported).unwrap(), b"first-source");

        let mut changed = saved.clone();
        changed.profiles[0]
            .host_configuration
            .as_mut()
            .unwrap()
            .claude_settings_file = Some(source_two);
        let result = with_directory_sync_failure(temp.path(), 1, || {
            store.compare_and_swap(saved.revision, changed, &HashSet::new())
        });
        assert!(matches!(result, Err(SettingsError::Io(_))));

        let committed = store.load().unwrap();
        assert_eq!(committed.revision, saved.revision + 1);
        assert_eq!(
            committed.profiles[0]
                .host_configuration
                .as_ref()
                .unwrap()
                .claude_settings_file,
            Some(source_root.join("second.json"))
        );
        assert_eq!(fs::read(&imported).unwrap(), b"second-source");

        let retry = store
            .compare_and_swap(committed.revision, committed, &HashSet::new())
            .unwrap();
        assert_eq!(retry.revision, saved.revision + 2);
        assert_eq!(fs::read(&imported).unwrap(), b"second-source");

        let mut changed_import = retry.clone();
        changed_import.profiles[0]
            .host_configuration
            .as_mut()
            .unwrap()
            .claude_settings_file = Some(source_three);
        let result = with_directory_sync_failure(&shared_home, 1, || {
            store.compare_and_swap(retry.revision, changed_import, &HashSet::new())
        });
        assert!(matches!(result, Err(SettingsError::Io(_))));
        let committed = store.load().unwrap();
        assert_eq!(committed.revision, retry.revision + 1);
        assert_eq!(fs::read(&imported).unwrap(), b"third-source");
    }

    #[test]
    fn ssh_directory_is_optional_on_legacy_json_and_uses_camel_case_when_present() {
        let legacy = serde_json::json!({
            "kind": "ssh",
            "host": "github.com",
            "user": "git",
            "port": 22
        });
        let parsed: CloneTransportSettings = serde_json::from_value(legacy).unwrap();
        assert!(matches!(
            parsed,
            CloneTransportSettings::Ssh {
                ssh_directory: None,
                ..
            }
        ));

        let configured = CloneTransportSettings::Ssh {
            host: "github.com".into(),
            user: "git".into(),
            port: Some(22),
            ssh_directory: Some(PathBuf::from("/home/alice/.ssh")),
        };
        let wire = serde_json::to_value(configured).unwrap();
        assert_eq!(wire["sshDirectory"], "/home/alice/.ssh");
        assert!(wire.get("ssh_directory").is_none());
    }

    #[test]
    fn explicit_empty_approval_list_is_valid_and_persisted_as_canonical_deny_all() {
        let settings = CodingAgentSettings {
            repository_hosts: vec![host()],
            native_repository_approvals: Some(Vec::new()),
            ..Default::default()
        };
        let saved = save(settings).unwrap();
        assert_eq!(saved.revision, 1);
        assert_eq!(saved.native_repository_approvals, Some(vec![]));

        let root = tempfile::tempdir().unwrap();
        let store = CodingAgentSettingsStore::new(root.path().join("settings.json"));
        let approvals_only = CodingAgentSettings {
            repository_hosts: vec![host()],
            native_repository_approvals: Some(Vec::new()),
            ..Default::default()
        };
        store
            .compare_and_swap(0, approvals_only, &HashSet::new())
            .unwrap();
        let expected_fingerprint = "a".repeat(64);
        let saved = store
            .compare_and_swap_native_baseline(1, expected_fingerprint.clone())
            .unwrap();
        let wire = serde_json::to_value(&saved).unwrap();
        assert_eq!(wire["nativeRepositoryApprovals"], serde_json::json!([]));
        assert_eq!(
            wire["nativeBackendPackageFingerprint"],
            expected_fingerprint
        );
        let mut ordinary_edit = saved.clone();
        ordinary_edit.auto_done_enabled = true;
        ordinary_edit.native_backend_package_fingerprint = None;
        let ordinary_edit = store
            .compare_and_swap(saved.revision, ordinary_edit, &HashSet::new())
            .unwrap();
        assert_eq!(
            ordinary_edit.native_backend_package_fingerprint.as_deref(),
            Some(expected_fingerprint.as_str())
        );

        let invalid_baseline = CodingAgentSettings {
            repository_hosts: vec![host()],
            native_repository_approvals: Some(vec![approval("repo", &["main"])]),
            ..Default::default()
        };
        assert!(save(invalid_baseline).is_ok());

        let root = tempfile::tempdir().unwrap();
        let store = CodingAgentSettingsStore::new(root.path().join("settings.json"));
        let current = store
            .compare_and_swap(
                0,
                {
                    CodingAgentSettings {
                        repository_hosts: vec![host()],
                        ..Default::default()
                    }
                },
                &HashSet::new(),
            )
            .unwrap();
        let mut changed = current.clone();
        changed.native_repository_approvals = Some(Vec::new());
        let saved = store
            .compare_and_swap(current.revision, changed, &HashSet::new())
            .unwrap();
        assert_eq!(saved.native_repository_approvals, Some(vec![]));
        assert_eq!(saved.native_backend_package_fingerprint, None);

        let mut package_change = saved.clone();
        package_change.native_backend_package_fingerprint = Some("b".repeat(64));
        assert!(matches!(
            store.compare_and_swap(saved.revision, package_change, &HashSet::new()),
            Err(SettingsError::Invalid(_))
        ));
    }

    #[test]
    fn native_approval_rejects_duplicate_repository_duplicate_branch_empty_branch_and_wrong_host() {
        for approvals in [
            vec![approval("repo", &["main"]), approval("repo", &["main"])],
            vec![approval("repo", &["main", "main"])],
            vec![approval("repo", &[""])],
        ] {
            let settings = CodingAgentSettings {
                repository_hosts: vec![host()],
                native_repository_approvals: Some(approvals),
                ..Default::default()
            };
            assert!(matches!(save(settings), Err(SettingsError::Invalid(_))));
        }

        let mut invalid = vec![approval("repo", &["main"])];
        invalid[0].repository.host = "gitlab.com".into();
        let settings = CodingAgentSettings {
            repository_hosts: vec![host()],
            native_repository_approvals: Some(invalid),
            ..Default::default()
        };
        assert!(matches!(save(settings), Err(SettingsError::Invalid(_))));
    }

    #[test]
    fn native_baseline_keeps_legacy_repository_and_token_references() {
        let root = tempfile::tempdir().unwrap();
        let store = CodingAgentSettingsStore::new(root.path().join("settings.json"));
        let mut settings = CodingAgentSettings::default();
        let mut configured_host = host();
        configured_host.read_credential_ref = Some("legacy-read".into());
        configured_host.publication_credential_ref = Some("legacy-publish".into());
        settings.repository_hosts = vec![configured_host];
        let saved = store
            .compare_and_swap(0, settings, &HashSet::new())
            .unwrap();

        let approved = store
            .compare_and_swap_native_baseline(saved.revision, "c".repeat(64))
            .unwrap();
        assert_eq!(approved.native_repository_approvals, None);
        assert_eq!(
            approved.repository_hosts[0].read_credential_ref.as_deref(),
            Some("legacy-read")
        );
        assert_eq!(
            approved.repository_hosts[0]
                .publication_credential_ref
                .as_deref(),
            Some("legacy-publish")
        );
    }

    #[cfg(unix)]
    #[test]
    fn ssh_directory_rejects_missing_file_symlink_public_and_protected_root() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let valid = temp.path().join("ssh");
        fs::create_dir(&valid).unwrap();
        fs::set_permissions(&valid, fs::Permissions::from_mode(0o700)).unwrap();

        let mut valid_host = host();
        valid_host.clone_transport = CloneTransportSettings::Ssh {
            host: "github.com".into(),
            user: "git".into(),
            port: Some(22),
            ssh_directory: Some(valid.clone()),
        };
        let settings = CodingAgentSettings {
            repository_hosts: vec![valid_host],
            ..Default::default()
        };
        assert!(save(settings).is_ok());

        for path in [temp.path().join("missing"), temp.path().join("file")] {
            if path.ends_with("file") {
                fs::write(&path, b"not a directory").unwrap();
            }
            let mut invalid_host = host();
            invalid_host.clone_transport = CloneTransportSettings::Ssh {
                host: "github.com".into(),
                user: "git".into(),
                port: Some(22),
                ssh_directory: Some(path),
            };
            let settings = CodingAgentSettings {
                repository_hosts: vec![invalid_host],
                ..Default::default()
            };
            assert!(matches!(save(settings), Err(SettingsError::Invalid(_))));
        }

        let real = temp.path().join("real");
        let symlink = temp.path().join("symlink");
        fs::create_dir(&real).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&real, &symlink).unwrap();
        let mut symlink_host = host();
        symlink_host.clone_transport = CloneTransportSettings::Ssh {
            host: "github.com".into(),
            user: "git".into(),
            port: Some(22),
            ssh_directory: Some(symlink),
        };
        let settings = CodingAgentSettings {
            repository_hosts: vec![symlink_host],
            ..Default::default()
        };
        assert!(matches!(save(settings), Err(SettingsError::Invalid(_))));

        let public = temp.path().join("public");
        fs::create_dir(&public).unwrap();
        fs::set_permissions(&public, fs::Permissions::from_mode(0o755)).unwrap();
        let mut public_host = host();
        public_host.clone_transport = CloneTransportSettings::Ssh {
            host: "github.com".into(),
            user: "git".into(),
            port: Some(22),
            ssh_directory: Some(public),
        };
        let settings = CodingAgentSettings {
            repository_hosts: vec![public_host],
            ..Default::default()
        };
        assert!(matches!(save(settings), Err(SettingsError::Invalid(_))));

        let mut root_host = host();
        root_host.clone_transport = CloneTransportSettings::Ssh {
            host: "github.com".into(),
            user: "git".into(),
            port: Some(22),
            ssh_directory: Some(PathBuf::from("/")),
        };
        let settings = CodingAgentSettings {
            repository_hosts: vec![root_host],
            ..Default::default()
        };
        assert!(matches!(save(settings), Err(SettingsError::Invalid(_))));
    }

    #[cfg(unix)]
    #[test]
    fn degraded_saved_ssh_directory_remains_loadable_and_can_be_repaired() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let settings_path = temp.path().join("settings.json");
        let ssh_directory = temp.path().join("ssh");
        fs::create_dir(&ssh_directory).unwrap();
        fs::set_permissions(&ssh_directory, fs::Permissions::from_mode(0o700)).unwrap();
        let mut settings = CodingAgentSettings::default();
        let mut configured_host = host();
        configured_host.clone_transport = CloneTransportSettings::Ssh {
            host: "github.com".into(),
            user: "git".into(),
            port: Some(22),
            ssh_directory: Some(ssh_directory.clone()),
        };
        settings.repository_hosts = vec![configured_host];
        let store = CodingAgentSettingsStore::new(&settings_path);
        let saved = store
            .compare_and_swap(0, settings, &HashSet::new())
            .unwrap();

        fs::remove_dir(&ssh_directory).unwrap();
        let degraded = store.load().unwrap();
        assert_eq!(degraded.revision, saved.revision);
        assert!(degraded.repository_hosts_core().is_err());

        let mut unrelated = degraded;
        unrelated.auto_done_enabled = true;
        let unrelated = store
            .compare_and_swap(saved.revision, unrelated, &HashSet::new())
            .unwrap();
        assert_eq!(unrelated.revision, saved.revision + 1);

        let repaired_directory = temp.path().join("repaired-ssh");
        fs::create_dir(&repaired_directory).unwrap();
        fs::set_permissions(&repaired_directory, fs::Permissions::from_mode(0o700)).unwrap();
        let mut repaired = store.load().unwrap();
        let CloneTransportSettings::Ssh { ssh_directory, .. } =
            &mut repaired.repository_hosts[0].clone_transport
        else {
            panic!("expected SSH host");
        };
        *ssh_directory = Some(repaired_directory);
        assert_eq!(
            store
                .compare_and_swap(unrelated.revision, repaired, &HashSet::new())
                .unwrap()
                .revision,
            unrelated.revision + 1
        );
    }
}
