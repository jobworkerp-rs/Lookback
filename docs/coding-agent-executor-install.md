# Coding-agent executor artifact procedure

This document describes the current **build and local verification** procedure
for a Linux/KVM runtime artifact and its separate `coding-agent-executor`
helper. It is not a claim that coding-agent execution or production installation
is ready. The macOS UI artifact and the Linux/KVM executor are separate
deliverables; do not add the Linux helper to the Tauri external-binary list.

The executor helper below is an existing implementation option, not a required
architecture for all coding-agent workflows. Issue publication can run as
workflow steps using the configured local-only, branch-push, or pull-request
mode; it does not require this separate host publisher or sealed-artifact
helper. The current app still contains helper-specific activation gates, so
this specification change does not make those paths available automatically.

## Current activation status

The manifest proves the selected source pins, built artifact hashes, helper
version/protocol/capabilities, and worker/workflow configuration hashes. It is
unsigned, so those hashes establish integrity only relative to that manifest;
they do not authenticate who produced it. The verifier runs the helper's
fixed `--version` and `--capabilities` queries with a three-second deadline and
a 64 KiB output cap. It kills the helper process group on timeout, overflow, or
completion; Windows verification fails closed because equivalent process-group
cleanup is not provided there. Run it only on an artifact
bundle built from source you trust, not on an untrusted manifest or binary.

VM concurrency and pool sizing use the existing jobworkerp worker and pool
configuration. This procedure introduces no separate app-managed VM quota
ledger, counter, semaphore, or quota-capability attestation. It does not claim
an independent aggregate VM quota.

The installed runtime does not currently expose a verified protocol- and
configuration-version probe for this artifact gate. Those manifest fields
remain unverified: `--provenance-only` can validate artifact provenance, but a
normal verification still exits non-zero and reports `enable_ready: false`.
Do not enable coding-agent execution until the runtime owner supplies an actual
protocol/configuration compatibility handshake and the verifier checks it.

## Executor protocol v2 contract

The manifest pins the executor protocol to v2. This is the helper wire protocol,
not an attestation of the installed jobworkerp runtime; its
`runtime_compatibility.protocol_version` and `config_version` remain `null`.
The verifier compares the helper's actual `--version` and `--capabilities`
responses with the manifest and rejects protocol-v1 artifacts. The required capability is `prepare`. Source inspection, base resolution, and
publication are checked separately when used. The legacy `capture`, `seal`,
`resolve`, and `artifact_only_publication` capabilities are optional and are
not prerequisites for preparing a workspace. Legacy artifact publication is optional: `push` is paired with
`secret_scan_supported_formats_v1`, and `pull_request` requires `push`.

`push_workspace` is an optional operation for publishing a prepared workspace
commit without Capture/Seal receipts. The app checks this capability separately
and invokes the operation through a registered workflow; a missing workflow does
not fall back to a direct helper invocation. Automatic Issue publication also
requires an approved push/PR plan and matching repository publication policy.
PR creation follows a successful push through a registered workflow. Publication
results are included in the Issue report before the run reaches review.
When the observed source has no changes, the app skips push and PR creation and
saves that outcome with the Issue report. Cancelling before enqueue prevents the
next remote phase; once enqueue begins, the app collects its actual job ID so it
can cancel the accepted job. Unconfirmed remote results require reconciliation.
Helpers advertising `lookup_publication` can check existing remote branches and
PRs without pushing or creating a PR. Missing remote results remain unconfirmed.
This helper support does not establish application recovery after a restart.

`resolve_base` is an additional optional capability. It resolves a configured,
allowed base branch to a full commit ID without preparing or checking out a
workspace. Helpers without this capability remain compatible with the existing
protocol-v2 operations, but cannot serve base-resolution requests. The cached
request is compared in full: within a run, the same operation ID returns the
original result even if the branch moves; a new operation ID performs a fresh
query. Operation IDs are scoped to the run, not globally unique. Persist the
receipt and pass its unchanged commit ID to Prepare; do not re-resolve the branch
to replace an already selected session base.

## Native interactive host policy

The native bootstrap, app commands, and separate Issue-detail terminal panel are
connected in source. The integrated path is still undergoing review and has not
passed real SDK/VM/TUI, authentication, or long-duration acceptance tests.
Installing these files alone does **not** establish execution readiness. Missing
packages, policy, or compatible observed runtime sources keep the feature
unavailable.

The app selects its resource directory natively. The package belongs under
`<resource-directory>/coding-agent/`, with
`coding-agent-artifact-manifest.json` and the artifact paths declared in it.
Use a reviewed package from the trusted app installation. Hashes detect changed
bytes. Tauri maps the repository's `resources/coding-agent/` directory to this
resource path; that mapping does not build or download a package. The checked-in
directory provides only the schema; a release package is produced by
`scripts/build-coding-agent-executor.sh`, and `pnpm tauri dev` stages a
development package into the cargo target directory (see "Development
staging" below). Do not stage host policies or credentials in this bundled
resource directory. A
user-selected manifest and matching binary are not independent proof of
authenticity or an attestation. Package files and directories must not be
group/world writable or symlinks.

The protected legacy policy is `<AppData>/coding-agent/native/trusted-host.json`.
Its containing app-owned directories must be private (`0700`), and the policy
must be an owner-owned regular file (`0600`), not a symlink. Setup does not require
sudo or scanning the user's home directory. Missing or unsafe setup disables
this feature rather than requiring the whole app to fail startup.

This file remains the legacy trusted-policy fallback and the protected source for
the package identity and credential metadata used by the native bootstrap. The
current Settings UI does not generate or ask you to hand-edit `trusted-host.json`.
It stores repository approvals, clone transport settings, and explicitly selected
SSH directories in the versioned Coding agent settings document instead.

For a new setup where the legacy file is absent, the verified package can be
approved from the Settings diagnostic. The backend records its fingerprint and
builds a safe empty legacy baseline before applying the GUI repository approvals,
branches, and SSH settings; no hand-written JSON or pre-created trusted-host file
is required. If a legacy file is present, its protected package and credential
validation remains authoritative for that fallback path.

The legacy policy version 1 has these fields:

- `version`: `1`.
- `max_import_bytes`: a positive artifact-import byte limit.
- `expected_package_identity`: exactly the approved manifest's `profile`,
  `target`, `sources`, and `executor` objects. Review those pins before approving
  them; do not automatically accept a changed manifest.
- `repositories`: entries containing `id` (`host`, `owner`, `name`), an exact
  matching `clone_url` (with optional `.git` suffix equivalence),
  `allowed_base_branches`, and `read_credential_source`
  (`null` for explicitly anonymous reads). Optional `publish_credential_source`
  selects a publication token; optional `forge` contains `kind` (`github` or
  `gitea`), `api_base_url`, and `web_base_url` for PR creation.
- `credential_sources`: entries containing `id`, `username`, and `token_file`.
  The token path is relative to
  `<AppData>/coding-agent/native/credential-sources/`; traversal, symlinks, unsafe
  permissions, and missing references are rejected. Token files are private
  (`0600`) in private (`0700`) directories. Authentication failures do not trigger
  an anonymous fallback. Bootstrap snapshots only explicitly referenced read and
  publication credentials; a shared reference is copied once. Omitting the
  publication fields preserves the existing preparation setup. Select
  `push_branch` or `create_pr` in the Issue run settings to request publication;
  `local_only` retains the commit in the local workspace. PR creation requires
  the configured forge and publication token. Each remote phase checks current
  Issue ownership before dispatch, and an unknown response is not resubmitted.
  The forge kind and read/publication credential reference IDs must match the
  repository host settings approved for the run.
  This does not prohibit explicitly selected agent settings or SSH mounts.

For legacy policy entries, `ssh_directory` identifies the SSH configuration
directory. For current GUI settings, choose that directory in the repository-host
editor instead; the GUI saves it as the SSH clone transport's `sshDirectory`.
SSH username, hostname, and port come from the selected clone transport;
they may differ from the repository label's forge/API host. The native policy
must agree with the clone transport selected in Settings.
When configuring a new SSH directory, use an absolute, existing directory that
you explicitly selected, such as `/home/your-user/.ssh`; it must be canonical,
non-symlink, owned by the current user, and have no group/other permission bits
(use mode `0700`, owner read/write/execute only).
Leave `read_credential_source` null when no HTTPS token is needed. The helper
uses this directory's `config`, `known_hosts`, and standard identity files,
with batch authentication and strict host-key checking. It does not inherit
an SSH agent socket or automatically discover credentials from the host home.
Paths containing quotes, backslashes, `%`, `$`, or control characters are rejected because
OpenSSH interprets them instead of preserving the selected path literally.

For the GUI, clearing the selected SSH directory only clears the GUI value. If
the matching legacy policy entry has an existing SSH directory, the native
backend may continue using that inherited path; otherwise SSH configuration is
unavailable until a valid directory is selected.

In the Coding agent Settings repository-host editor, HTTPS read authentication and
host API/publication authentication are entered as opaque setting IDs. Each ID must
match a `credential_sources[].id` entry in `trusted-host.json`; the UI never accepts
token bytes. When SSH is selected, the HTTPS read-token ID is hidden but retained,
while the host API/publication ID remains available for the separate API or
publication path. The editor provides an explicit SSH directory picker (or an
absolute path input) and never registers keys or accepts private-key contents.

For either GUI credential reference, an empty value means unspecified. If the
matching repository in the legacy trusted policy already has a reference, the
native backend inherits it; otherwise reads are anonymous and publication is
unconfigured. An explicitly entered GUI reference takes precedence. Clearing the
input only clears the GUI value and does not guarantee removal of an inherited
credential; use the existing trusted-policy administration path when an inherited
credential must be removed.

1. Select SSH as the clone transport and choose the directory containing the
   intended `config`, `known_hosts`, and identity files. The picker keeps the
   selected absolute path; it does not expand `~` or scan the home directory.
   If the GUI directory is left unspecified, an existing path for the matching
   repository in the legacy trusted policy may be inherited; otherwise no SSH
   configuration directory is available.
2. Add or edit the native repository approvals for the exact repository and its
   allowed base branches. This list is separate from the profile repository
   allowlist; an explicit empty list denies every native repository.
3. Add the server key to the selected directory's `known_hosts` through your usual
   SSH setup and ensure batch authentication works with the selected keys. SSH
   agent sockets and interactive passphrases are not supported.
4. Confirm the host dialog, save the settings with the common footer, and run the
   native setup diagnostic. The diagnostic reports package availability and the
   current settings revision. Use its explicit approval action only when the
   package identity matches; that action records the package fingerprint in a
   protected settings field through a dedicated atomic compare-and-swap. Editing
   the ordinary settings payload cannot approve or replace that package baseline.

When `nativeRepositoryApprovals` is omitted, the native backend keeps the legacy
trusted-policy repository list as its effective approval source. An explicit empty
list denies every native repository. The GUI approval list is global to the
settings document, separate from each profile's repository allowlist, and is
saved through the ordinary settings compare-and-swap.

The profile editor also offers `App-private settings` and `Share host settings`.
Host sharing accepts only explicitly selected absolute Codex/Claude directories and
an optional Claude settings file as an import source. It mounts the selected
directories read/write for the setup shell and normal runs, so directory-backed
authentication updates are reflected on the host. On first use or when the file
source path changes, the backend privately copies that file into the shared
app-owned home; later CLI updates stay there and are never written back to the
source. Clearing the source stops future imports but does not delete data already
imported. OS credential stores such as the Codex keyring or macOS Keychain and SSH
agents are not shared automatically; use the authentication/settings shell when
login is needed, and do not assume that selecting a directory alone authenticates
every CLI. Host-shared profiles use their selected directories and shared home
directly; dedicated-home preparation is unavailable and unnecessary. Existing user
persistent mounts remain intact when switching modes; the app does not discover or
share the whole host home automatically.

Host sharing weakens the sandbox boundary. Because the selected directories are
writable from the VM, an agent hijacked by prompt injection can leave binaries
or settings there (MCP server commands, hooks, notify commands, managed CLI
packages) that the host CLI runs automatically the next time it is used on the
host, with no user decision in between. The Settings UI shows this risk next to
`CLI settings storage`; use host sharing only when that risk is acceptable and
prefer `App-private settings` otherwise.

For HTTPS publication and PR API credentials:

1. Store the selected token in a private file under `credential-sources` and
   add its `id`, `username`, and relative `token_file` to `credential_sources`.
2. Set `publish_credential_source` on the selected repository to that ID. It may
   share the read credential ID when the selected token has both permissions.
3. For PR creation, set the repository's `forge` API and web endpoints and kind.
   SSH Git transport can be paired with this separately selected API token.
4. Use the same opaque credential reference in the repository host settings and
   prepare a fresh run after changes. Keep token bytes out of settings JSON,
   Issue reports, and prompts.

Profile network access is selected in Settings. Existing profiles without a
network selection retain disabled networking. New profiles show Public access
for internet providers; private-network and host access require explicit
selection. The selected policy and finite connection limits are frozen into
the prepared worker configuration.

1. Select the agent profile in Settings and choose the network scope required
   by its provider and configured MCP services.
2. Select Public for internet providers. Add private-network or host access
   only when those services require it, or select Disabled for offline use.
3. Save the profile, then prepare a fresh workspace/session. An existing
   prepared or started context keeps its original policy.

This wiring is verified for interactive preparation. The automatic Issue
entry point and end-to-end provider authentication are still under implementation.

Bootstrap generates repository-specific immutable helper configs and private
workspace, publisher, state, credentials, session, and output directories below
`coding-agent/native/repositories/<repository-identity-hash>/`. Only that
repository's workspace directory and explicitly approved profile mounts may be
exposed to the guest—not the parent directory, policy, credential, config, or
output directories. Credential rotation creates a new immutable snapshot and
does not overwrite the snapshot referenced by an existing configuration.

Core-only protocol-v2 packages can configure a workspace executor. Base
resolution still requires the optional `resolve_base` capability; unavailable
publication capabilities are rejected before dispatch. Successful configuration
and helper probing remain `NotObserved` for SDK/VM/TUI readiness.
A CLI version probe requires the expected version output, exit 0, and a complete
error-free result stream. Bounded stderr warnings alone do not invalidate the
probe; their presence is recorded without persisting raw stderr.

Prepare creates the attempt workspace at the configured workspace root under
`<run_id>/<attempt_id>`. Normal `push_workspace` publishes the observed clean
workspace commit and rechecks source and remote state before mutation.

In the optional legacy artifact path, Capture, Seal, and Resolve identify that
same attempt; legacy Push and Pull Request require its resolved artifact digest
and host-generated commit. That path publishes the sealed copy and must not
substitute a mutable workspace or another attempt's artifact. Both PR paths
require a successful push receipt. The `host_isolated_after_seal` result means the sealed copy
is independent of later workspace writes; it does not prove that the guest
writer stopped, that runtime assets were validated, or that production
activation is safe. Normal one-shot completion uses the actual CLI exit, clean
stream end, and saved results; it does not require physical VM termination or a
generation-bound stop proof. Artifact/source consistency, runtime compatibility,
and configured static-worker behavior still require their own validation.

Prepare rejects approved base trees containing symlinks (`120000`) or gitlinks
(`160000`) before creating a workspace. It does not flatten or convert these
entries, and passing artifact provenance does not mean arbitrary repositories
are supported.

An interrupted Capture may leave capture data without a completed receipt; the
same attempt then fails closed and is not automatically recovered. Preserve
that state for diagnosis. Do not remove a sealed artifact or receipt to force a
retry: later Push/PR reconciliation uses the same run, attempt, and artifact.
A new attempt requires explicit human approval, confirmation that the previous
execution is terminal, and permission from the native coordinator. An unknown
writer or outcome is not success.

## Settings setup diagnostic

**Settings → Coding agent** shows the native backend as a three-step checklist.
Every state names the next action; no step requires editing a file by hand.

1. **Execution package.** Reports whether the bundled package was verified. When
   it is unavailable the reason is specific: `package_missing` (reinstall the
   app, or check the `pnpm tauri dev` staging output in development),
   `unsupported_platform` (Linux KVM build only), `trusted_policy_unsafe` /
   `trusted_policy_invalid` (application-data policy or repository host settings
   cannot be read), `storage_unavailable`, `executor_unavailable`, or
   `settings_unavailable`. A package staged from a working tree is labelled as a
   development package; release builds reject it. This step re-reads only the
   manifest; it does not re-hash artifacts (see "When package bytes are
   verified" below), so a replaced or removed package is reported here while
   changed artifact bytes are reported by the action that would use them.
2. **Native backend approval.** The step explains what approval means, and the
   **Approve native backend** action opens a confirmation that names the target
   (executor version and manifest fingerprint) and lists what approval records
   and enables: only the fingerprint is written to the settings document through
   the dedicated compare-and-swap; nothing is executed, fetched, or read at that
   moment. After approval the app may, when an Issue is run, use this package's
   executor to check approved repositories out into a private host workspace and
   start the agent CLI inside the bundled runtime's sandbox VM, publish a branch
   or pull request when selected for an Issue, and run the policy/Git stages of
   the repository host check. Nothing starts automatically, repositories outside
   the approval list stay rejected, and the authentication shell does not depend
   on it. `not_approved` and `reapproval_required` (the package changed since it
   was approved) both enable the action; an approved package is shown as
   completed with its version and fingerprint and without the approve action. `package_mismatch` means the protected legacy
   `trusted-host.json` pins a different package identity; this cannot be
   repaired from Settings. The approve action also waits for a clean, saved
   settings draft and a current diagnostic revision.
3. **Approved repositories (per-host approval list).** This count is the global
   `nativeRepositoryApprovals` list, not any profile's allowed repositories.
   "Not configured" means no GUI list has been saved and the legacy policy (or
   its empty virtual baseline) is the authority; an explicit empty list denies
   every repository. Repositories that a profile allows but the list does not
   cover are listed with **Add … to the approval list**, which appends them to
   the draft with `main` as the only allowed base branch; confirm with the page
   footer **Save and apply** and adjust branches through **Edit host …**, which
   opens the repository host editor. A repository whose host is not registered
   shows a hint to add the host first.

### When package bytes are verified

Artifact hashes are checked when the package is selected at app startup, when
the native backend is approved, before every executor invocation, when the
running sidecar's `/proc/<pid>/exe` is compared with the manifest runtime
digest, and in every policy read that prepares, starts, checks, or publishes.
The Settings diagnostic is display-only and skips the artifact re-hash; the
repository host check likewise leaves it to the step that executes the check.

These checks establish consistency with the selected manifest. They do not
authenticate the package: the manifest is unsigned, and anyone able to rewrite
the installed package can also rewrite the app that performs the checks.
Authenticity comes from the installation channel (root-owned deb/rpm files, the
read-only AppImage mount, or macOS code signing), not from these in-app hashes.

The authentication/settings dialog likewise shows whether the profile's
dedicated home is prepared and keeps the shell action disabled until a writable
home mount exists.

## Add a sandbox agent from a preset

For image and CLI updates, follow the [CLI version update procedure](coding-agent-cli-version-policy.md).

The Coding agent Settings page exposes read-only presets for the published
sandbox images. Choose a preset and select **Add profile** to create a new
editable profile draft; selecting a preset alone does not change the active
profile or any saved profile. The same preset can be added more than once, and
each draft receives a new profile ID.

The available presets are:

- OpenAI Codex: `ghcr.io/sutr-app/openai-codex:20261025_0`, command `codex exec`
- Claude Code: `ghcr.io/sutr-app/claude-code:20261025_0`, command `claude -p`
- OpenCode: `ghcr.io/sutr-app/opencode:20261025_0`, command `opencode run`

To configure and verify one:

1. Open **Settings → Coding agent → Execution profiles**, choose a preset from
   **Preset for the new profile**, and select **Add profile**. Choose
   **No preset (empty profile)** when you want to enter the profile manually.
   The profile editor opens; confirm the dialog before the profile appears in
   the overview list.
2. Review and edit the generated display name, image reference, command,
   `Container home`, runtime environment paths, network scope, resources, mounts,
   and repository policy. Presets use `/home/codespace/.agent-home` as their
   container home, and keep image-provided CLI/Rust directories visible with
   `CARGO_HOME=/home/codespace/.cargo` and `RUSTUP_HOME=/home/codespace/.rustup`.
   The Codex preset additionally sets
   `CODEX_HOME=/home/codespace/.agent-home/.codex`. `HOME` and `PATH` are not
   editable runtime environment keys; empty optional values are removed when
   saved. Use the field help buttons by hover or keyboard focus when you need
   the tooltip; pressing Escape dismisses the tooltip without closing the
   profile editor.
3. In the profile editor, choose **Apply changes**. This updates the draft only.
   If the profile editor is closed with unsaved edits, its independent
   confirmation offers
   **Apply changes and close**, **Discard and close**, or **Continue editing**.
   **Apply changes and close** still updates only the page draft; use the
   common page footer **Save and apply** to persist it. Preset values are saved
   as an ordinary profile; the app does not automatically add profiles during
   settings load.
4. From the saved profile row, choose **Authentication and settings**. In
   **App-private settings** mode, if the configured custom `Container home`
   already has a writable home mount, that existing mount can be reused by
   ordinary runs; otherwise choose **Prepare dedicated home** to create the
   persistent private home at the configured container home. In **Share host
   settings** mode, use the explicitly selected host directories directly:
   dedicated-home preparation is unavailable and unnecessary. Home preparation
   is a separate settings update and requires the Coding agent draft to be saved
   and clean.
5. Choose **Open authentication and settings shell** from the same profile
   action. The shell uses the same image and shared home as an ordinary run,
   so saved authentication and settings are reused there. Follow the
   CLI-specific manual guide below. Closing the shell is not treated as proof
   that login or configuration succeeded; close it only after the operation is
   confirmed.
6. Start a one-shot or interactive run with the saved profile and verify that
   the selected command, image, home mount, and repository policy work in the
   target sandbox.

If a home mount would hide files supplied by an image, inspect the image's
startup and configuration layout before adding additional mounts. The preset
home is intentionally separate from `/home/codespace`, so image-installed
executables such as `/home/codespace/.local/bin` remain visible. Keep the
container home writable and avoid mounting over the CLI executable directory.
Image pull, CLI probe, authentication, and run failures remain execution
diagnostics; selecting a preset does not bypass image or runtime validation.

## Check a repository host before running an agent

Each Git service entry in the **Repository hosts** overview has a **Check
settings and connection** action. Edit or add a host in its dialog, apply the
dialog, and save the settings with the common footer before checking. The check
uses the saved host settings only when the expected and observed settings
revisions match; it does not save drafts. A repository can be selected only when
it is registered in an allowed-repository list for that same host; duplicate
entries are shown once.
Leaving the repository unselected performs a structural check only. Add the
repository to the appropriate profile before requesting a repository-aware
check.

The check reports four independent stages:

1. configuration;
2. the effective native repository approval policy;
3. read access through Git; and
4. read access through the configured forge API.

Every stage is reported as passed, failed, or skipped. The result is not a
general “ready”, “authenticated”, or “all checks passed” assertion. A passed
Git read does not prove forge authentication, push, pull-request, or other
write permissions. A successful API read likewise does not verify writes. The
check is read-only and never displays token bytes, SSH paths, HTTP response
bodies, or CLI stderr.

The effective native repository approval policy (the GUI list, or the legacy
policy when the GUI list is omitted) must match before credential or network
checks can be treated as configured. An unset credential, an unavailable
credential, a missing repository registration, or an older executor package is
reported as unconfirmed or unsupported; these states are not inferred to be
authenticated.
After editing a host, changing the selected repository, saving settings, or
changing the settings revision, run the check again because the previous result
is no longer applicable. A 403 means that access is restricted or unavailable
to the current caller, while a 404 means that the target was not found or is
not referenceable; neither response is treated as proof of a specific token or
repository error.

## Prerequisites

- Use only already-present, locally reviewed source checkouts. The scripts do
  not fetch or update Git sources, install Rust/toolchains, read `.env` files,
  or use the user's Cargo/Rustup home.
- The agent-app checkout and the selected jobworkerp runtime checkout must be
  clean and have an `origin` that exactly matches their credential-free source
  URL. The configured ref must exist locally, contain the configured full Git
  SHA, and must not be `main`, `master`, `HEAD`, or `latest`.
- The helper must exist in the pinned agent-app commit. The app registers
  its workflow definitions dynamically; separate worker/workflow YAML files
  are not required. Optional configuration assets listed in a manifest are
  still checked for missing files, duplicate paths, and hash changes.
- Use an already-installed Linux x86-64 Rust toolchain in an approved
  development directory. Set `RUSTUP_HOME` to that directory. Cargo builds
  run offline with `--locked`; set
  `LOOKBACK_CODING_AGENT_CARGO_HOME` to an approved, pre-populated cache under
  `/tmp/opencode` or this checkout. That cache must not contain Cargo config or
  credential files. Missing offline dependencies stop the build; the script
  does not fall back to the user's home or the network.
- The `jobworkerp-client` Git revision must be copied explicitly from the
  current `Cargo.lock` and match that lock exactly. The current checkout's
  recorded value is `e732e0f8587762803e4b928b5df66845304c26d7`; re-check it
  before use. No S2 runtime commit is supplied as a default.

## Build a local artifact bundle

### Local Linux SDK location

The app-managed Linux jobworkerp child defaults to the OS-registered user's
`~/.local/share/lookback/msb` for `MSB_HOME` and its `config.json` for
`MSB_CONFIG_PATH`. Startup and restart use the same child-only configuration;
remote servers and non-Linux launchers are unchanged. Inherited or `.env`
runtime binary overrides (`MSB_PATH`, `MSB_LIBKRUNFW_PATH`, `MSB_AGENTD_PATH`)
are removed from that child.

The launcher does not create or overwrite the SDK configuration or install
runtime assets. An existing configuration's explicit `home` takes precedence
over `MSB_HOME` under SDK 0.7.6. A socket-path-length warning does not stop
ordinary jobworkerp startup. These defaults alone do not verify runtime assets
or enable coding-agent execution; the activation gates above still apply. Native
package staging does not automatically install the SDK/runtime assets or add
their provenance to this artifact manifest, so this remains a separate
distribution and activation gate.

Set each value after manually reviewing the source checkout and its pin. Do
not put credentials in repository URLs. The app/runtime refs and full SHAs are
operator-selected inputs; the build script intentionally does not infer them.

```sh
export LOOKBACK_CODING_AGENT_APP_REPO_URL='https://example.invalid/team/agent-app.git'
export LOOKBACK_CODING_AGENT_APP_REF='<reviewed-non-main-ref>'
export LOOKBACK_CODING_AGENT_APP_REVISION='<full-agent-app-commit-sha>'

export LOOKBACK_CODING_AGENT_RUNTIME_URL='https://github.com/jobworkerp-rs/jobworkerp-rs'
export LOOKBACK_CODING_AGENT_RUNTIME_REF='<reviewed-runtime-ref>'
export LOOKBACK_CODING_AGENT_RUNTIME_REVISION='<full-runtime-commit-sha>'
export LOOKBACK_CODING_AGENT_CLIENT_REVISION='e732e0f8587762803e4b928b5df66845304c26d7'

export RUSTUP_HOME='/tmp/opencode/rustup'
export LOOKBACK_CODING_AGENT_CARGO_HOME='/tmp/opencode/cargo-home'

bash scripts/build-coding-agent-executor.sh \
  --target x86_64-unknown-linux-gnu \
  --runtime-source .build-deps/jobworkerp-rs \
  --artifact-dir artifacts/coding-agent/x86_64-unknown-linux-gnu
```

The output directory must not already exist and must be inside this checkout or
`/tmp/opencode`. The build uses isolated temporary output, does not overwrite
the regular Tauri sidecar staging directory, and records
`coding-agent-artifact-manifest.json` beside the runtime and helper binaries.
The manifest includes source URL/ref/full SHA, app/runtime submodule state, the
locked client revision and lockfile hash, target/profile/protocol versions,
executor capabilities, artifact hashes, and the four configuration hashes.
Dirty sources, floating refs, lock mismatch,
quoted/escaped lockfile keys, unsupported array-table headers, unsupported
targets, missing configs, or offline build failures stop the build.
The lockfile scanner accepts a conservative subset with canonical `[[package]]`
records; it is not a general TOML parser and does not silently ignore aliases.

Use this dedicated command directly; do not route it through the general
`build-release.sh` orchestrator. That legacy release path has separate toolchain
preflight and UI staging behavior and is not part of this executor artifact
gate.

### Development staging

`pnpm tauri dev` (through `scripts/stage-dev-external-bins.sh`) builds
`coding-agent-executor` from this workspace in debug mode and stages a
development package at `<cargo target dir>/debug/coding-agent/` (for an
explicit `--target`, `<cargo target dir>/<triple>/debug/coding-agent/`). That
directory is what Tauri resolves as `resource_dir()` during `tauri dev`, so the
package is never part of a release bundle and disappears with `cargo clean`.
The staged `runtime/all-in-one` is the same binary installed as the sidecar, so
the runtime digest check against the running child passes.

The manifest is written by the shared `scripts/lib/coding-agent-manifest.mjs`
with `--allow-development-sources`: it records the real provenance of the
working trees (`dirty: true` when uncommitted changes exist, the current branch
or `HEAD` as `ref`, scp-style remotes normalised to `ssh://`). Only debug builds
of the app accept such a manifest (`PackageSourcePolicy::for_build()`); release
builds keep requiring clean, pinned sources. The Settings diagnostic labels the
result as a development package. Staging re-runs are idempotent: an unchanged
manifest is left in place, and the verifier runs in
`--provenance-only --allow-development-sources` mode before the app starts.
Staging is skipped with a log line on non-Linux hosts and non-x86_64 triples.

Requirements: a jobworkerp runtime checkout with an `origin` remote (the sibling
checkout containing this app by default; override with
`LOOKBACK_CODING_AGENT_RUNTIME_SOURCE_DIR`), `/tmp/opencode` writable for the
verifier probe, and a umask that keeps the staged directories free of group/world
write bits (`022`). A relocated `CARGO_TARGET_DIR` outside the checkout is staged
but not verified.

## Verify provenance without enabling the feature

Run from the agent-app checkout. The `--provenance-only` mode has exit status
zero only when the manifest, hashes, configuration, and actual helper CLI
queries agree. Its JSON still reports that execution is disabled:

```sh
node scripts/verify-coding-agent-artifacts.mjs \
  --manifest artifacts/coding-agent/x86_64-unknown-linux-gnu/coding-agent-artifact-manifest.json \
  --artifact-root artifacts/coding-agent/x86_64-unknown-linux-gnu \
  --config-root . \
  --provenance-only --json
```

Without `--provenance-only`, the verifier exits non-zero while installed
runtime protocol/configuration compatibility is unverified. Missing files,
unknown manifest fields, unsafe paths, hash mismatches, unsupported profile or
target versions, helper version/protocol mismatches, and unsupported
capabilities fail closed. Manifest-controlled paths are relative and
symlink-free; verifier commands and arguments are fixed rather than taken from
the manifest.

## Installation and distribution gate

No host installation, privileged or superuser directory write, code signing,
or deployment is performed by these scripts. Before distribution, the platform
owner must use the existing trusted signing/release process, retain the
Linux/KVM target separately from UI packages, and verify the post-sign artifact
hashes. A signature process is not configured by this procedure, and a hash
manifest is not a signature.

Activation additionally requires a real test of the installed runtime and
helper on the intended Linux/KVM executor host. That test must establish the
runtime's protocol and configuration-version compatibility using an API
actually implemented by the runtime; a mocked CLI, helper self-report, or
source pin alone is insufficient. Use the existing jobworkerp worker
concurrency/pool controls for VM scheduling; activation does not require a
separate quota ledger or quota attestation. Until the compatibility contract is
implemented and verified, keep the feature disabled and preserve this artifact
as provenance-checked but not enable-ready.

## Automatic Issue completion

Enable semantic judging before enabling automatic completion. The settings screen clears and disables automatic completion when judging is turned off. Confirm automatic completion in the execution plan before starting a run.

The app checks the current Issue and saved results before completing an approved implementation or investigation run. A passing semantic judgement, successful required validation, saved results, and the selected publication result are required. Conflicts or unconfirmed results keep the run pending; reopening the app reconciles the same completion intent. Older runs without an approved assessment contract require human review.

## Semantic judgement retries and human reassessment

1. In **Settings → Issue settings → Coding agent completion assessment**, enable semantic
   judging and set communication retries (0–5) and the total deadline
   (10–600 seconds). The initial retry count is zero. Use the common page
   footer **Save and apply** before previewing and approving execution.
2. A semantic FAIL or INCONCLUSIVE result is not retried automatically until it passes. Communication failures use the approved retry limits; an unconfirmed enqueue is not submitted again.
3. For a run awaiting review, use the Issue screen to request reassessment and confirm the paid evaluation. The confirmation dialog shows whether auto-completion was approved for this run. If enabled, a passing reassessment automatically moves the Issue to DONE after all other completion requirements are met. Otherwise it stays awaiting review. The original judgment remains recorded.

## Automatic Issue validation

1. In the Issue execution panel, enter one JSON argv array per line, such as `["cargo", "test"]`. Arguments are passed directly to the executable. Escape embedded newlines as `\\n` within JSON strings.
2. Set the validation directory below `/workspace` and a timeout in seconds. `/workspace` refers to this execution's prepared Issue workspace; `/workspace/crate` selects its `crate` subdirectory.
3. Choose whether successful validation is required for automatic Issue completion, then preview and review the commands with the selected profile before approving execution.
4. The app saves the report before dispatching the validation workflow. An observed nonzero result prevents automatic commit/publication. A required check that was not executed prevents automatic DONE. Unconfirmed results remain UNKNOWN and are not automatically dispatched again.

Validation uses the same approved profile and SANDBOX worker as the Issue execution. Agent reports of successful tests do not substitute for the actual validation workflow result.

## Profile setup and authentication/settings shell

1. In **Settings → Coding agent → Execution profiles**, choose **No preset
   (empty profile)** to add a profile manually, or choose a preset for its
   initial values. Select **Add profile**, then edit the image reference,
   command, custom `Container home` outside `/workspace`, resources, and
   network access. Select **Confirm changes** when editing an existing profile,
   then use the common page footer **Save and apply**.
   The image may come from another registry; it is resolved and pinned for each
   new shell.
2. From the saved profile row, choose **Authentication and settings**. In
   **App-private settings** mode, use **Prepare dedicated home** only when the
   custom `Container home` does not already have a writable home mount. The
   dedicated home is private, persistent, and separate for each profile. It is
   mounted executable by default because agents install and run their own tools
   from home (for example the Codex app-server daemon under `CODEX_HOME`); only
   the sandbox VM executes from it. Clear **Executable** on that mount to make it
   non-executable; preparing the home again keeps that choice. An
   existing writable home mount is valid and is shared with ordinary execution;
   remove only mounts that conflict with a newly created dedicated home. In
   **Share host settings** mode, the selected host directories are used directly
   and dedicated-home preparation is unavailable and unnecessary. Existing
   credentials are never replaced. Home preparation is separate from the
   execution backend probe and requires the Coding agent draft to be saved and
   clean.
3. Choose **Open authentication and settings shell** from that profile action.
   The app opens an interactive shell in the selected profile image with the
   same home mounts used for agent execution: `bash -i` when the image provides
   bash (line editing and arrow keys), otherwise `sh -i`. Before the VM starts,
   the tool homes named by the profile runtime environment (`CODEX_HOME`,
   `CARGO_HOME`, `RUSTUP_HOME`) are created as private directories when they lie
   inside a writable home mount; paths outside such a mount belong to the image
   and are left untouched. The same happens before Issue runs. Run the agent's login or configuration
   command there. Configure provider, subagent, MCP, and other agent settings in
   those persistent directories; closing the shell does not prove that login or
   configuration succeeded.

The authentication/settings shell shows a manual guide for the selected command. For the
Codex command, use `codex login`; `CODEX_HOME` already exists in the dedicated
home. For Claude, enter `claude auth --help` first to confirm the
available authentication operation. For OpenCode, enter
`opencode providers --help` to confirm provider and authentication operations.
For an unknown command, inspect its own help. These commands are suggestions
to enter in the terminal; the app never runs them automatically and does not
assume an unverified `claude auth login` or `opencode auth login` operation.

4. Exit the shell or choose **Finish input**, then wait for confirmed completion.
   Cancellation is a request, not proof of process termination. If the result is
   unknown, inspect the existing session rather than starting another shell.
5. Use the saved profile for ordinary interactive or automatic execution.
   Closing the terminal view only detaches its display. Agent homes retain
   agent session logs; the app does not save settings terminal transcripts.

Profiles with an opaque `credentialReference` cannot open the
authentication/settings shell because the backend profile-settings worker rejects that authority
shape. Remove the reference and save the profile before opening the terminal.

The shell requires the managed local SANDBOX runtime and a writable
persistent mount inside the selected home. The authentication/settings dialog
shows "Dedicated home: not prepared" and keeps the shell action disabled until
such a mount exists (host-shared profiles count as prepared). Image pull,
SDK/KVM availability, and actual provider login must be verified on the
executor host.

## Window close and full quit

Closing the window opens a dialog in the app's selected language with **Quit**,
**Run in background**, and **Cancel** choices. Running in the background hides
only the window and does not cancel an app-owned coding-agent run. Cancel leaves
the window open.

Background execution is available only when a supported restoration path is
verified; creating a tray icon alone is not proof that the desktop exposes its
menu. When background execution is unavailable (including on Linux), the dialog
hides that choice and asks only whether to quit the app. Windows checks
the current tray icon rectangle, while macOS uses its system-owned status bar.
Use the tray's Open action to restore a hidden window.

If the app interface is not ready or does not respond, a native Quit/Cancel
dialog provides an alternative. It uses the last synchronized app language,
or Japanese and English together before the first language synchronization.

Use Quit for full application shutdown. Unconfirmed runs are durably recorded
as Unknown before sidecars stop; this does not claim successful cancellation or
normal VM termination. If the app cannot persist the unresolved state, it
refuses to quit and offers a retry. Actual desktop/tray behavior remains a
distribution test gate.
