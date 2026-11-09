use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::repository::{CredentialReference, RepositoryIdentity};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SandboxResources {
    pub cpu_millis: u32,
    pub memory_mib: u64,
    pub root_disk_mib: u64,
    pub max_parallel_runs: u32,
    /// The API intentionally defaults to false when omitted. The settings UI
    /// may call [`Self::with_ui_static_default`] to request its explicit true
    /// display default before serializing a profile.
    pub use_static: bool,
}

impl SandboxResources {
    pub(crate) fn validate_cpu_allocation(&self) -> Result<(), ProfileError> {
        if self.cpu_millis == 0 || !self.cpu_millis.is_multiple_of(1000) {
            return Err(ProfileError::Invalid(
                "SANDBOX CPU allocation must use whole virtual CPUs (multiples of 1000 millicores)"
                    .into(),
            ));
        }
        Ok(())
    }

    pub fn with_ui_static_default(mut self) -> Self {
        self.use_static = true;
        self
    }

    pub fn validate_against(&self, limits: &HostResourceLimits) -> Result<(), ProfileError> {
        if self.cpu_millis == 0
            || self.memory_mib == 0
            || self.root_disk_mib == 0
            || self.max_parallel_runs == 0
        {
            return Err(ProfileError::Invalid(
                "CPU, memory, disk, and parallelism must be positive".into(),
            ));
        }
        if limits.cpu_millis == 0
            || limits.memory_mib == 0
            || limits.root_disk_mib == 0
            || limits.max_parallel_runs == 0
        {
            return Err(ProfileError::Invalid(
                "executor resource limits must be positive".into(),
            ));
        }
        if self.cpu_millis > limits.cpu_millis
            || self.memory_mib > limits.memory_mib
            || self.root_disk_mib > limits.root_disk_mib
            || self.max_parallel_runs > limits.max_parallel_runs
        {
            return Err(ProfileError::Invalid(
                "profile resources exceed configured executor limits".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostResourceLimits {
    pub cpu_millis: u32,
    pub memory_mib: u64,
    pub root_disk_mib: u64,
    pub max_parallel_runs: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistentMount {
    pub host_path: PathBuf,
    pub guest_path: String,
    pub read_only: bool,
    pub executable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedHomeOverlay {
    pub parent: PersistentMount,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<ManagedHomeOverlayChild>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagedHomeOverlayChild {
    CodexDirectory { host_path: PathBuf },
    ClaudeDirectory { host_path: PathBuf },
}

impl ManagedHomeOverlayChild {
    pub fn host_path(&self) -> &Path {
        match self {
            Self::CodexDirectory { host_path } | Self::ClaudeDirectory { host_path } => host_path,
        }
    }

    pub fn guest_path(&self, container_home: &str) -> String {
        let child = match self {
            Self::CodexDirectory { .. } => ".codex",
            Self::ClaudeDirectory { .. } => ".claude",
        };
        if container_home == "/" {
            format!("/{child}")
        } else {
            format!("{container_home}/{child}")
        }
    }

    pub fn is_codex_directory(&self, expected: &Path) -> bool {
        matches!(self, Self::CodexDirectory { host_path } if host_path == expected)
    }

    pub fn is_claude_directory(&self, expected: &Path) -> bool {
        matches!(self, Self::ClaudeDirectory { host_path } if host_path == expected)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NetworkAccess {
    #[default]
    Disabled,
    Public,
    PublicAndPrivate,
    PublicPrivateAndHost,
}

impl NetworkAccess {
    pub fn runner_config(self) -> serde_json::Value {
        let profiles: &[&str] = match self {
            Self::Disabled => return serde_json::json!({"enabled": false}),
            Self::Public => &["public"],
            Self::PublicAndPrivate => &["public", "private"],
            Self::PublicPrivateAndHost => &["public", "private", "host"],
        };
        serde_json::json!({
            "enabled": true,
            "profiles": profiles,
            "max_tcp_connections": 128,
            "max_udp_connections": 128,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProfile {
    pub profile_id: String,
    pub version: u64,
    pub display_name: String,
    pub image_ref: String,
    pub allowed_image_refs: Vec<String>,
    pub command: Vec<String>,
    pub container_home: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_home_overlay: Option<ManagedHomeOverlay>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub runtime_environment: BTreeMap<String, String>,
    pub resources: SandboxResources,
    #[serde(default)]
    pub network_access: NetworkAccess,
    pub mount_allowed_roots: Vec<PathBuf>,
    pub persistent_mounts: Vec<PersistentMount>,
    pub allowed_repositories: Vec<RepositoryIdentity>,
    pub credential_reference: Option<CredentialReference>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProfileSnapshot {
    pub profile_id: String,
    pub version: u64,
    pub display_name: String,
    pub image_ref: String,
    pub image_digest: String,
    pub command: Vec<String>,
    pub container_home: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_home_overlay: Option<ManagedHomeOverlay>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub runtime_environment: BTreeMap<String, String>,
    pub resources: SandboxResources,
    #[serde(default)]
    pub network_access: NetworkAccess,
    pub validated_host_limits: HostResourceLimits,
    pub canonical_mount_allowed_roots: Vec<PathBuf>,
    pub persistent_mounts: Vec<PersistentMount>,
    pub allowed_repositories: Vec<RepositoryIdentity>,
    /// Opaque reference only; credential bytes, auth home contents, and keys
    /// are deliberately not fields in an agent profile snapshot.
    pub credential_reference: Option<CredentialReference>,
}

impl AgentProfileSnapshot {
    /// Create the tool home directories (`CODEX_HOME`, ...) that the runtime
    /// environment points into a writable persistent mount. A fresh dedicated
    /// home is empty and hides the image's own directories, and tools such as
    /// Codex refuse to start when their configured home does not exist.
    /// Paths outside every writable mount belong to the image and are left alone.
    pub fn ensure_runtime_environment_directories(&self) -> Result<(), ProfileError> {
        for guest in self.runtime_environment.values() {
            let guest = Path::new(guest);
            let Some((mount, relative)) = self
                .persistent_mounts
                .iter()
                .filter(|mount| !mount.read_only)
                .filter_map(|mount| {
                    guest
                        .strip_prefix(&mount.guest_path)
                        .ok()
                        .map(|relative| (mount, relative))
                })
                .max_by_key(|(mount, _)| mount.guest_path.len())
            else {
                continue;
            };
            let mut host = mount.host_path.clone();
            for component in relative.components() {
                let Component::Normal(name) = component else {
                    return Err(ProfileError::Invalid(
                        "runtime environment paths must be normalized".into(),
                    ));
                };
                host.push(name);
                create_private_directory_component(&host)?;
            }
        }
        Ok(())
    }

    pub fn validate_snapshot(&self) -> Result<(), ProfileError> {
        self.resources.validate_cpu_allocation()?;
        if !valid_digest_reference(&self.image_ref)
            || self.image_ref.rsplit_once('@').map(|(_, digest)| digest)
                != Some(self.image_digest.as_str())
        {
            return Err(ProfileError::Invalid(
                "runtime image must be pinned to its resolved digest".into(),
            ));
        }
        self.validate_structure()
    }

    pub(crate) fn validate_structure(&self) -> Result<(), ProfileError> {
        if !valid_profile_id(&self.profile_id)
            || self.version == 0
            || self.display_name.trim().is_empty()
            || self.display_name.chars().any(char::is_control)
            || self.command.is_empty()
            || self.command[0].is_empty()
            || self
                .command
                .iter()
                .any(|arg| arg.chars().any(char::is_control))
        {
            return Err(ProfileError::Invalid(
                "snapshot identity, pinned image, command, or resources are invalid".into(),
            ));
        }
        self.resources
            .validate_against(&self.validated_host_limits)?;
        validate_guest_path(&self.container_home)?;
        validate_runtime_environment(&self.runtime_environment)?;
        if self.canonical_mount_allowed_roots.iter().any(|root| {
            !root.is_absolute()
                || root
                    .components()
                    .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
        }) {
            return Err(ProfileError::Invalid(
                "snapshot mount roots must be canonical absolute paths".into(),
            ));
        }
        let mut host_mounts: BTreeSet<PathBuf> = BTreeSet::new();
        let mut guest_mounts = BTreeSet::new();
        for mount in &self.persistent_mounts {
            validate_guest_path(&mount.guest_path)?;
            if !mount.host_path.is_absolute()
                || mount
                    .host_path
                    .components()
                    .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
                || !self
                    .canonical_mount_allowed_roots
                    .iter()
                    .any(|root| mount.host_path.starts_with(root))
                || host_mounts.iter().any(|existing| {
                    existing.starts_with(&mount.host_path) || mount.host_path.starts_with(existing)
                })
                || !host_mounts.insert(mount.host_path.clone())
                || !guest_mounts.insert(mount.guest_path.clone())
                || guest_mounts.iter().any(|existing| {
                    existing != &mount.guest_path
                        && (existing.starts_with(&format!("{}/", mount.guest_path))
                            || mount.guest_path.starts_with(&format!("{existing}/")))
                })
            {
                return Err(ProfileError::Invalid(
                    "snapshot mount path is not canonical and disjoint".into(),
                ));
            }
        }
        validate_managed_home_overlay(
            self.managed_home_overlay.as_ref(),
            &self.container_home,
            &self.canonical_mount_allowed_roots,
            &self.persistent_mounts,
        )
        .map_err(|_| ProfileError::Invalid("snapshot managed home overlay is invalid".into()))?;
        let mut repositories = BTreeSet::new();
        for repository in &self.allowed_repositories {
            repository
                .validate()
                .map_err(|_| ProfileError::Invalid("invalid allowed repository".into()))?;
            if !repositories.insert(repository) {
                return Err(ProfileError::Invalid(
                    "allowed repository list contains duplicates".into(),
                ));
            }
        }
        if let Some(reference) = &self.credential_reference {
            reference
                .validate()
                .map_err(|_| ProfileError::Invalid("invalid credential reference".into()))?;
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum ProfileError {
    #[error("invalid agent profile: {0}")]
    Invalid(String),
    #[error("profile path could not be canonicalized: {0}")]
    Path(#[from] std::io::Error),
}

impl AgentProfile {
    pub fn validate_and_snapshot(
        &self,
        limits: &HostResourceLimits,
    ) -> Result<AgentProfileSnapshot, ProfileError> {
        if !valid_profile_id(&self.profile_id)
            || self.version == 0
            || self.display_name.trim().is_empty()
            || self.display_name.chars().any(char::is_control)
        {
            return Err(ProfileError::Invalid(
                "profile identity, version, and display name are required".into(),
            ));
        }
        self.resources.validate_against(limits)?;
        if !valid_digest_reference(&self.image_ref)
            || self
                .allowed_image_refs
                .iter()
                .any(|reference| !super::settings::valid_image_reference(reference))
            || !self
                .allowed_image_refs
                .iter()
                .any(|reference| reference == &self.image_ref)
            || self
                .allowed_image_refs
                .iter()
                .collect::<BTreeSet<_>>()
                .len()
                != self.allowed_image_refs.len()
        {
            return Err(ProfileError::Invalid(
                "image must be an exact digest reference on the allowlist".into(),
            ));
        }
        if self.command.is_empty()
            || self.command[0].is_empty()
            || self
                .command
                .iter()
                .any(|argument| argument.chars().any(char::is_control))
        {
            return Err(ProfileError::Invalid(
                "agent command must be a non-empty argument vector".into(),
            ));
        }
        validate_guest_path(&self.container_home)?;
        validate_runtime_environment(&self.runtime_environment)?;
        let (canonical_mount_allowed_roots, mounts) = self.validate_mounts()?;
        validate_managed_home_overlay(
            self.managed_home_overlay.as_ref(),
            &self.container_home,
            &canonical_mount_allowed_roots,
            &mounts,
        )?;
        let mut repositories = BTreeSet::new();
        for repository in &self.allowed_repositories {
            repository
                .validate()
                .map_err(|_| ProfileError::Invalid("invalid allowed repository".into()))?;
            if !repositories.insert(repository) {
                return Err(ProfileError::Invalid(
                    "allowed repository list contains duplicates".into(),
                ));
            }
        }

        let image_digest = self
            .image_ref
            .rsplit_once('@')
            .map(|(_, digest)| digest.to_string())
            .expect("validated digest reference contains @");
        let snapshot = AgentProfileSnapshot {
            profile_id: self.profile_id.clone(),
            version: self.version,
            display_name: self.display_name.clone(),
            image_ref: self.image_ref.clone(),
            image_digest,
            command: self.command.clone(),
            container_home: self.container_home.clone(),
            managed_home_overlay: self.managed_home_overlay.clone(),
            runtime_environment: self.runtime_environment.clone(),
            resources: self.resources,
            network_access: self.network_access,
            validated_host_limits: *limits,
            canonical_mount_allowed_roots,
            persistent_mounts: mounts,
            allowed_repositories: self.allowed_repositories.clone(),
            credential_reference: self.credential_reference.clone(),
        };
        snapshot.validate_snapshot()?;
        Ok(snapshot)
    }

    fn validate_mounts(&self) -> Result<(Vec<PathBuf>, Vec<PersistentMount>), ProfileError> {
        let roots = self
            .mount_allowed_roots
            .iter()
            .map(std::fs::canonicalize)
            .collect::<Result<Vec<_>, _>>()?;
        if roots.iter().any(|root| !root.is_dir()) {
            return Err(ProfileError::Invalid(
                "allowed mount roots must be directories".into(),
            ));
        }
        if roots.iter().collect::<BTreeSet<_>>().len() != roots.len() {
            return Err(ProfileError::Invalid(
                "allowed mount roots must be unique after canonicalization".into(),
            ));
        }
        if roots.is_empty() && !self.persistent_mounts.is_empty() {
            return Err(ProfileError::Invalid(
                "mounts require at least one allowed host root".into(),
            ));
        }
        let mut canonical_mounts = Vec::with_capacity(self.persistent_mounts.len());
        let mut host_paths = BTreeSet::new();
        let mut guest_paths = BTreeSet::new();
        for mount in &self.persistent_mounts {
            let canonical = std::fs::canonicalize(&mount.host_path)?;
            if (!canonical.is_dir() && !canonical.is_file())
                || !roots.iter().any(|root| canonical.starts_with(root))
            {
                return Err(ProfileError::Invalid(
                    "mount host path must be within an allowed canonical directory".into(),
                ));
            }
            validate_guest_path(&mount.guest_path)?;
            if host_paths.iter().any(|existing: &PathBuf| {
                existing.starts_with(&canonical) || canonical.starts_with(existing)
            }) || !host_paths.insert(canonical.clone())
                || !guest_paths.insert(mount.guest_path.clone())
                || guest_paths.iter().any(|existing| {
                    existing != &mount.guest_path
                        && (existing.starts_with(&format!("{}/", mount.guest_path))
                            || mount.guest_path.starts_with(&format!("{existing}/")))
                })
            {
                return Err(ProfileError::Invalid(
                    "persistent mounts must be disjoint from one another".into(),
                ));
            }
            canonical_mounts.push(PersistentMount {
                host_path: canonical,
                guest_path: mount.guest_path.clone(),
                read_only: mount.read_only,
                executable: mount.executable,
            });
        }
        Ok((roots, canonical_mounts))
    }
}

fn validate_managed_home_overlay(
    overlay: Option<&ManagedHomeOverlay>,
    container_home: &str,
    roots: &[PathBuf],
    mounts: &[PersistentMount],
) -> Result<(), ProfileError> {
    let Some(overlay) = overlay else {
        return Ok(());
    };
    let parent = &overlay.parent;
    validate_guest_path(&parent.guest_path)?;
    if parent.guest_path != container_home
        || parent.read_only
        || parent.executable
        || !parent.host_path.is_absolute()
        || !roots.iter().any(|root| root == &parent.host_path)
    {
        return Err(ProfileError::Invalid(
            "managed home overlay parent is not an exact trusted root".into(),
        ));
    }
    if mounts.iter().any(|mount| {
        mount.host_path == parent.host_path
            || mount.host_path.starts_with(&parent.host_path)
            || parent.host_path.starts_with(&mount.host_path)
            || guest_paths_overlap(&mount.guest_path, &parent.guest_path)
    }) {
        return Err(ProfileError::Invalid(
            "managed home overlay overlaps a persistent mount".into(),
        ));
    }
    let mut child_hosts = BTreeSet::<PathBuf>::new();
    let mut child_guests = BTreeSet::new();
    let mut has_codex = false;
    let mut has_claude = false;
    for child in &overlay.children {
        let path = child.host_path();
        if !path.is_absolute()
            || !roots.iter().any(|root| root == path)
            || !child_hosts.insert(path.to_path_buf())
            || path.starts_with(&parent.host_path)
            || parent.host_path.starts_with(path)
        {
            return Err(ProfileError::Invalid(
                "managed home overlay child is not an exact trusted root".into(),
            ));
        }
        let guest = child.guest_path(container_home);
        let duplicate_kind = match child {
            ManagedHomeOverlayChild::CodexDirectory { .. } => {
                let duplicate = has_codex;
                has_codex = true;
                duplicate
            }
            ManagedHomeOverlayChild::ClaudeDirectory { .. } => {
                let duplicate = has_claude;
                has_claude = true;
                duplicate
            }
        };
        if duplicate_kind || !child_guests.insert(guest.clone()) {
            return Err(ProfileError::Invalid(
                "managed home overlay child types are duplicated".into(),
            ));
        }
        validate_guest_path(&guest)?;
        if mounts.iter().any(|mount| {
            mount.host_path == path
                || mount.host_path.starts_with(path)
                || path.starts_with(&mount.host_path)
                || guest_paths_overlap(&mount.guest_path, &guest)
        }) {
            return Err(ProfileError::Invalid(
                "managed home overlay child overlaps a persistent mount".into(),
            ));
        }
    }
    Ok(())
}

fn guest_path_is_descendant(path: &str, parent: &str) -> bool {
    path.strip_prefix(parent)
        .is_some_and(|suffix| suffix.starts_with('/'))
}

fn guest_paths_overlap(left: &str, right: &str) -> bool {
    left == right || guest_path_is_descendant(left, right) || guest_path_is_descendant(right, left)
}

fn valid_profile_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
}

fn valid_digest_reference(value: &str) -> bool {
    let Some((image, digest)) = value.rsplit_once("@sha256:") else {
        return false;
    };
    !image.is_empty()
        && !image.contains("://")
        && !image
            .split('/')
            .any(|segment| matches!(segment, "." | ".."))
        && image
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._/: -".contains(&byte))
        && !image.contains(' ')
        && digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_guest_path(value: &str) -> Result<(), ProfileError> {
    let path = Path::new(value);
    if !path.is_absolute()
        || value.chars().any(char::is_control)
        || value.contains("//")
        || (value.len() > 1 && value.ends_with('/'))
        || value
            .split('/')
            .skip(1)
            .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
        || path
            .components()
            .any(|component| matches!(component, Component::Prefix(_)))
        || value.contains('\\')
    {
        return Err(ProfileError::Invalid(
            "guest mount paths must be absolute, normalized paths".into(),
        ));
    }
    Ok(())
}

/// Create one directory level without following a symlink at that level.
/// Existing directories are reused as-is; their owner and mode are not changed.
fn create_private_directory_component(path: &Path) -> Result<(), ProfileError> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => (),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(_) => {
            return Err(ProfileError::Invalid(
                "runtime tool home directory cannot be created".into(),
            ));
        }
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|_| {
        ProfileError::Invalid("runtime tool home directory cannot be inspected".into())
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ProfileError::Invalid(
            "runtime tool home must be a non-symlink directory".into(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_runtime_environment(
    environment: &BTreeMap<String, String>,
) -> Result<(), ProfileError> {
    for (key, value) in environment {
        if !matches!(key.as_str(), "CODEX_HOME" | "CARGO_HOME" | "RUSTUP_HOME") {
            return Err(ProfileError::Invalid(
                "runtime environment contains an unsupported key".into(),
            ));
        }
        validate_guest_path(value).map_err(|_| {
            ProfileError::Invalid("runtime environment values must be absolute guest paths".into())
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const IMAGE: &str = "registry.example/agents/opencode:2026@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn limits() -> HostResourceLimits {
        HostResourceLimits {
            cpu_millis: 8000,
            memory_mib: 16384,
            root_disk_mib: 102400,
            max_parallel_runs: 4,
        }
    }

    fn profile(root: &Path) -> AgentProfile {
        AgentProfile {
            profile_id: "opencode-default".into(),
            version: 1,
            display_name: "OpenCode".into(),
            image_ref: IMAGE.into(),
            allowed_image_refs: vec![IMAGE.into()],
            command: vec!["opencode".into(), "run".into()],
            container_home: "/home/codespace".into(),
            managed_home_overlay: None,
            runtime_environment: BTreeMap::new(),
            network_access: Default::default(),
            resources: SandboxResources {
                cpu_millis: 1000,
                memory_mib: 2048,
                root_disk_mib: 10240,
                max_parallel_runs: 1,
                use_static: false,
            },
            mount_allowed_roots: vec![root.to_path_buf()],
            persistent_mounts: vec![PersistentMount {
                host_path: root.join("workspace"),
                guest_path: "/workspace".into(),
                read_only: false,
                executable: false,
            }],
            allowed_repositories: Vec::new(),
            credential_reference: Some(CredentialReference::new("keychain:agent").unwrap()),
        }
    }

    fn home_profile(root: &Path, read_only: bool) -> AgentProfile {
        std::fs::create_dir(root.join("workspace")).unwrap();
        std::fs::create_dir(root.join("home")).unwrap();
        let mut configured = profile(root);
        configured.persistent_mounts.push(PersistentMount {
            host_path: root.join("home"),
            guest_path: "/home/codespace".into(),
            read_only,
            executable: false,
        });
        configured.runtime_environment = BTreeMap::from([
            ("CODEX_HOME".into(), "/home/codespace/.agent/.codex".into()),
            ("CARGO_HOME".into(), "/opt/cargo".into()),
        ]);
        configured
    }

    #[test]
    fn runtime_tool_homes_inside_a_writable_mount_are_created_privately() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let snapshot = home_profile(temp.path(), false)
            .validate_and_snapshot(&limits())
            .unwrap();

        snapshot.ensure_runtime_environment_directories().unwrap();
        snapshot.ensure_runtime_environment_directories().unwrap();

        let codex = temp.path().join("home/.agent/.codex");
        assert!(codex.is_dir());
        for created in [temp.path().join("home/.agent"), codex] {
            let mode = std::fs::metadata(&created).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "{}", created.display());
        }
        assert!(!temp.path().join("home/opt").exists());
    }

    #[test]
    fn runtime_tool_homes_are_not_created_through_read_only_mounts() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot = home_profile(temp.path(), true)
            .validate_and_snapshot(&limits())
            .unwrap();

        snapshot.ensure_runtime_environment_directories().unwrap();

        assert!(!temp.path().join("home/.agent").exists());
    }

    #[test]
    fn runtime_tool_homes_refuse_a_symlinked_path_component() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot = home_profile(temp.path(), false)
            .validate_and_snapshot(&limits())
            .unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), temp.path().join("home/.agent")).unwrap();

        assert!(snapshot.ensure_runtime_environment_directories().is_err());
        assert!(!outside.path().join(".codex").exists());
    }

    #[test]
    fn sandbox_cpu_admission_rejects_fractional_virtual_cpus_without_rounding() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("workspace")).unwrap();
        for cpu_millis in [1, 999, 1500, 2001] {
            let mut configured = profile(temp.path());
            configured.resources.cpu_millis = cpu_millis;
            assert!(
                configured.validate_and_snapshot(&limits()).is_err(),
                "fractional CPU {cpu_millis} admitted"
            );
        }
        let mut configured = profile(temp.path());
        configured.resources.cpu_millis = 2000;
        assert_eq!(
            configured
                .validate_and_snapshot(&limits())
                .unwrap()
                .resources
                .cpu_millis,
            2000
        );
    }

    #[test]
    fn network_selection_is_frozen_in_profile_and_rejects_unknown_access() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("workspace")).unwrap();
        let mut configured = profile(temp.path());
        configured.network_access = NetworkAccess::PublicAndPrivate;
        let frozen = configured.validate_and_snapshot(&limits()).unwrap();
        configured.network_access = NetworkAccess::Disabled;
        assert_eq!(frozen.network_access, NetworkAccess::PublicAndPrivate);
        assert!(
            serde_json::from_value::<NetworkAccess>(serde_json::json!("unrestricted")).is_err()
        );
        assert_eq!(
            NetworkAccess::Disabled.runner_config(),
            serde_json::json!({"enabled":false})
        );
        let public = NetworkAccess::Public.runner_config();
        assert_eq!(public["profiles"], serde_json::json!(["public"]));
        assert!(public["max_tcp_connections"].as_u64().unwrap() > 0);
        assert!(public["max_udp_connections"].as_u64().unwrap() > 0);
        assert_eq!(
            frozen.network_access.runner_config()["profiles"],
            serde_json::json!(["public", "private"])
        );
    }

    #[test]
    fn unselected_tag_candidates_do_not_become_runtime_images() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("workspace")).unwrap();
        let mut profile = profile(temp.path());
        profile.allowed_image_refs.extend([
            "future.registry/org/coder:next".into(),
            "another.registry/org/coder:stable".into(),
        ]);
        let snapshot = profile.validate_and_snapshot(&limits()).unwrap();
        assert_eq!(snapshot.image_ref, profile.image_ref);
        assert!(snapshot.image_ref.contains("@sha256:"));
        profile
            .allowed_image_refs
            .push("https://invalid.registry/coder:next".into());
        assert!(profile.validate_and_snapshot(&limits()).is_err());
    }

    #[test]
    fn explicitly_selected_agent_auth_and_ssh_mounts_are_preserved_under_guest_home() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("workspace")).unwrap();
        let mut profile = profile(temp.path());
        for name in [".claude", ".codex", ".ssh", ".config"] {
            let host_path = temp.path().join(name);
            std::fs::create_dir(&host_path).unwrap();
            profile.persistent_mounts.push(PersistentMount {
                host_path: host_path.clone(),
                guest_path: format!("/home/codespace/{name}"),
                read_only: name == ".ssh",
                executable: false,
            });
        }
        let snapshot = profile.validate_and_snapshot(&limits()).unwrap();
        snapshot.validate_snapshot().unwrap();
        assert_eq!(snapshot.persistent_mounts.len(), 5);
        assert!(
            snapshot
                .persistent_mounts
                .iter()
                .any(|mount| mount.guest_path == "/home/codespace/.ssh" && mount.read_only)
        );
    }

    #[test]
    fn profile_snapshot_requires_positive_bounded_resources_and_allowed_digest_image() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let profile = profile(temp.path());

        let snapshot = profile.validate_and_snapshot(&limits()).unwrap();
        assert_eq!(snapshot.image_digest, format!("sha256:{}", "a".repeat(64)));
        assert_eq!(
            snapshot.canonical_mount_allowed_roots,
            vec![std::fs::canonicalize(temp.path()).unwrap()]
        );
        assert_eq!(
            snapshot.persistent_mounts[0].host_path,
            std::fs::canonicalize(workspace).unwrap()
        );
        assert_eq!(
            snapshot.credential_reference.as_ref().unwrap().as_str(),
            "keychain:agent"
        );

        let mut over_limit = profile.clone();
        over_limit.resources.cpu_millis = limits().cpu_millis + 1;
        assert!(over_limit.validate_and_snapshot(&limits()).is_err());

        let mut mutable_image = profile;
        mutable_image.image_ref = "registry.example/agents/opencode:latest".into();
        assert!(mutable_image.validate_and_snapshot(&limits()).is_err());
    }

    #[test]
    fn missing_static_worker_field_deserializes_false_but_ui_can_explicitly_default_true() {
        let omitted: SandboxResources = serde_json::from_str(
            r#"{"cpu_millis":1,"memory_mib":1,"root_disk_mib":1,"max_parallel_runs":1}"#,
        )
        .unwrap();
        assert!(!omitted.use_static);
        assert!(omitted.with_ui_static_default().use_static);
        let serialized = serde_json::to_value(omitted.with_ui_static_default()).unwrap();
        assert_eq!(serialized["use_static"], true);
    }

    #[test]
    fn mount_validation_rejects_escape_and_overlapping_guest_paths() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let nested = workspace.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let mut path_escape = profile(temp.path());
        path_escape.persistent_mounts[0].guest_path = "/workspace/../outside".into();
        assert!(path_escape.validate_and_snapshot(&limits()).is_err());

        let mut profile = profile(temp.path());
        profile.persistent_mounts.push(PersistentMount {
            host_path: nested,
            guest_path: "/workspace/nested".into(),
            read_only: true,
            executable: false,
        });
        assert!(profile.validate_and_snapshot(&limits()).is_err());
    }

    #[test]
    fn managed_home_overlay_rejects_persistent_mounts_on_both_sides_of_parent_guest_path() {
        let temp = tempfile::tempdir().unwrap();
        let parent_root = temp.path().join("shared-home");
        let generic_root = temp.path().join("generic-mount");
        std::fs::create_dir(&parent_root).unwrap();
        std::fs::create_dir(&generic_root).unwrap();
        std::fs::create_dir(parent_root.join("workspace")).unwrap();

        let mut configured = profile(&parent_root);
        configured.mount_allowed_roots = vec![parent_root.clone(), generic_root.clone()];
        configured.persistent_mounts.clear();
        configured.managed_home_overlay = Some(ManagedHomeOverlay {
            parent: PersistentMount {
                host_path: parent_root.clone(),
                guest_path: configured.container_home.clone(),
                read_only: false,
                executable: false,
            },
            children: Vec::new(),
        });

        for guest_path in ["/home", "/home/codespace", "/home/codespace/.config"] {
            let mut overlapping = configured.clone();
            overlapping.persistent_mounts.push(PersistentMount {
                host_path: generic_root.clone(),
                guest_path: guest_path.into(),
                read_only: false,
                executable: false,
            });
            assert!(
                overlapping.validate_and_snapshot(&limits()).is_err(),
                "persistent guest path {guest_path} must overlap the managed HOME parent"
            );
        }

        let mut sibling = configured;
        sibling.persistent_mounts.push(PersistentMount {
            host_path: generic_root,
            guest_path: "/workspace-other".into(),
            read_only: false,
            executable: false,
        });
        assert!(sibling.validate_and_snapshot(&limits()).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn mount_validation_rejects_a_symlink_that_escapes_the_canonical_allowed_root() {
        let allowed = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let target = external.path().join("auth");
        std::fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, allowed.path().join("redirect")).unwrap();
        let mut profile = profile(allowed.path());
        profile.persistent_mounts[0].host_path = allowed.path().join("redirect");

        assert!(profile.validate_and_snapshot(&limits()).is_err());
    }

    #[test]
    fn runtime_environment_is_optional_in_legacy_profiles_and_validates_guest_paths() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("workspace")).unwrap();
        let template = profile(temp.path());
        let serialized = serde_json::to_value(&template).unwrap();
        assert!(serialized.get("runtime_environment").is_none());
        assert!(serialized.get("managed_home_overlay").is_none());
        let legacy: AgentProfile = serde_json::from_value(serialized).unwrap();
        assert!(legacy.runtime_environment.is_empty());
        assert!(legacy.managed_home_overlay.is_none());

        let mut configured = template.clone();
        configured
            .runtime_environment
            .insert("CARGO_HOME".into(), "/home/codespace/.cargo".into());
        configured
            .runtime_environment
            .insert("RUSTUP_HOME".into(), "/home/codespace/.rustup".into());
        let snapshot = configured.validate_and_snapshot(&limits()).unwrap();
        assert_eq!(
            snapshot.runtime_environment["CARGO_HOME"],
            "/home/codespace/.cargo"
        );
        let snapshot_wire = serde_json::to_value(&snapshot).unwrap();
        assert!(snapshot_wire.get("managed_home_overlay").is_none());
        let legacy_snapshot: AgentProfileSnapshot = serde_json::from_value(snapshot_wire).unwrap();
        assert!(legacy_snapshot.managed_home_overlay.is_none());
        assert!(
            serde_json::to_value(snapshot)
                .unwrap()
                .get("runtime_environment")
                .is_some()
        );

        for (key, value) in [
            ("HOME", "/home/codespace/.agent-home"),
            ("PATH", "/home/codespace/.bin"),
            ("CARGO_HOME", "relative/cargo"),
            ("RUSTUP_HOME", ""),
            ("CODEX_HOME", "/home/codespace/.agent-home\n"),
        ] {
            let mut invalid = profile(temp.path());
            invalid.runtime_environment.insert(key.into(), value.into());
            assert!(
                invalid.validate_and_snapshot(&limits()).is_err(),
                "{key}={value:?}"
            );
        }
    }
}
