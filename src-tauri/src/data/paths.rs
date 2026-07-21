use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::error::{AppError, AppResult};

/// Single application data root.
///
/// All persistent state — sqlite, LanceDB, plugin dylibs, llama.cpp model
/// cache, sidecar logs — lives under this directory:
/// "delete everything" maps to `rm -rf` of this single path.
#[derive(Debug, Clone)]
pub struct DataPaths {
    pub root: PathBuf,
}

impl DataPaths {
    /// macOS: `~/Library/Application Support/lookback/`.
    /// Other platforms fall back to the OS data dir + `lookback/`.
    pub fn detect() -> AppResult<Self> {
        Ok(Self {
            root: default_root()?,
        })
    }

    /// Resolve the data root honouring [`bootstrap.json`](BootstrapConfig).
    ///
    /// Two-stage: (1) compute the OS-default root, (2) read the
    /// bootstrap file at `<default>/bootstrap.json` and, if it carries a
    /// `data_root_override` pointing at an existing directory, use that
    /// instead. A missing / unparseable file or a non-existent override
    /// path silently falls back to the default so a user can never end
    /// up with an unbootable app.
    pub fn resolve() -> AppResult<Self> {
        let default = default_root()?;
        let bootstrap = load_bootstrap_config(&bootstrap_path_for(&default));
        let candidate = resolved_data_root(&default, bootstrap.data_root_override.as_deref());
        // If the override is usable-in-principle (parent is writable)
        // but the leaf doesn't exist — e.g. `purge_all_data` just `rm
        // -rf`'d it — materialise it now so subsequent `ensure()`
        // calls don't trip over a missing root. Fall back to default
        // if creation actually fails (read-only mount, permission
        // change since `override_is_usable` probed) so the app stays
        // bootable instead of failing at startup.
        if candidate != default
            && !candidate.is_dir()
            && std::fs::create_dir_all(&candidate).is_err()
        {
            return Ok(Self { root: default });
        }
        Ok(Self { root: candidate })
    }

    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn ensure(&self) -> AppResult<()> {
        self.ensure_runtime()?;
        for sub in [
            self.lancedb_dir(),
            self.lancedb_backup_dir(),
            self.memories_data_dir(),
        ] {
            std::fs::create_dir_all(sub)?;
        }
        Ok(())
    }

    /// Creates state required by local jobworkerp/conductor without creating
    /// a memories SQLite or LanceDB store for a remote-only connection.
    pub fn ensure_runtime(&self) -> AppResult<()> {
        for sub in [
            self.root.as_path(),
            &self.db_dir(),
            &self.plugins_dir(),
            &self.models_dir(),
            &self.log_dir(),
            &self.workers_dir(),
            &self.staged_workers_dir(),
            &self.lance_language_model_home(),
        ] {
            std::fs::create_dir_all(sub)?;
        }
        Ok(())
    }

    pub fn db_dir(&self) -> PathBuf {
        self.root.join("db")
    }

    pub fn sqlite_path(&self) -> PathBuf {
        self.db_dir().join("jobworkerp.sqlite3")
    }

    pub fn sqlite_url(&self) -> String {
        format!("sqlite://{}?mode=rwc", self.sqlite_path().display())
    }

    pub fn plugins_dir(&self) -> PathBuf {
        self.root.join("plugins")
    }

    pub fn models_dir(&self) -> PathBuf {
        self.root.join("models")
    }

    /// Where the sidecar's `HF_HOME` will actually point.
    ///
    /// Precedence (highest first):
    ///   1. The user-selected HF_HOME mode in `app-settings.json`:
    ///      - `Custom` → the explicit path.
    ///      - `DataRoot` → `<data>/models`.
    ///      - `Global` → see below. This is also the default `AppSettings`,
    ///        so a fresh-install user who has never opened Settings lands
    ///        here too. `app_settings = None` (used only as a defensive
    ///        fallback, e.g. by direct API callers) takes the same
    ///        `Global` path.
    ///   2. Global resolution (shared between explicit Global users and
    ///      the fresh-install default):
    ///      - shell env `HF_HOME` (verbatim, mirrors the user's terminal).
    ///      - `HF_HOME=` line in the optional `.env` template — keeps
    ///        `LOOKBACK_ENV_FILE` / workspace `.env` dev setups working
    ///        when they direct HF_HOME at a shared cache.
    ///      - `$XDG_CACHE_HOME/huggingface` if XDG_CACHE_HOME is set.
    ///      - `~/.cache/huggingface` (fixed across OSes, matching the
    ///        official `huggingface_hub` default).
    ///
    /// Used by BOTH `Sidecars::spawn_jobworkerp` (to inject `HF_HOME` into
    /// the child) AND `get_model_status` (the readiness scanner),
    /// so the readiness check follows the same cache the sidecar is actually
    /// populating.
    pub fn effective_hf_home(
        &self,
        env_file: Option<&Path>,
        app_settings: Option<&AppSettings>,
    ) -> PathBuf {
        if let Some(s) = app_settings {
            match s.hf_home_mode {
                HfHomeMode::Custom => {
                    if let Some(p) = s
                        .hf_home_path
                        .as_ref()
                        .filter(|p| !p.as_os_str().is_empty())
                    {
                        return p.clone();
                    }
                    // Misconfigured custom (empty path): fall through.
                }
                HfHomeMode::Global => return global_hf_home(env_file),
                HfHomeMode::DataRoot => return self.models_dir(),
            }
        }
        // No app-settings AND no caller-supplied mode (defensive). Treat
        // as Global so this branch agrees with what `load_app_settings`'s
        // default returns.
        global_hf_home(env_file)
    }

    pub fn log_dir(&self) -> PathBuf {
        self.root.join("log")
    }

    pub fn lancedb_dir(&self) -> PathBuf {
        self.root.join("lancedb")
    }

    pub fn workers_dir(&self) -> PathBuf {
        self.root.join("workers")
    }

    /// Heuristic: a directory is "an existing Lookback root" if it
    /// contains the sqlite file or the LanceDB subdir at the canonical
    /// sub-paths the running app uses. Lives next to the sub-path
    /// methods (not in `commands::app_settings`) so the canonical layout
    /// stays owned by `DataPaths` — a future rename of `db/` or `lancedb/`
    /// only needs to change this file.
    pub fn looks_like_existing_root(dir: &Path) -> bool {
        let probe = Self::with_root(dir);
        probe.sqlite_path().exists() || probe.lancedb_dir().exists()
    }

    pub fn memories_data_dir(&self) -> PathBuf {
        self.root.join("memories")
    }

    /// Canonical memories SQLite database.
    ///
    /// The released v0.0.6 DMG's memories sidecar actually opened this
    /// `default.sqlite3` file. Although its Lookback host source attempted
    /// to set `SQLITE_URL` to `memory.sqlite3`, it omitted the companion
    /// `SQLITE_MAX_CONNECTIONS` required by memories' RDB config and the
    /// sidecar silently used its default filename instead. Therefore this is
    /// the existing production store, not a rename from `memory.sqlite3`.
    /// All local-sidecar, startup-gate, and migration code must share this
    /// path so they cannot create or inspect a parallel empty database.
    pub fn memories_sqlite_path(&self) -> PathBuf {
        self.memories_data_dir().join("default.sqlite3")
    }

    /// `LANCE_LANGUAGE_MODEL_HOME` target. `lance-index` reads the Lindera
    /// FTS dictionary from `<this>/lindera/<dict>/config.yml`. We stage the
    /// bundled IPADIC dictionary under here at startup (see
    /// `lindera::stage_lindera_dict`).
    pub fn lance_language_model_home(&self) -> PathBuf {
        self.root.join("lance_language_models")
    }

    /// Directory where the staged Lindera IPADIC dictionary lives:
    /// `<lance_language_model_home>/lindera/ipadic/`.
    pub fn lindera_ipadic_dir(&self) -> PathBuf {
        self.lance_language_model_home()
            .join("lindera")
            .join("ipadic")
    }

    /// User-editable app settings (App data dir override, HF_HOME mode).
    /// Lives inside the data root so a purge wipes it too, mirroring the
    /// other JSON config files.
    pub fn app_settings_path(&self) -> PathBuf {
        self.root.join("app-settings.json")
    }

    /// Where the connection-target override (local vs remote
    /// server) is persisted. Lives *inside* the data root on purpose, so
    /// "delete all data" wipes it too — a purge is meant to
    /// leave no trace, and resetting back to the local default afterwards is
    /// the safe fallback (the user can't get stranded pointing at an
    /// unreachable remote).
    pub fn connection_config_path(&self) -> PathBuf {
        self.root.join("connection.json")
    }

    /// LLM provider settings (local vs external, model name, base_url, etc.).
    /// API key is NOT stored here — it goes to the OS keychain. Lives inside
    /// the data root so a purge wipes it too.
    pub fn llm_settings_path(&self) -> PathBuf {
        self.root.join("llm-settings.json")
    }

    /// Embedding model settings (preset id / custom override). Mirrors
    /// [`Self::llm_settings_path`]'s shape — a small JSON next to the other
    /// per-app config files so a purge wipes it too. Changing the saved
    /// embedding model rewrites this file and triggers a sidecar restart;
    /// see `commands::embedding_settings`.
    pub fn embedding_settings_path(&self) -> PathBuf {
        self.root.join("embedding-settings.json")
    }

    /// MCP server settings (enabled flag + advanced overrides). Mirrors
    /// [`Self::embedding_settings_path`]'s shape — a small JSON next to the
    /// other per-app config files so a purge wipes it too. Toggling MCP
    /// rewrites this file and triggers a sidecar restart (the `MCP_ENABLED`
    /// env is read at jobworkerp spawn time); see `commands::mcp_settings`.
    pub fn mcp_settings_path(&self) -> PathBuf {
        self.root.join("mcp-settings.json")
    }

    /// Marker that records the one-time insertion of disabled default
    /// periodic schedules. It lives under the data root so "delete all data"
    /// resets the first-run experience while a user-deleted template is not
    /// recreated on the next launch.
    pub fn periodic_defaults_seed_path(&self) -> PathBuf {
        self.root.join("periodic-defaults-seeded.json")
    }

    pub fn jobworkerp_maintenance_marker_path(&self) -> PathBuf {
        self.root.join("jobworkerp-maintenance.json")
    }

    /// Where pre-resize LanceDB directories are renamed to when the user
    /// switches embedding model (= vector dimension changes). The actual
    /// per-resize directory under here is suffixed with a timestamp so
    /// repeated swaps do not collide.
    pub fn lancedb_backup_dir(&self) -> PathBuf {
        self.root.join("lancedb-backup")
    }

    /// Staged worker YAML dir for runtime-rendered files (e.g. the
    /// auto-embedding workers YAML, which has a conditional
    /// `tokenizer_model_id` line that `expand_env` cannot express). The
    /// sidecar's `MEMORY_WORKERS_YAML` is pointed at a file in this dir
    /// when a staged version exists; otherwise it falls back to the
    /// committed YAML under `workers/workflows/`.
    pub fn staged_workers_dir(&self) -> PathBuf {
        self.workers_dir().join("staged")
    }

    /// Where the PIDs of the currently-spawned sidecars are recorded so the
    /// next launch can reap any that survived an app crash. macOS has no
    /// `PR_SET_PDEATHSIG`, so `kill_on_drop` / the `RunEvent::ExitRequested`
    /// stop path only cover graceful exits — a `kill -9` of the app strands
    /// the children, which then hold ports 9000/9010 and force the next
    /// launch onto random fallback ports. Reaping by recorded PID closes that
    /// gap. Lives inside the data root so a purge wipes it too.
    pub fn sidecar_pids_path(&self) -> PathBuf {
        self.root.join("sidecar.pids")
    }

    /// Advisory-lock file held for the lifetime of a running app instance.
    /// A second launch tries to acquire it non-blocking before reaping
    /// recorded PIDs: failure means another live instance owns it (so the
    /// recorded sidecars are *its* live children, not crash orphans) and the
    /// reap must be skipped. The lock is released automatically by the kernel
    /// when the holding process exits — even on `kill -9` — which is exactly
    /// the crash case we want to recover from. Lives inside the data root so a
    /// purge wipes it too.
    pub fn sidecar_lock_path(&self) -> PathBuf {
        self.root.join("sidecar.lock")
    }

    /// Migration owns this lock before it writes the legacy memories DB. It is
    /// distinct from the sidecar lock because startup can be blocked before
    /// any child process exists.
    pub fn memory_kind_migration_lock_path(&self) -> PathBuf {
        self.root.join("memory-kind-migration.lock")
    }

    /// The single active migration marker. Historical audits live below the
    /// work root and must never be mistaken for work that needs recovery.
    pub fn memory_kind_migration_marker_path(&self) -> PathBuf {
        self.root.join("memory-kind-migration.active.json")
    }

    pub fn memory_kind_migration_work_dir(&self) -> PathBuf {
        self.root.join("memory-kind-migration")
    }
}

/// HF cache root policy persisted in `app-settings.json`. The mode names
/// drive both the Rust resolver (`DataPaths::effective_hf_home`) and the
/// Settings UI radio buttons — keep the wire names in sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum HfHomeMode {
    /// Lookback-owned cache under `<data>/models`. Pick this to keep all
    /// models inside the single delete target.
    DataRoot,
    /// User's OS-wide HuggingFace cache: shell env `HF_HOME` first, then
    /// `~/.cache/huggingface`. Default because typical models run 10–30 GB
    /// each and most users already have a shared cache populated by
    /// `huggingface-cli` / `transformers` / Ollama — re-downloading them
    /// into a Lookback-private cache is wasteful disk and bandwidth.
    #[default]
    Global,
    /// Free-form path supplied via the UI (e.g. an external disk).
    Custom,
}

/// Persisted app-wide settings (sits next to `connection.json` and
/// `llm-settings.json` inside the data root). Read by
/// [`DataPaths::effective_hf_home`] every time the sidecar (re)starts so
/// a Settings change takes effect without an app relaunch.
///
/// The `data_root_override` field is shadowed here for serialisation
/// symmetry but the resolver only reads it out of [`BootstrapConfig`] at
/// the OS-default `bootstrap.json` — a copy inside the data root cannot
/// influence the root that is *currently* loaded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AppSettings {
    #[serde(default)]
    pub hf_home_mode: HfHomeMode,
    #[serde(default)]
    pub hf_home_path: Option<PathBuf>,
    /// Output language for generation (summary / personality / reflection /
    /// work-summary), `"ja"` | `"en"`. Persisted so headless paths (conductor
    /// periodic runs, which never touch the frontend) can pick the same
    /// language the UI is set to. `None` falls back to `"ja"`. The frontend
    /// dispatch commands also pass an explicit value that takes precedence.
    #[serde(default)]
    pub output_language: Option<String>,
    /// Explicit IANA timezone (e.g. `"Asia/Tokyo"`, `"America/New_York"`) for
    /// the agent-chat summary / import workflows' day/week/month boundary jq
    /// (read via `env.TZ` in the jobworkerp worker). `None` = "Auto": fall back
    /// to the process `TZ` env → `/etc/localtime` → `Asia/Tokyo`, preserving the
    /// historical OS-following behaviour. `Some(_)` takes precedence over the
    /// env so a GUI selection is honoured deterministically even under a DMG
    /// launch. Resolved by `sidecar::lifecycle::resolve_timezone`.
    #[serde(default)]
    pub timezone: Option<String>,
}

/// Bootstrap-time config: lives at a fixed path (`<os-default>/bootstrap.json`)
/// and is read BEFORE the data root is decided. The only allowed field is
/// the override target; anything else belongs in `app-settings.json`
/// inside the chosen data root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BootstrapConfig {
    #[serde(default)]
    pub data_root_override: Option<PathBuf>,
    #[serde(default)]
    pub setup_completed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapSnapshot {
    pub config: BootstrapConfig,
    pub setup_completed_present: bool,
}

/// OS-default data root (`~/Library/Application Support/lookback/` on
/// macOS). Centralised so `detect()` / `resolve()` / `bootstrap_path()`
/// don't drift apart.
pub fn default_root() -> AppResult<PathBuf> {
    let base =
        dirs::data_dir().ok_or_else(|| AppError::Config("no application data directory".into()))?;
    Ok(base.join("lookback"))
}

/// Path of the bootstrap config for the OS-default root. Always evaluated
/// against the default location so a user can recover from a bad override
/// by editing this single file.
pub fn bootstrap_path() -> AppResult<PathBuf> {
    Ok(bootstrap_path_for(&default_root()?))
}

fn bootstrap_path_for(default_root: &Path) -> PathBuf {
    default_root.join("bootstrap.json")
}

/// Read the bootstrap file; any error (missing, unparseable) returns the
/// default so a corrupt file can never block startup.
pub fn load_bootstrap_config(path: &Path) -> BootstrapConfig {
    load_bootstrap_snapshot(path).config
}

pub fn load_bootstrap_snapshot(path: &Path) -> BootstrapSnapshot {
    let Ok(bytes) = std::fs::read(path) else {
        return BootstrapSnapshot {
            config: BootstrapConfig::default(),
            setup_completed_present: false,
        };
    };
    // Parse once: the Value both reveals whether `setup_completed` was written
    // explicitly (distinguishing a legacy file from a pending one) and
    // deserializes into the typed config. A corrupt file yields the default
    // with the key reported absent.
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return BootstrapSnapshot {
            config: BootstrapConfig::default(),
            setup_completed_present: false,
        };
    };
    let setup_completed_present = value
        .as_object()
        .is_some_and(|object| object.contains_key("setup_completed"));
    BootstrapSnapshot {
        config: serde_json::from_value(value).unwrap_or_default(),
        setup_completed_present,
    }
}

pub fn has_existing_setup_evidence(data: &DataPaths) -> bool {
    [
        data.llm_settings_path(),
        data.embedding_settings_path(),
        data.app_settings_path(),
        data.connection_config_path(),
    ]
    .into_iter()
    .any(|path| path.is_file())
}

/// Whether a `data_root_override` should be honoured at boot.
///
/// Two acceptable shapes:
///   1. The override already exists as a directory.
///   2. The override doesn't exist yet but its direct parent IS an
///      existing writable directory — `DataPaths::resolve` will
///      `create_dir_all` the leaf on boot. This branch is what lets
///      "全データを削除" (`purge_all_data`, which `rm -rf`s the data
///      root itself but intentionally keeps `bootstrap.json` outside
///      it) leave the next boot pointing at the *same* custom root
///      the user configured, instead of silently reverting to the OS
///      default.
///
/// Anything else — relative paths, regular files at the override path,
/// external disks that happen to be unplugged right now (parent gone) —
/// falls back to default. Pure (only stats the filesystem; never
/// creates anything) so the UI can call it to compute
/// `pending_data_root` without side effects.
///
/// We deliberately check only the *direct parent* rather than walking
/// ancestors: the purge case is "leaf was just deleted, parent still
/// there", which is one level. Allowing a deeper missing chain would
/// silently materialise a `<typo>/<typo>/lookback` tree on a typo'd
/// absolute path that happened to land inside a tempdir or
/// `/Users/<name>`.
fn override_is_usable(p: &Path) -> bool {
    if !p.is_absolute() {
        return false;
    }
    // Single `metadata()` call covers both "exists" and "is dir":
    //   - dir found → reuse as-is.
    //   - non-dir found (regular file) → not recoverable by mkdir.
    //   - nothing there → fall through to the parent-writable recovery
    //     branch (purge case).
    match std::fs::metadata(p) {
        Ok(md) if md.is_dir() => true,
        Ok(_) => false,
        Err(_) => parent_is_writable(p),
    }
}

/// Resolve the data root the next launch will actually use.
///
/// Mirrors the gate inside [`DataPaths::resolve`] via
/// [`override_is_usable`]: an override is honoured both when it already
/// exists AND when it can be created at boot (recovery from
/// `purge_all_data`).
///
/// Exposed so the Settings UI can show the *same* path that the next
/// boot will land on, instead of echoing a raw override that
/// `DataPaths::resolve` is going to silently ignore.
pub fn resolved_data_root(default: &Path, override_path: Option<&Path>) -> PathBuf {
    match override_path {
        Some(p) if override_is_usable(p) => p.to_path_buf(),
        _ => default.to_path_buf(),
    }
}

/// Safe write probe: try to atomically create + remove a sentinel file
/// in `dir`. `O_CREAT|O_EXCL` (`create_new`) guarantees we never open an
/// unrelated existing file in write mode — without this, the validate
/// path (called on every 300 ms input debounce) would silently truncate
/// then delete any user file that happened to share the probe's name.
/// The filename mixes the process id with a per-call nanos suffix so
/// concurrent probes (or a left-over from a previous crashed call)
/// can't trip each other.
///
/// Lives in `data::paths` so both `commands::app_settings` (validate /
/// create) and `override_is_usable` (boot-time resolver) hit the same
/// implementation — the previous split between this probe and a naive
/// `metadata().readonly()` check meant a path could pass one and fail
/// the other on macOS sandbox / read-only-attr mounts.
/// Whether `path`'s direct parent is an existing writable directory —
/// i.e. a `create_dir_all(path)` would succeed at the leaf level. Shared
/// by every validation site that wants the "missing leaf, parent OK"
/// recovery semantic: `override_is_usable` (boot resolver),
/// `validate_data_root_impl` (Settings creatable badge), and
/// `validate_hf_home_custom_path` (Settings save guard). Centralising
/// this stops the three sites from drifting on edge cases (root-only
/// parents, empty strings, sandbox quirks).
pub(crate) fn parent_is_writable(path: &Path) -> bool {
    path.parent().is_some_and(|parent| {
        !parent.as_os_str().is_empty() && parent.is_dir() && is_writable(parent)
    })
}

pub(crate) fn is_writable(dir: &Path) -> bool {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let probe = dir.join(format!(
        ".lookback-write-probe-{}-{}",
        std::process::id(),
        nonce
    ));
    let opened = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .is_ok();
    if opened {
        // Only remove the file we ourselves just created — `create_new`
        // returning Ok means it didn't exist a moment ago.
        let _ = std::fs::remove_file(&probe);
    }
    opened
}

pub fn save_bootstrap_config(path: &Path, cfg: &BootstrapConfig) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(cfg)
        .map_err(|e| AppError::Config(format!("serialize bootstrap config: {e}")))?;
    std::fs::write(path, json)?;
    Ok(())
}

/// Read the persisted [`AppSettings`]. A missing / unparseable file falls
/// back to defaults — same robustness as the other config readers here.
/// Lives in `data::paths` (not `commands::app_settings`) so the sidecar
/// lifecycle can call it without an upward dependency on the commands
/// layer.
///
/// Falling back to default is intentional and matches the UI: a
/// fresh-install user sees `HfHomeMode::Global` as the selected mode in
/// HfHomeCard, and the sidecar must resolve the same mode so the shown
/// "OS グローバル" and the actually-used cache directory agree.
pub fn load_app_settings(path: &Path) -> AppSettings {
    let Ok(bytes) = std::fs::read(path) else {
        return AppSettings::default();
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

pub fn save_app_settings(path: &Path, cfg: &AppSettings) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(cfg)
        .map_err(|e| AppError::Config(format!("serialize app settings: {e}")))?;
    std::fs::write(path, json)?;
    Ok(())
}

/// OS-wide HuggingFace cache, used by [`HfHomeMode::Global`].
///
/// Resolution order:
///   1. Shell env `HF_HOME` (mirrors the user's terminal) or, equivalent
///      for dev setups, a `HF_HOME=` line in the optional `.env` template
///      that `LOOKBACK_ENV_FILE` / the workspace forwards to the sidecars.
///      Without this branch, a dev who points their `.env` at a shared
///      cache would silently switch to `~/.cache/huggingface` and
///      re-download every model after this feature shipped.
///   2. `$XDG_CACHE_HOME/huggingface` if XDG_CACHE_HOME is set.
///   3. `~/.cache/huggingface` (fixed across OSes — explicitly NOT
///      `dirs::cache_dir()`, which yields `~/Library/Caches/huggingface`
///      on macOS and would silently miss the user's existing cache).
///
/// Source: huggingface_hub env-vars docs, "HF_HOME" + "XDG_CACHE_HOME".
fn global_hf_home(env_file: Option<&Path>) -> PathBuf {
    if let Some(p) = lookup_external_hf_home(env_file) {
        return p;
    }
    if let Some(v) = std::env::var_os("XDG_CACHE_HOME")
        && !v.is_empty()
    {
        return PathBuf::from(v).join("huggingface");
    }
    if let Some(home) = dirs::home_dir() {
        return home.join(".cache").join("huggingface");
    }
    // Last-ditch fallback; `dirs::home_dir()` is `Some` on every supported
    // OS, but we still return a deterministic path instead of panicking.
    PathBuf::from(".cache/huggingface")
}

pub fn purge(root: &Path) -> AppResult<()> {
    if root.exists() {
        std::fs::remove_dir_all(root)?;
    }
    Ok(())
}

/// `HF_HOME` value supplied from outside the app: shell env first, then a
/// `HF_HOME=` line in the optional `.env` template. Empty values are
/// treated as "not set" so a `.env` author can explicitly clear the var.
fn lookup_external_hf_home(env_file: Option<&Path>) -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("HF_HOME")
        && !v.is_empty()
    {
        return Some(PathBuf::from(v));
    }
    let env_file = env_file?;
    let iter = dotenvy::from_path_iter(env_file).ok()?;
    for entry in iter.flatten() {
        if entry.0 == "HF_HOME" && !entry.1.is_empty() {
            return Some(PathBuf::from(entry.1));
        }
    }
    None
}

fn bundled_resource_path_from_root(root: &Path, relative: &str) -> PathBuf {
    root.join(relative)
}

fn bundled_resource_path_from_executable(exe: &Path, relative: &str) -> Option<PathBuf> {
    let contents = exe.parent()?.parent()?;
    Some(contents.join("Resources").join(relative))
}

/// Resolve a path below the packaged app's `Contents/Resources` directory.
/// This matches the explicit destination mapping in `tauri.conf.json`.
pub fn bundled_resource_path(app: &AppHandle, relative: &str) -> Option<PathBuf> {
    if let Ok(root) = app.path().resource_dir() {
        let candidate = bundled_resource_path_from_root(&root, relative);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let candidate = bundled_resource_path_from_executable(&exe, relative)?;
    candidate.exists().then_some(candidate)
}

/// Point `LOOKBACK_WORKERS_DIR` at the Tauri-bundled `workers/` so that
/// production `.app` builds (where `CARGO_MANIFEST_DIR` is unset) resolve
/// the worker YAMLs. Mirrors `plugins::resolve_plugins_source`: the
/// `tauri.conf.json` maps the source directory to the stable
/// `<app>/Contents/Resources/workers/` runtime path.
///
/// No-op when `LOOKBACK_WORKERS_DIR` is already set (dev / tests keep
/// their override) or when the resource isn't present (dev builds, where
/// `workers_bundle_dir` falls back to the `CARGO_MANIFEST_DIR` path).
pub fn stage_workers_env(app: &AppHandle) {
    if std::env::var_os("LOOKBACK_WORKERS_DIR").is_some() {
        return;
    }
    if let Some(dir) = bundled_resource_path(app, "workers") {
        // SAFETY: runs once during Tauri setup before any sidecar thread
        // reads the var, so there is no concurrent env access.
        unsafe { std::env::set_var("LOOKBACK_WORKERS_DIR", &dir) };
    }
}

/// Locate the bundled `workers/` directory that ships the LLM worker YAML
/// and the LLMPromptRunner-routed workflow YAMLs. Resolution:
///   1. `LOOKBACK_WORKERS_DIR` env override (set in dev, by integration
///      tests, or by [`stage_workers_env`] from the Tauri resource bundle).
///   2. `<CARGO_MANIFEST_DIR>/../workers` for `cargo run` / `pnpm tauri
///      dev`, where `CARGO_MANIFEST_DIR = agent-app/src-tauri/`.
pub fn workers_bundle_dir() -> AppResult<PathBuf> {
    if let Ok(p) = std::env::var("LOOKBACK_WORKERS_DIR") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Ok(pb);
        }
        return Err(AppError::Config(format!(
            "LOOKBACK_WORKERS_DIR={} does not exist",
            pb.display()
        )));
    }
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .map_err(|_| AppError::Config("CARGO_MANIFEST_DIR not set".into()))?;
    let candidate = PathBuf::from(manifest_dir).join("../workers");
    if !candidate.exists() {
        return Err(AppError::Config(format!(
            "workers bundle dir not found at {}",
            candidate.display()
        )));
    }
    Ok(candidate)
}

/// `<workers>/llm-workers.yaml` — the YAML applied at sidecar startup to
/// register the `memories-llm` named worker.
pub fn llm_workers_yaml() -> AppResult<PathBuf> {
    Ok(workers_bundle_dir()?.join("llm-workers.yaml"))
}

/// `<workers>/function-sets.yaml` — the YAML applied AFTER worker apply
/// to register the `lookback-rag` function set. Separate file because
/// the worker-YAML deserializer is `deny_unknown_fields` and rejects a
/// `function_sets:` key in the same document (rag-chat-design.md
/// DECIDE-CHAT-9).
pub fn function_sets_yaml() -> AppResult<PathBuf> {
    Ok(workers_bundle_dir()?.join("function-sets.yaml"))
}

/// `<workers>/workflows/` — root of the LLMPromptRunner-routed workflow
/// YAMLs (thread-summary / thread-personality / thread-reflection /
/// agent-chat-pipeline / auto-embedding-workers). Used both by the
/// memories-import CLI invocation (`--summarize-workflow <path>`) and by
/// `jobworkerp-client manifest apply` for the embedding worker.
pub fn workflows_bundle_dir() -> AppResult<PathBuf> {
    Ok(workers_bundle_dir()?.join("workflows"))
}

/// `<workers>/lang-workers` — the `--repo-root` for `memories-import
/// upsert-generation-workers`, which reads `<root>/workers/<feature>/
/// <feature>-single.yaml` and `<root>/workers/<feature>/prompts/<role>.<lang>.txt`
/// to register the language-specific generation workers
/// (`memories-<feature>-single-<lang>`). The single YAML kept here is the
/// agent-app local-merged variant (LLM call on `workerName: memories-llm`,
/// prompt via baked `workflow_context`), distinct from the runtime batch
/// YAMLs under `workflows/`.
pub fn lang_workers_repo_root() -> AppResult<PathBuf> {
    Ok(workers_bundle_dir()?.join("lang-workers"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tempdir_in_target_returns_a_unique_directory_per_test() {
        let first = tempdir_in_target();
        let second = tempdir_in_target();

        assert_ne!(first.path(), second.path());
        std::fs::remove_dir_all(first).ok();
        std::fs::remove_dir_all(second).ok();
    }

    #[test]
    fn ensure_creates_all_subdirs() {
        let tmp = tempdir_in_target();
        let paths = DataPaths::with_root(&tmp);
        paths.ensure().unwrap();

        assert!(paths.db_dir().exists());
        assert!(paths.plugins_dir().exists());
        assert!(paths.models_dir().exists());
        assert!(paths.log_dir().exists());
        assert!(paths.lancedb_dir().exists());
        assert!(paths.lancedb_backup_dir().exists());
        assert!(paths.workers_dir().exists());
        assert!(paths.staged_workers_dir().exists());

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn ensure_runtime_leaves_the_local_memory_store_absent() {
        let tmp = tempdir_in_target();
        let paths = DataPaths::with_root(&tmp);
        paths.ensure_runtime().unwrap();

        assert!(paths.db_dir().exists());
        assert!(paths.workers_dir().exists());
        assert!(!paths.memories_data_dir().exists());
        assert!(!paths.lancedb_dir().exists());

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn bundled_resource_path_joins_from_resource_root() {
        let root = PathBuf::from("/Applications/Lookback.app/Contents/Resources");
        assert_eq!(
            bundled_resource_path_from_root(&root, "workers/workflows"),
            root.join("workers/workflows")
        );
    }

    #[test]
    fn bundled_resource_path_falls_back_from_macos_executable() {
        let exe = PathBuf::from("/Applications/Lookback.app/Contents/MacOS/lookback-tauri");
        assert_eq!(
            bundled_resource_path_from_executable(&exe, "plugins"),
            Some(PathBuf::from(
                "/Applications/Lookback.app/Contents/Resources/plugins"
            ))
        );
    }

    #[test]
    fn embedding_settings_path_is_under_root() {
        let paths = DataPaths::with_root("/tmp/lookback-emb-test");
        assert_eq!(
            paths.embedding_settings_path(),
            PathBuf::from("/tmp/lookback-emb-test/embedding-settings.json")
        );
    }

    #[test]
    fn periodic_defaults_seed_path_is_under_root() {
        let paths = DataPaths::with_root("/tmp/lookback-periodic-test");
        assert_eq!(
            paths.periodic_defaults_seed_path(),
            PathBuf::from("/tmp/lookback-periodic-test/periodic-defaults-seeded.json")
        );
    }

    #[test]
    fn jobworkerp_maintenance_marker_path_is_under_root() {
        let paths = DataPaths::with_root("/tmp/lookback-maintenance-test");
        assert_eq!(
            paths.jobworkerp_maintenance_marker_path(),
            PathBuf::from("/tmp/lookback-maintenance-test/jobworkerp-maintenance.json")
        );
    }

    #[test]
    fn lancedb_backup_dir_is_sibling_of_lancedb_dir() {
        // Important for "delete all data" semantics — backup must live
        // under the same root so a single rm -rf wipes the user's data.
        let paths = DataPaths::with_root("/tmp/lookback-emb-test");
        assert_eq!(
            paths.lancedb_backup_dir(),
            PathBuf::from("/tmp/lookback-emb-test/lancedb-backup")
        );
    }

    #[test]
    fn memory_kind_migration_paths_are_root_local() {
        let paths = DataPaths::with_root("/tmp/lookback-migration-test");
        assert_eq!(
            paths.memory_kind_migration_lock_path(),
            PathBuf::from("/tmp/lookback-migration-test/memory-kind-migration.lock")
        );
        assert_eq!(
            paths.memory_kind_migration_marker_path(),
            PathBuf::from("/tmp/lookback-migration-test/memory-kind-migration.active.json")
        );
    }

    #[test]
    fn staged_workers_dir_is_under_workers_dir() {
        // Keep staged YAMLs separate from the committed bundle so a
        // future `LOOKBACK_WORKERS_DIR` pointed at staged/ would fail
        // fast instead of mixing staged + committed files.
        let paths = DataPaths::with_root("/tmp/lookback-emb-test");
        assert_eq!(
            paths.staged_workers_dir(),
            paths.workers_dir().join("staged")
        );
    }

    #[test]
    fn sqlite_url_is_rwc_with_absolute_path() {
        let paths = DataPaths::with_root("/tmp/lookback-test");
        let url = paths.sqlite_url();
        assert!(url.starts_with("sqlite:///tmp/lookback-test/db/jobworkerp.sqlite3"));
        assert!(url.ends_with("?mode=rwc"));
    }

    #[test]
    fn memories_sqlite_path_preserves_the_released_dmg_database_name() {
        let paths = DataPaths::with_root("/tmp/lookback-data");

        assert_eq!(
            paths.memories_sqlite_path(),
            PathBuf::from("/tmp/lookback-data/memories/default.sqlite3")
        );
    }

    #[test]
    fn effective_hf_home_no_settings_no_env_falls_back_to_dot_cache_huggingface() {
        // Pre-feature / no-Settings path with no shell HF_HOME and no
        // `.env` HF_HOME: must land in `~/.cache/huggingface` (HF Hub's
        // canonical default), NOT in `<data>/models`. This pins the
        // fresh-install agreement between the UI's "OS グローバル"
        // default and the sidecar's resolved cache.
        unsafe { std::env::remove_var("HF_HOME") };
        unsafe { std::env::remove_var("XDG_CACHE_HOME") };
        let paths = DataPaths::with_root("/tmp/lookback-hf-test");
        let expected = dirs::home_dir().unwrap().join(".cache").join("huggingface");
        assert_eq!(paths.effective_hf_home(None, None), expected);
    }

    #[test]
    fn effective_hf_home_honours_shell_env() {
        unsafe { std::env::set_var("HF_HOME", "/tmp/shared-hf-cache") };
        let paths = DataPaths::with_root("/tmp/lookback-hf-test");
        assert_eq!(
            paths.effective_hf_home(None, None),
            PathBuf::from("/tmp/shared-hf-cache")
        );
        unsafe { std::env::remove_var("HF_HOME") };
    }

    #[test]
    fn effective_hf_home_honours_env_file_when_shell_unset() {
        unsafe { std::env::remove_var("HF_HOME") };
        let tmp = tempdir_in_target();
        let env_file = tmp.join(".env");
        std::fs::write(&env_file, "HF_HOME=/tmp/dotenv-hf\n").unwrap();
        let paths = DataPaths::with_root(&tmp);
        assert_eq!(
            paths.effective_hf_home(Some(&env_file), None),
            PathBuf::from("/tmp/dotenv-hf")
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn effective_hf_home_treats_empty_env_file_value_as_unset() {
        // `.env` authors can clear the HF_HOME override by writing an
        // empty `HF_HOME=` line. The resolver must skip it and fall
        // through to the global default — without this guard a
        // `Sidecars` constructed from such an env file would inject
        // `HF_HOME=""` into the child and llama.cpp would download into
        // cwd.
        unsafe { std::env::remove_var("HF_HOME") };
        unsafe { std::env::remove_var("XDG_CACHE_HOME") };
        let tmp = tempdir_in_target();
        let env_file = tmp.join(".env");
        std::fs::write(&env_file, "HF_HOME=\n").unwrap();
        let paths = DataPaths::with_root(&tmp);
        let expected = dirs::home_dir().unwrap().join(".cache").join("huggingface");
        assert_eq!(paths.effective_hf_home(Some(&env_file), None), expected);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn effective_hf_home_data_root_mode_uses_models_dir_regardless_of_env() {
        unsafe { std::env::set_var("HF_HOME", "/tmp/shell-hf-cache") };
        let paths = DataPaths::with_root("/tmp/lookback-mode-test");
        let app = AppSettings {
            hf_home_mode: HfHomeMode::DataRoot,
            hf_home_path: None,
            output_language: None,
            timezone: None,
        };
        // Explicit `data_root` mode must beat the shell env — that's the
        // whole point of the user opting in to Lookback-owned storage.
        assert_eq!(
            paths.effective_hf_home(None, Some(&app)),
            paths.models_dir()
        );
        unsafe { std::env::remove_var("HF_HOME") };
    }

    #[test]
    fn effective_hf_home_global_mode_honours_shell_env() {
        unsafe { std::env::set_var("HF_HOME", "/tmp/user-global-hf") };
        let paths = DataPaths::with_root("/tmp/lookback-mode-test");
        let app = AppSettings {
            hf_home_mode: HfHomeMode::Global,
            hf_home_path: None,
            output_language: None,
            timezone: None,
        };
        assert_eq!(
            paths.effective_hf_home(None, Some(&app)),
            PathBuf::from("/tmp/user-global-hf")
        );
        unsafe { std::env::remove_var("HF_HOME") };
    }

    #[test]
    fn effective_hf_home_global_mode_falls_back_to_dot_cache_huggingface() {
        // Mirrors the official `huggingface_hub` default — `~/.cache/huggingface`
        // on EVERY OS (NOT `dirs::cache_dir()` which yields
        // `~/Library/Caches/huggingface` on macOS). Sharing the cache with
        // `huggingface-cli` / `transformers` is the whole point of Global mode,
        // and macOS would silently miss the user's existing cache without
        // this fix.
        unsafe { std::env::remove_var("HF_HOME") };
        unsafe { std::env::remove_var("XDG_CACHE_HOME") };
        let paths = DataPaths::with_root("/tmp/lookback-mode-test");
        let app = AppSettings {
            hf_home_mode: HfHomeMode::Global,
            hf_home_path: None,
            output_language: None,
            timezone: None,
        };
        let got = paths.effective_hf_home(None, Some(&app));
        let expected = dirs::home_dir().unwrap().join(".cache").join("huggingface");
        assert_eq!(got, expected);
    }

    #[test]
    fn effective_hf_home_global_mode_honours_env_file_when_shell_unset() {
        // Regression: a dev who sets `HF_HOME=/shared/cache` in their
        // workspace `.env` (forwarded via LOOKBACK_ENV_FILE) and never
        // opens Settings would silently land in `~/.cache/huggingface`
        // because `Global` mode short-circuited before consulting the
        // env file. Both the explicit-Global save and the fresh-install
        // default (which is Global) must walk the same path.
        unsafe { std::env::remove_var("HF_HOME") };
        unsafe { std::env::remove_var("XDG_CACHE_HOME") };
        let tmp = tempdir_in_target();
        let env_file = tmp.join(".env");
        std::fs::write(&env_file, "HF_HOME=/tmp/workspace-env-hf\n").unwrap();
        let paths = DataPaths::with_root("/tmp/lookback-mode-test");
        let app = AppSettings {
            hf_home_mode: HfHomeMode::Global,
            hf_home_path: None,
            output_language: None,
            timezone: None,
        };
        assert_eq!(
            paths.effective_hf_home(Some(&env_file), Some(&app)),
            PathBuf::from("/tmp/workspace-env-hf")
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn effective_hf_home_global_mode_honours_xdg_cache_home() {
        // Per the huggingface_hub docs, `XDG_CACHE_HOME` is consulted only
        // when `HF_HOME` is unset. Pinning this so a Linux user with the
        // XDG var set lands in the cache they actually share with other HF
        // tools, not in `~/.cache/huggingface`.
        unsafe { std::env::remove_var("HF_HOME") };
        unsafe { std::env::set_var("XDG_CACHE_HOME", "/tmp/xdg-cache-root") };
        let paths = DataPaths::with_root("/tmp/lookback-mode-test");
        let app = AppSettings {
            hf_home_mode: HfHomeMode::Global,
            hf_home_path: None,
            output_language: None,
            timezone: None,
        };
        let got = paths.effective_hf_home(None, Some(&app));
        assert_eq!(got, PathBuf::from("/tmp/xdg-cache-root/huggingface"));
        unsafe { std::env::remove_var("XDG_CACHE_HOME") };
    }

    #[test]
    fn effective_hf_home_custom_mode_uses_specified_path() {
        unsafe { std::env::set_var("HF_HOME", "/tmp/ignored-shell-hf") };
        let paths = DataPaths::with_root("/tmp/lookback-mode-test");
        let app = AppSettings {
            hf_home_mode: HfHomeMode::Custom,
            hf_home_path: Some(PathBuf::from("/Volumes/Ext/hf")),
            output_language: None,
            timezone: None,
        };
        assert_eq!(
            paths.effective_hf_home(None, Some(&app)),
            PathBuf::from("/Volumes/Ext/hf")
        );
        unsafe { std::env::remove_var("HF_HOME") };
    }

    #[test]
    fn effective_hf_home_custom_mode_falls_through_when_path_empty() {
        // Defensive: a Custom selection with an unfilled path must NOT
        // resolve to "" (which would make llama.cpp download into cwd).
        // Falls through to Global resolution.
        unsafe { std::env::remove_var("HF_HOME") };
        unsafe { std::env::remove_var("XDG_CACHE_HOME") };
        let paths = DataPaths::with_root("/tmp/lookback-mode-test");
        let app = AppSettings {
            hf_home_mode: HfHomeMode::Custom,
            hf_home_path: None,
            output_language: None,
            timezone: None,
        };
        let expected = dirs::home_dir().unwrap().join(".cache").join("huggingface");
        assert_eq!(paths.effective_hf_home(None, Some(&app)), expected);
    }

    #[test]
    fn resolved_data_root_uses_override_when_absolute_existing_dir() {
        let tmp = tempdir_in_target();
        let default = tmp.join("default");
        let override_dir = tmp.join("override");
        std::fs::create_dir_all(&default).unwrap();
        std::fs::create_dir_all(&override_dir).unwrap();
        assert_eq!(
            resolved_data_root(&default, Some(&override_dir)),
            override_dir
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn resolved_data_root_keeps_override_when_parent_is_writable() {
        // The leaf doesn't exist (e.g. `purge_all_data` just deleted it)
        // but the parent is a writable directory — `DataPaths::resolve`
        // will `create_dir_all` it on boot. UI `pending_data_root` MUST
        // agree so the user sees the same custom root they configured,
        // not a phantom revert to default.
        let tmp = tempdir_in_target();
        let default = tmp.join("default");
        std::fs::create_dir_all(&default).unwrap();
        let recoverable = tmp.join("override-after-purge");
        assert!(!recoverable.exists());
        assert_eq!(
            resolved_data_root(&default, Some(&recoverable)),
            recoverable
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn resolved_data_root_falls_back_when_parent_does_not_exist() {
        // External disk unplugged: the override's parent isn't a
        // writable directory either. `DataPaths::resolve` will fall
        // back to default rather than fail at startup.
        let tmp = tempdir_in_target();
        let default = tmp.join("default");
        std::fs::create_dir_all(&default).unwrap();
        let unrecoverable = tmp.join("missing-parent").join("override");
        assert_eq!(resolved_data_root(&default, Some(&unrecoverable)), default);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn resolved_data_root_falls_back_when_override_is_a_file() {
        let tmp = tempdir_in_target();
        let default = tmp.join("default");
        std::fs::create_dir_all(&default).unwrap();
        let file_override = tmp.join("a-file");
        std::fs::write(&file_override, b"").unwrap();
        assert_eq!(resolved_data_root(&default, Some(&file_override)), default);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn resolved_data_root_falls_back_when_override_is_relative() {
        let tmp = tempdir_in_target();
        let default = tmp.join("default");
        std::fs::create_dir_all(&default).unwrap();
        let relative = std::path::PathBuf::from("relative/path");
        assert_eq!(resolved_data_root(&default, Some(&relative)), default);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn resolved_data_root_falls_back_when_no_override() {
        let tmp = tempdir_in_target();
        let default = tmp.join("default");
        std::fs::create_dir_all(&default).unwrap();
        assert_eq!(resolved_data_root(&default, None), default);
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// Regression for `purge_all_data` + custom data root: purge deletes
    /// the data root *directory* but intentionally keeps `bootstrap.json`
    /// at the OS default. The next boot must:
    ///   1. Keep the override (UI showed it, user picked it).
    ///   2. Re-create the directory so `ensure()` doesn't fail.
    ///
    /// Without the "parent-writable" branch in `override_is_usable`, the
    /// override silently reverted to default and the user's choice was
    /// lost.
    #[test]
    fn purge_then_resolve_preserves_custom_data_root() {
        let tmp = tempdir_in_target();
        let default = tmp.join("default");
        let custom = tmp.join("custom-root");
        std::fs::create_dir_all(&default).unwrap();
        std::fs::create_dir_all(&custom).unwrap();
        // Populate the custom root so purge has something to delete.
        std::fs::write(custom.join("marker"), b"x").unwrap();

        // Simulate purge: remove the data root, leave bootstrap intact.
        purge(&custom).unwrap();
        assert!(!custom.exists(), "purge should remove the root");

        // The resolver still picks the same custom path.
        assert_eq!(
            resolved_data_root(&default, Some(&custom)),
            custom,
            "purge must not silently revert the user's data root choice"
        );
        // And `DataPaths::resolve`'s create_dir_all step (modelled by
        // ensure()) must succeed against the now-empty parent.
        let paths = DataPaths::with_root(&custom);
        paths.ensure().unwrap();
        assert!(custom.is_dir());

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn override_is_usable_accepts_existing_dir() {
        let tmp = tempdir_in_target();
        assert!(override_is_usable(&tmp));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn override_is_usable_accepts_missing_leaf_with_writable_parent() {
        let tmp = tempdir_in_target();
        let leaf = tmp.join("does-not-exist");
        assert!(override_is_usable(&leaf));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn override_is_usable_rejects_regular_file() {
        let tmp = tempdir_in_target();
        let f = tmp.join("a-file");
        std::fs::write(&f, b"").unwrap();
        assert!(!override_is_usable(&f));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn override_is_usable_rejects_relative_path() {
        assert!(!override_is_usable(Path::new("relative/dir")));
    }

    #[test]
    fn override_is_usable_rejects_nested_missing_path() {
        // `<tmp>/missing-mid/leaf`: only the direct parent
        // `<tmp>/missing-mid` is checked, and that doesn't exist. We
        // refuse rather than walk up to the first existing ancestor —
        // otherwise a typo'd absolute path like `/Users/me/typo/dataroot`
        // would silently materialise a phantom tree inside the home dir.
        // The legitimate purge case is "leaf was just deleted, parent
        // still there", which is a single missing level.
        let tmp = tempdir_in_target();
        let deep = tmp.join("missing-mid").join("leaf");
        assert!(!override_is_usable(&deep));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn resolve_uses_bootstrap_override_when_target_exists() {
        let tmp = tempdir_in_target();
        let default_root = tmp.join("default");
        let override_root = tmp.join("override");
        std::fs::create_dir_all(&default_root).unwrap();
        std::fs::create_dir_all(&override_root).unwrap();
        let bootstrap_file = bootstrap_path_for(&default_root);
        save_bootstrap_config(
            &bootstrap_file,
            &BootstrapConfig {
                data_root_override: Some(override_root.clone()),
                setup_completed: false,
            },
        )
        .unwrap();

        let loaded = load_bootstrap_config(&bootstrap_file);
        assert_eq!(loaded.data_root_override.as_ref(), Some(&override_root));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn load_bootstrap_returns_default_when_missing() {
        let tmp = tempdir_in_target();
        let cfg = load_bootstrap_config(&tmp.join("nope.json"));
        assert_eq!(cfg, BootstrapConfig::default());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn bootstrap_snapshot_distinguishes_missing_key_from_explicit_false() {
        let tmp = tempdir_in_target();
        let path = tmp.join("bootstrap.json");
        std::fs::write(&path, br#"{"data_root_override":null}"#).unwrap();
        let legacy = load_bootstrap_snapshot(&path);
        assert!(!legacy.setup_completed_present);
        assert!(!legacy.config.setup_completed);

        std::fs::write(
            &path,
            br#"{"data_root_override":null,"setup_completed":false}"#,
        )
        .unwrap();
        let pending = load_bootstrap_snapshot(&path);
        assert!(pending.setup_completed_present);
        assert!(!pending.config.setup_completed);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn existing_setup_evidence_ignores_auto_created_empty_directories() {
        let tmp = tempdir_in_target();
        let data = DataPaths::with_root(&tmp);
        data.ensure().unwrap();
        assert!(!has_existing_setup_evidence(&data));

        std::fs::write(data.llm_settings_path(), b"{}").unwrap();
        assert!(has_existing_setup_evidence(&data));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn load_bootstrap_returns_default_when_corrupt() {
        let tmp = tempdir_in_target();
        let path = tmp.join("bootstrap.json");
        std::fs::write(&path, b"{not json").unwrap();
        let cfg = load_bootstrap_config(&path);
        assert_eq!(cfg, BootstrapConfig::default());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn save_then_load_bootstrap_roundtrip() {
        let tmp = tempdir_in_target();
        let path = tmp.join("bootstrap.json");
        let cfg = BootstrapConfig {
            data_root_override: Some(PathBuf::from("/Volumes/Ext/lookback")),
            setup_completed: true,
        };
        save_bootstrap_config(&path, &cfg).unwrap();
        assert_eq!(load_bootstrap_config(&path), cfg);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn load_app_settings_returns_default_when_missing() {
        let tmp = tempdir_in_target();
        let s = load_app_settings(&tmp.join("nope.json"));
        assert_eq!(s, AppSettings::default());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn effective_hf_home_default_app_settings_honours_shell_env() {
        // A fresh-install user has no `app-settings.json`. The sidecar
        // resolver (`resolve_effective_hf_home`) passes
        // `AppSettings::default()` (= Global) in that case so the UI's
        // "OS グローバル" default and the actual resolution agree. The
        // Global branch consults shell `HF_HOME` first, so a user with
        // a pre-existing `HF_HOME` keeps using their cache.
        unsafe { std::env::set_var("HF_HOME", "/tmp/fresh-install-shell-hf") };
        let paths = DataPaths::with_root("/tmp/lookback-fresh-install-test");
        let default = AppSettings::default();
        assert_eq!(
            paths.effective_hf_home(None, Some(&default)),
            PathBuf::from("/tmp/fresh-install-shell-hf")
        );
        unsafe { std::env::remove_var("HF_HOME") };
    }

    #[test]
    fn effective_hf_home_explicit_data_root_wins_over_shell_env() {
        // Explicit DataRoot save = user wants Lookback-private cache —
        // their choice wins even though `HF_HOME` is set in the shell.
        // Pins that the saved mode is authoritative once the user has
        // touched Settings.
        unsafe { std::env::set_var("HF_HOME", "/tmp/shell-should-be-ignored") };
        let paths = DataPaths::with_root("/tmp/lookback-explicit-data-root-test");
        let explicit_data_root = AppSettings {
            hf_home_mode: HfHomeMode::DataRoot,
            hf_home_path: None,
            output_language: None,
            timezone: None,
        };
        assert_eq!(
            paths.effective_hf_home(None, Some(&explicit_data_root)),
            paths.models_dir()
        );
        unsafe { std::env::remove_var("HF_HOME") };
    }

    #[test]
    fn load_app_settings_returns_default_when_corrupt() {
        let tmp = tempdir_in_target();
        let path = tmp.join("app-settings.json");
        std::fs::write(&path, b"{not json").unwrap();
        assert_eq!(load_app_settings(&path), AppSettings::default());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn save_then_load_app_settings_round_trip() {
        let tmp = tempdir_in_target();
        let path = tmp.join("app-settings.json");
        let cfg = AppSettings {
            hf_home_mode: HfHomeMode::Custom,
            hf_home_path: Some(PathBuf::from("/Volumes/Ext/hf")),
            output_language: None,
            timezone: None,
        };
        save_app_settings(&path, &cfg).unwrap();
        assert_eq!(load_app_settings(&path), cfg);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn app_settings_default_round_trips() {
        let s = AppSettings::default();
        let back: AppSettings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(s, back);
        // `Global` is the fresh-install default so the first save from
        // Settings doesn't silently flip the user out of their existing
        // huggingface-cli / transformers cache.
        assert_eq!(back.hf_home_mode, HfHomeMode::Global);
        // Output language is unset by default → the backend resolver falls back
        // to "ja".
        assert!(back.output_language.is_none());
    }

    #[test]
    fn app_settings_output_language_round_trips_and_defaults_when_absent() {
        let tmp = tempdir_in_target();
        let path = tmp.join("app-settings.json");
        let cfg = AppSettings {
            hf_home_mode: HfHomeMode::Global,
            hf_home_path: None,
            output_language: Some("en".into()),
            timezone: None,
        };
        save_app_settings(&path, &cfg).unwrap();
        assert_eq!(
            load_app_settings(&path).output_language.as_deref(),
            Some("en")
        );

        // A settings file written by an older build (no `output_language` key)
        // still loads, with the field defaulting to None.
        std::fs::write(&path, r#"{"hf_home_mode":"global"}"#).unwrap();
        assert!(load_app_settings(&path).output_language.is_none());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn lang_workers_repo_root_is_under_workers_bundle() {
        unsafe { std::env::remove_var("LOOKBACK_WORKERS_DIR") };
        let root = lang_workers_repo_root().unwrap();
        assert!(
            root.ends_with("workers/lang-workers"),
            "lang-workers repo root should sit under the workers bundle: {root:?}"
        );
        // `upsert-generation-workers --repo-root <root>` reads <root>/workers/<feature>/...
        assert!(
            root.join("workers/thread-summary/thread-summary-single.yaml")
                .exists(),
            "expected the thread-summary single under the lang-workers tree"
        );
    }

    #[test]
    fn app_settings_serde_uses_snake_case_modes() {
        // Pin the wire format so the frontend can rely on these strings.
        for (mode, wire) in [
            (HfHomeMode::DataRoot, "data_root"),
            (HfHomeMode::Global, "global"),
            (HfHomeMode::Custom, "custom"),
        ] {
            let s = AppSettings {
                hf_home_mode: mode,
                hf_home_path: None,
                output_language: None,
                timezone: None,
            };
            let json = serde_json::to_value(&s).unwrap();
            assert_eq!(json["hf_home_mode"], wire);
        }
    }

    #[test]
    fn purge_removes_root() {
        let tmp = tempdir_in_target();
        std::fs::write(tmp.join("marker"), "x").unwrap();
        assert!(tmp.join("marker").exists());
        purge(&tmp).unwrap();
        assert!(!tmp.exists());
    }

    /// `workers_bundle_dir` resolves to `<CARGO_MANIFEST_DIR>/../workers`
    /// for `cargo test`. The directory is a real one populated by Phase
    /// C-0; the assertions verify the path semantics, not its contents.
    #[test]
    fn workers_bundle_dir_falls_back_to_manifest_relative() {
        // SAFETY: the test runs single-threaded under `--test-threads=1`
        // per the project's testing convention, so removing env keys
        // here cannot race other tests' reads.
        unsafe { std::env::remove_var("LOOKBACK_WORKERS_DIR") };
        let dir =
            workers_bundle_dir().expect("manifest-relative fallback should exist in this repo");
        assert!(dir.ends_with("workers"));
        assert!(dir.exists());
        assert!(dir.join("llm-workers.yaml").exists());
        assert!(dir.join("workflows").is_dir());
    }

    /// Both reflection embedding pipelines (summary F-G11a + intent
    /// F-G11b) must ship BOTH their workflow body and their worker-def
    /// YAML in the bundle. memories constructs the summary AND intent
    /// dispatchers together when `MEMORY_REFLECTION_DISPATCH_ENABLED`,
    /// and `lifecycle.rs` points `REFLECTION_WORKERS_YAML` /
    /// `REFLECTION_INTENT_WORKERS_YAML` at these files; a missing file
    /// makes the dispatcher fall back to memories' compile-time default
    /// (absent in a bundled .app) and fail to register at startup.
    #[test]
    fn reflection_embedding_workflow_yamls_are_bundled() {
        unsafe { std::env::remove_var("LOOKBACK_WORKERS_DIR") };
        let dir = workers_bundle_dir()
            .unwrap()
            .join("workflows")
            .join("thread-reflection");
        for name in [
            "auto-reflection-summary-embedding.yaml",
            "auto-reflection-summary-embedding-workers.yaml",
            "auto-reflection-intent-embedding.yaml",
            "auto-reflection-intent-embedding-workers.yaml",
        ] {
            assert!(dir.join(name).exists(), "missing bundled workflow: {name}");
        }
    }

    /// The jobworkerp-rs DSL validator rejects two shapes the embedding
    /// workflows previously used: (a) a doTask-style `if` + nested `do`
    /// branch carried under `then:` / `else:` ("did not match any
    /// variant of untagged enum Task"), and (b) a bare `exit:` task.
    /// Conditional branching must instead be mutually-exclusive
    /// conditional runTasks + the `then: exit` flow directive. This is a
    /// plain-text scan (no YAML/DSL parser dependency in this crate) that
    /// guards against a regression to the rejected forms.
    #[test]
    fn reflection_embedding_workflows_avoid_rejected_dsl_shapes() {
        unsafe { std::env::remove_var("LOOKBACK_WORKERS_DIR") };
        let dir = workers_bundle_dir()
            .unwrap()
            .join("workflows")
            .join("thread-reflection");
        for name in [
            "auto-reflection-summary-embedding.yaml",
            "auto-reflection-intent-embedding.yaml",
        ] {
            let yaml = std::fs::read_to_string(dir.join(name)).unwrap();
            // `then:`/`else:` may only appear inside comments here. Strip
            // comment bodies before scanning so the prose explaining the
            // fix doesn't trip the guard. Only a `#` at line-start or
            // preceded by whitespace begins a comment — a `#` mid-token
            // (e.g. inside a quoted jq string) is left intact so the scan
            // can't be defeated or false-tripped by code content.
            let code: String = yaml
                .lines()
                .map(strip_yaml_comment)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                !code.contains("else:"),
                "{name}: `else:` is not a valid DSL task key"
            );
            assert!(
                !code.contains("exit:"),
                "{name}: bare `exit:` task is invalid; use `then: exit`"
            );
            // A `then:` whose value opens a block (followed by `do:`) is
            // the rejected nested form. The only legal `then:` here is the
            // inline flow directive `then: exit`.
            for (i, line) in code.lines().enumerate() {
                let t = line.trim_start();
                if t.starts_with("then:") {
                    assert_eq!(
                        t,
                        "then: exit",
                        "{name} line {}: only `then: exit` is allowed, got `{t}`",
                        i + 1
                    );
                }
            }
        }
    }

    /// `try` already represents the task list to attempt. The workflow
    /// schema defines `try: <taskList>` and reserves `do:` for `catch.do`,
    /// so `try: { do: ... }` fails runner loading as an untagged `Task`.
    #[test]
    fn reflection_embedding_workflows_use_schema_valid_try_task_shape() {
        unsafe { std::env::remove_var("LOOKBACK_WORKERS_DIR") };
        let dir = workers_bundle_dir()
            .unwrap()
            .join("workflows")
            .join("thread-reflection");
        for name in [
            "auto-reflection-summary-embedding.yaml",
            "auto-reflection-intent-embedding.yaml",
        ] {
            let yaml = std::fs::read_to_string(dir.join(name)).unwrap();
            let code_lines: Vec<_> = yaml.lines().map(strip_yaml_comment).collect();
            for (i, line) in code_lines.iter().enumerate() {
                if line.trim() != "try:" {
                    continue;
                }
                let Some(next) = code_lines
                    .iter()
                    .skip(i + 1)
                    .map(|line| line.trim())
                    .find(|line| !line.is_empty())
                else {
                    continue;
                };
                assert_ne!(
                    next,
                    "do:",
                    "{name} line {}: `try:` must contain a task list directly; `do:` is only valid under `catch:`",
                    i + 1
                );
            }
        }
    }

    #[test]
    fn reflection_embedding_workflows_pin_grpc_runner_method() {
        unsafe { std::env::remove_var("LOOKBACK_WORKERS_DIR") };
        let dir = workers_bundle_dir()
            .unwrap()
            .join("workflows")
            .join("thread-reflection");
        for name in [
            "auto-reflection-summary-embedding.yaml",
            "auto-reflection-intent-embedding.yaml",
        ] {
            let yaml = std::fs::read_to_string(dir.join(name)).unwrap();
            assert!(
                yaml.matches("using: unary").count() >= 5,
                "{name}: all GRPC worker calls must specify using: unary"
            );
        }
        let intent_yaml =
            std::fs::read_to_string(dir.join("auto-reflection-intent-embedding.yaml")).unwrap();
        assert!(
            intent_yaml.contains(".successCount") && !intent_yaml.contains(".success_count"),
            "intent workflow must read protobuf JSON's lowerCamelCase successCount field"
        );
    }

    /// Every workflow YAML under `workers/workflows/` must parse as
    /// valid YAML. `jobworkerp-client`'s `$file:` loader embeds these
    /// files verbatim into the worker `settings.workflow_data` payload;
    /// the WORKFLOW runner reparses them at job runtime, so a syntactic
    /// error survives `worker apply` silently and only fires on the
    /// first dispatch as "Failed to load workflow from json=document:".
    /// Catch them at unit-test time instead.
    ///
    /// Concrete regression: `lookback-recall.yaml` had an unquoted
    /// `result: ${ { sources: $sources } }` where the inner
    /// `sources:` was parsed as a nested YAML mapping, exploding on
    /// the first chat dispatch (PR-AGENT-DEBUG, 2026-05-28).
    #[test]
    fn workflow_yamls_parse() {
        unsafe { std::env::remove_var("LOOKBACK_WORKERS_DIR") };
        let workflows_dir = workers_bundle_dir().unwrap().join("workflows");
        let mut bad = Vec::new();
        let mut count = 0usize;
        for entry in walkdir(&workflows_dir) {
            if entry.extension().and_then(|s| s.to_str()) != Some("yaml") {
                continue;
            }
            count += 1;
            let body = std::fs::read_to_string(&entry).unwrap();
            // jobworkerp's loader runs `expand_env` (a raw text substitution)
            // BEFORE parsing, so a workflow can carry UNQUOTED `%{VAR:-default}`
            // placeholders that are only valid YAML once substituted (e.g.
            // `lookback-recall.yaml`'s `memories_grpc_port`, which must expand
            // to a native integer, not a quoted string — see that file's note).
            // Mirror that here so the test validates the post-expansion document
            // the runtime actually parses.
            let expanded = expand_env_defaults(&body);
            if let Err(e) = serde_yaml::from_str::<serde_yaml::Value>(&expanded) {
                bad.push(format!(
                    "{}: {e}",
                    entry.strip_prefix(&workflows_dir).unwrap().display()
                ));
            }
        }
        assert!(count > 0, "no workflow YAMLs scanned (path wrong?)");
        assert!(
            bad.is_empty(),
            "workflow YAML parse failures:\n  - {}",
            bad.join("\n  - ")
        );
    }

    #[test]
    fn lookback_raw_recall_search_avoids_thread_filter_expansion() {
        let workflow = std::fs::read_to_string(
            workers_bundle_dir()
                .unwrap()
                .join("workflows/rag/lookback-recall.yaml"),
        )
        .unwrap();
        let raw_step = workflow
            .split_once("  - searchRaw:")
            .and_then(|(_, rest)| rest.split_once("  - projectHits:").map(|(step, _)| step))
            .expect("lookback recall must contain distinct raw-search and projection steps");

        assert!(raw_step.contains("memory_kinds: [\"MEMORY_KIND_RAW\"]"));
        assert!(
            !raw_step.contains("thread_filter:"),
            "RAW recall must not expand every matching thread into memory IDs"
        );
    }

    #[test]
    fn lookback_summary_recall_expands_threads_only_for_explicit_labels() {
        let workflow = std::fs::read_to_string(
            workers_bundle_dir()
                .unwrap()
                .join("workflows/rag/lookback-recall.yaml"),
        )
        .unwrap();
        let summary_step = workflow
            .split_once("  - searchSummaries:")
            .and_then(|(_, rest)| rest.split_once("  - searchRaw:").map(|(step, _)| step))
            .expect("lookback recall must contain distinct summary and raw-search steps");

        assert!(summary_step.contains("memory_kinds: [\"MEMORY_KIND_THREAD_SUMMARY\""));
        assert!(
            summary_step.contains("if $has then {"),
            "only an explicit summary_labels request may add a thread filter"
        );
        assert!(
            summary_step.contains("thread_filter:"),
            "explicit summary labels must remain searchable through thread labels"
        );
    }

    /// The lang-worker single YAMLs (day/week/month summary) live outside
    /// `workers/workflows/`, so `workflow_yamls_parse` never scans them —
    /// yet they carry the same UTC/TZ jq boundary logic (memories 5e996f5,
    /// `env.TZ`-vs-`timezone_offset_hours`). A stray quote in that jq would
    /// only surface on the first summary dispatch. Reparse them here too.
    #[test]
    fn lang_worker_yamls_parse() {
        unsafe { std::env::remove_var("LOOKBACK_WORKERS_DIR") };
        let lang_workers_dir = workers_bundle_dir().unwrap().join("lang-workers");
        let mut bad = Vec::new();
        let mut count = 0usize;
        for entry in walkdir(&lang_workers_dir) {
            if entry.extension().and_then(|s| s.to_str()) != Some("yaml") {
                continue;
            }
            count += 1;
            let body = std::fs::read_to_string(&entry).unwrap();
            let expanded = expand_env_defaults(&body);
            if let Err(e) = serde_yaml::from_str::<serde_yaml::Value>(&expanded) {
                bad.push(format!(
                    "{}: {e}",
                    entry.strip_prefix(&lang_workers_dir).unwrap().display()
                ));
            }
        }
        assert!(count > 0, "no lang-worker YAMLs scanned (path wrong?)");
        assert!(
            bad.is_empty(),
            "lang-worker YAML parse failures:\n  - {}",
            bad.join("\n  - ")
        );
    }

    #[test]
    fn llm_worker_file_includes_exist_in_workers_bundle() {
        unsafe { std::env::remove_var("LOOKBACK_WORKERS_DIR") };
        let workers_dir = workers_bundle_dir().unwrap();
        let yaml_path = workers_dir.join("llm-workers.yaml");
        let body = std::fs::read_to_string(&yaml_path).unwrap();
        let expanded = expand_env_defaults(&body);
        let doc: serde_yaml::Value = serde_yaml::from_str(&expanded).unwrap();
        let mut includes = Vec::new();
        collect_file_includes(&doc, &mut includes);

        assert!(
            !includes.is_empty(),
            "llm-workers.yaml has no $file includes"
        );

        let missing: Vec<_> = includes
            .iter()
            .filter(|rel| !workers_dir.join(rel).exists())
            .cloned()
            .collect();
        assert!(
            missing.is_empty(),
            "llm-workers.yaml references missing $file include(s): {}",
            missing.join(", ")
        );
    }

    #[test]
    fn language_generation_worker_assets_are_bundled() {
        unsafe { std::env::remove_var("LOOKBACK_WORKERS_DIR") };
        let root = lang_workers_repo_root().unwrap();
        for (feature, single_name) in [
            ("thread-summary", "thread-summary-single.yaml"),
            ("daily-work-summary", "daily-work-summary-single.yaml"),
            ("weekly-work-summary", "weekly-work-summary-single.yaml"),
            ("monthly-work-summary", "monthly-work-summary-single.yaml"),
            ("personality", "thread-personality-single.yaml"),
            ("thread-reflection", "thread-reflection-single.yaml"),
        ] {
            let feature_dir = root.join("workers").join(feature);
            let single = feature_dir.join(single_name);
            assert!(
                single.exists(),
                "missing language generation single YAML: {}",
                single.display()
            );

            let prompts = feature_dir.join("prompts");
            assert!(
                prompts.is_dir(),
                "missing language generation prompt dir: {}",
                prompts.display()
            );
            for lang in ["ja", "en"] {
                assert!(
                    prompt_for_language_exists(&prompts, lang),
                    "missing {lang} prompt file under {}",
                    prompts.display()
                );
            }
        }

        let merge = root
            .join("workers")
            .join("personality")
            .join("user-personality-merge.yaml");
        assert!(
            merge.exists(),
            "missing language generation merge YAML: {}",
            merge.display()
        );
    }

    #[test]
    fn personality_merge_timeouts_allow_slow_local_llm() {
        unsafe { std::env::remove_var("LOOKBACK_WORKERS_DIR") };

        let merge_path = lang_workers_repo_root()
            .unwrap()
            .join("workers/personality/user-personality-merge.yaml");
        let merge_body = std::fs::read_to_string(&merge_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", merge_path.display()));
        assert_yaml_timeout_hours(&merge_body, 6, "user-personality-merge top-level timeout");
        assert_eq!(
            timeout_hours_after_marker(&merge_body, "  - mergeProfile:"),
            Some(3),
            "user-personality-merge inner memories-llm timeout"
        );

        let batch_path = workers_bundle_dir()
            .unwrap()
            .join("workflows/personality/thread-personality-batch.yaml");
        let batch_body = std::fs::read_to_string(&batch_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", batch_path.display()));
        assert_eq!(
            timeout_hours_after_marker(&batch_body, "  - userPersonalityMerge:"),
            Some(3),
            "thread-personality-batch userPersonalityMerge timeout"
        );

        let pipeline_path = workers_bundle_dir()
            .unwrap()
            .join("workflows/agent-chat-pipeline/agent-chat-pipeline.yaml");
        let pipeline_body = std::fs::read_to_string(&pipeline_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", pipeline_path.display()));
        assert_eq!(
            timeout_hours_after_marker(&pipeline_body, "                - userPersonalityMerge:"),
            Some(3),
            "agent-chat-pipeline userPersonalityMerge timeout"
        );
    }

    #[test]
    fn personality_merge_context_budget_stays_below_llm_context_limit() {
        unsafe { std::env::remove_var("LOOKBACK_WORKERS_DIR") };

        let merge_path = lang_workers_repo_root()
            .unwrap()
            .join("workers/personality/user-personality-merge.yaml");
        let merge_body = std::fs::read_to_string(&merge_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", merge_path.display()));
        assert!(
            merge_body.contains("max_context_chars: { type: integer, default: 150000 }"),
            "user-personality-merge should default to a 150k signal JSON budget"
        );
        assert!(
            merge_body.contains("default: 100"),
            "user-personality-merge should default to 100 LLM-visible signals"
        );

        let batch_path = workers_bundle_dir()
            .unwrap()
            .join("workflows/personality/thread-personality-batch.yaml");
        let batch_body = std::fs::read_to_string(&batch_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", batch_path.display()));
        assert!(
            batch_body.contains("max_context_chars: { type: integer, default: 150000 }"),
            "thread-personality-batch should pass a 150k context budget by default"
        );
        assert!(
            batch_body
                .contains("target_signal_count:\n          type: integer\n          default: 100"),
            "thread-personality-batch should default to 100 merge signals"
        );

        let periodic_path = workers_bundle_dir()
            .unwrap()
            .join("workflows/lookback-periodic-run.yaml");
        let periodic_body = std::fs::read_to_string(&periodic_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", periodic_path.display()));
        assert!(
            periodic_body.contains("max_context_chars: 150000"),
            "periodic personality generation should use the same 150k context budget"
        );

        let pipeline_path = workers_bundle_dir()
            .unwrap()
            .join("workflows/agent-chat-pipeline/agent-chat-pipeline.yaml");
        let pipeline_body = std::fs::read_to_string(&pipeline_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", pipeline_path.display()));
        assert!(
            pipeline_body.contains("max_signals:\n          type: integer\n          default: 100"),
            "agent-chat-pipeline should default to 100 merge signals"
        );
    }

    #[test]
    fn thread_reflection_context_budget_stays_below_default_llm_context_limit() {
        unsafe { std::env::remove_var("LOOKBACK_WORKERS_DIR") };
        const DEFAULT_LLM_CTX_TOKENS: u32 = 131_072;
        const REFLECTION_MAX_OUTPUT_TOKENS: u32 = 20_000;
        const DEFAULT_REFLECTION_INPUT_TOKENS: u32 = 110_000;
        const DEFAULT_REFLECTION_THRESHOLD_TOKENS: u32 = 100_000;
        const DEFAULT_REFLECTION_MERGE_TOKENS: u32 = 90_000;

        const {
            assert!(
                DEFAULT_REFLECTION_INPUT_TOKENS + REFLECTION_MAX_OUTPUT_TOKENS
                    <= DEFAULT_LLM_CTX_TOKENS,
                "reflection input + output budget must fit the default local LLM ctx"
            );
        }

        let batch_path = workers_bundle_dir()
            .unwrap()
            .join("workflows/thread-reflection/thread-reflection-batch.yaml");
        let batch_body = std::fs::read_to_string(&batch_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", batch_path.display()));
        let single_path = lang_workers_repo_root()
            .unwrap()
            .join("workers/thread-reflection/thread-reflection-single.yaml");
        let single_body = std::fs::read_to_string(&single_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", single_path.display()));

        for (name, body) in [
            ("thread-reflection-batch", batch_body),
            ("thread-reflection-single", single_body),
        ] {
            assert!(
                body.contains(&format!(
                    "context_limit_tokens: {{ type: integer, default: {DEFAULT_REFLECTION_INPUT_TOKENS} }}"
                )),
                "{name} should cap default reflection input below the default LLM ctx"
            );
            assert!(
                body.contains(&format!(
                    "single_pass_threshold_tokens: {{ type: integer, default: {DEFAULT_REFLECTION_THRESHOLD_TOKENS} }}"
                )),
                "{name} threshold should stay below the input cap"
            );
            assert!(
                body.contains(&format!(
                    "merge_max_input_tokens: {{ type: integer, default: {DEFAULT_REFLECTION_MERGE_TOKENS} }}"
                )),
                "{name} merge budget should stay below the input cap"
            );
        }
    }

    #[test]
    fn thread_reflection_timeouts_allow_slow_local_llm() {
        unsafe { std::env::remove_var("LOOKBACK_WORKERS_DIR") };

        let batch_path = workers_bundle_dir()
            .unwrap()
            .join("workflows/thread-reflection/thread-reflection-batch.yaml");
        let batch_body = std::fs::read_to_string(&batch_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", batch_path.display()));
        assert_yaml_timeout_hours(&batch_body, 24, "thread-reflection-batch top-level timeout");
        assert_eq!(
            timeout_hours_after_marker(&batch_body, "  - reflectEach:"),
            Some(24),
            "thread-reflection-batch fan-out timeout"
        );
        assert_eq!(
            timeout_after_marker(&batch_body, "              - invokeSingle:"),
            Some(TimeoutAfter::Hours(3)),
            "thread-reflection-batch invokeSingle timeout"
        );
    }

    #[test]
    fn thread_personality_filters_non_conversation_scaffolding() {
        unsafe { std::env::remove_var("LOOKBACK_WORKERS_DIR") };
        // The single moved under lang-workers (it's registered as a language
        // worker now); the scaffolding-filter logic lives in the same YAML.
        let path = lang_workers_repo_root()
            .unwrap()
            .join("workers/personality/thread-personality-single.yaml");
        let body = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

        assert!(
            body.contains("visible_messages:"),
            "thread-personality-single must distinguish raw fetched messages from visible conversation messages"
        );
        for marker in [
            "# AGENTS.md instructions",
            "# CLAUDE.md instructions",
            "<environment_context>",
            "<permissions instructions>",
            "<turn_aborted>",
        ] {
            assert!(
                body.contains(marker),
                "personality extraction must filter Codex-injected user scaffolding marker {marker:?}"
            );
        }
        // AGENTS/CLAUDE heading-only matches would also catch user questions
        // that open with the same markdown heading, so the filter must require
        // the co-occurring <INSTRUCTIONS> block — mirrors the TS side in
        // src/lib/codexMemoryVisibility.ts.
        assert!(
            body.contains("contains(\"<INSTRUCTIONS>\")"),
            "AGENTS/CLAUDE.md heading filter must AND with the <INSTRUCTIONS> block to avoid hiding user questions"
        );
        assert!(
            body.contains("payload_type == \"agent_message\""),
            "personality extraction must filter Codex assistant event shadows"
        );
        assert!(
            body.contains("($visible_messages | length)"),
            "min-message gating must count only visible conversation messages"
        );
        assert!(
            body.contains("$visible_messages | map(select(.data.role == \"ROLE_USER\"))"),
            "min-user gating must count only visible user messages"
        );
        assert!(
            body.contains("user_messages: >-\n          ${ $messages | map(select(.data.role == \"ROLE_USER\")) }"),
            "truncation must be computed from the filtered message set"
        );
    }

    /// Test-only stand-in for jobworkerp-client's `expand_env`: replace each
    /// `%{VAR:-default}` with its default and each bare `%{VAR}` with empty
    /// (the env is intentionally not consulted — the committed file must parse
    /// to valid YAML on its DEFAULTS alone). Sufficient to validate that the
    /// post-substitution document the runtime parses is well-formed; it is NOT
    /// the production expander.
    fn expand_env_defaults(input: &str) -> String {
        let mut out = String::with_capacity(input.len());
        let mut rest = input;
        while let Some(start) = rest.find("%{") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let Some(end) = after.find('}') else {
                // Unterminated `%{` — leave it verbatim and stop scanning.
                out.push_str(&rest[start..]);
                return out;
            };
            let token = &after[..end];
            let value = token.split_once(":-").map(|(_, def)| def).unwrap_or("");
            out.push_str(value);
            rest = &after[end + 1..];
        }
        out.push_str(rest);
        out
    }

    #[test]
    fn expand_env_defaults_substitutes_default_and_empty() {
        assert_eq!(expand_env_defaults("port: %{P:-9010}"), "port: 9010");
        assert_eq!(expand_env_defaults("tls: %{T:-false}"), "tls: false");
        assert_eq!(expand_env_defaults("x: %{NODEFAULT}"), "x: ");
        assert_eq!(expand_env_defaults("plain text"), "plain text");
        // Quoted placeholders (the common case) expand inside the quotes.
        assert_eq!(
            expand_env_defaults("host: \"%{H:-127.0.0.1}\""),
            "host: \"127.0.0.1\""
        );
    }

    fn collect_file_includes(value: &serde_yaml::Value, out: &mut Vec<String>) {
        match value {
            serde_yaml::Value::Mapping(map) => {
                for (key, value) in map {
                    if key.as_str() == Some("$file")
                        && let Some(path) = value.as_str()
                    {
                        out.push(path.to_string());
                    }
                    collect_file_includes(value, out);
                }
            }
            serde_yaml::Value::Sequence(items) => {
                for item in items {
                    collect_file_includes(item, out);
                }
            }
            _ => {}
        }
    }

    fn assert_yaml_timeout_hours(body: &str, expected_hours: i64, context: &str) {
        let yaml: serde_yaml::Value = serde_yaml::from_str(body).unwrap_or_else(|e| {
            panic!("{context}: YAML parse failed: {e}");
        });
        let hours = yaml
            .get("timeout")
            .and_then(|v| v.get("after"))
            .and_then(|v| v.get("hours"))
            .and_then(|v| v.as_i64());
        assert_eq!(hours, Some(expected_hours), "{context}");
    }

    fn timeout_hours_after_marker(body: &str, marker: &str) -> Option<i64> {
        match timeout_after_marker(body, marker)? {
            TimeoutAfter::Hours(hours) => Some(hours),
            TimeoutAfter::Minutes(_) => None,
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum TimeoutAfter {
        Hours(i64),
        Minutes(i64),
    }

    fn timeout_after_marker(body: &str, marker: &str) -> Option<TimeoutAfter> {
        let start = body.find(marker)?;
        let mut after_timeout = false;
        let mut after_after = false;
        for line in body[start + marker.len()..].lines() {
            let trimmed = line.trim();
            if trimmed == "timeout:" {
                after_timeout = true;
                after_after = false;
                continue;
            }
            if after_timeout && trimmed == "after:" {
                after_after = true;
                continue;
            }
            if after_timeout && after_after {
                if let Some(raw) = trimmed.strip_prefix("hours:") {
                    return raw.trim().parse().ok().map(TimeoutAfter::Hours);
                }
                if let Some(raw) = trimmed.strip_prefix("minutes:") {
                    return raw.trim().parse().ok().map(TimeoutAfter::Minutes);
                }
                if !trimmed.is_empty() && !trimmed.starts_with('#') {
                    return None;
                }
            }
        }
        None
    }

    fn prompt_for_language_exists(prompts: &std::path::Path, lang: &str) -> bool {
        let suffix = format!(".{lang}.txt");
        let Ok(rd) = std::fs::read_dir(prompts) else {
            return false;
        };
        rd.flatten()
            .any(|entry| entry.file_name().to_string_lossy().ends_with(&suffix))
    }

    fn walkdir(root: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(p) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&p) else {
                continue;
            };
            for entry in rd.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    out.push(path);
                }
            }
        }
        out
    }

    #[test]
    fn workers_bundle_dir_honors_env_override() {
        let tmp = tempdir_in_target();
        unsafe { std::env::set_var("LOOKBACK_WORKERS_DIR", &tmp) };
        let dir = workers_bundle_dir().unwrap();
        assert_eq!(dir, tmp.path());
        unsafe { std::env::remove_var("LOOKBACK_WORKERS_DIR") };
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn workers_bundle_dir_errors_on_missing_env_path() {
        unsafe { std::env::set_var("LOOKBACK_WORKERS_DIR", "/tmp/lookback-nonexistent-xxx") };
        let err = workers_bundle_dir().unwrap_err();
        unsafe { std::env::remove_var("LOOKBACK_WORKERS_DIR") };
        assert!(matches!(err, AppError::Config(_)));
    }

    /// Drop the trailing `# ...` comment from a YAML line, treating a `#`
    /// as a comment opener only at line-start or after whitespace. A `#`
    /// embedded in a token (quoted jq string, fragment selector) is kept.
    fn strip_yaml_comment(line: &str) -> &str {
        let bytes = line.as_bytes();
        for (i, &b) in bytes.iter().enumerate() {
            if b == b'#' && (i == 0 || bytes[i - 1].is_ascii_whitespace()) {
                return &line[..i];
            }
        }
        line
    }

    struct TestTempDir(tempfile::TempDir);

    impl TestTempDir {
        fn path(&self) -> &Path {
            self.0.path()
        }
    }

    impl std::ops::Deref for TestTempDir {
        type Target = Path;

        fn deref(&self) -> &Self::Target {
            self.path()
        }
    }

    impl AsRef<Path> for TestTempDir {
        fn as_ref(&self) -> &Path {
            self.path()
        }
    }

    impl AsRef<std::ffi::OsStr> for TestTempDir {
        fn as_ref(&self) -> &std::ffi::OsStr {
            self.path().as_ref()
        }
    }

    fn tempdir_in_target() -> TestTempDir {
        TestTempDir(
            tempfile::Builder::new()
                .prefix("lookback-test-")
                .tempdir_in(std::env::temp_dir())
                .unwrap(),
        )
    }
}
