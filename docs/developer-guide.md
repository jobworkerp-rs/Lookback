# Developer Guide

This guide contains development-focused setup, launch, test, and environment details for Lookback. For the user-facing overview and first-run tutorial, see [../README.md](../README.md).

## Repository Layout

```text
src/                 React UI, hooks, API wrappers, and Vitest tests
src-tauri/           Tauri application, Rust commands, backend process lifecycle, gRPC clients
workers/             Lookback worker and workflow YAML bundle
proto/               Vendored protobuf used by the Rust gRPC client (jobworkerp, llm_memory, conductor, issue_tracker)
dict/                Optional search dictionaries staged for memory-store
docs/                Public developer documentation
```

See [../workers/README.md](../workers/README.md) for the worker/workflow bundle design.

## Install Dependencies

```bash
pnpm install
```

## Frontend Development

```bash
pnpm dev
pnpm test
pnpm build
pnpm lint
```

## Rust Checks

```bash
cargo build -p lookback-tauri
cargo clippy -p lookback-tauri --all-targets -- -D warnings
cargo test -p lookback-tauri -- --test-threads=1
```

Rust tests are run with one test thread because several tests share backend-like ports and data directories.

## Complete Desktop App Launch

1. Build or obtain release binaries for:
   - `all-in-one` from [`jobworkerp`](https://github.com/jobworkerp-rs/jobworkerp-rs)
   - `front` from [`memory-store`](https://github.com/jobworkerp-rs/memory-store)
   - `conductor-main` from [`jobworkerp-conductor`](https://github.com/jobworkerp-rs/jobworkerp-conductor)
   - `memories-import` from [`memory-store`](https://github.com/jobworkerp-rs/memory-store)
   - `issue-tracker-server` from the public repository pinned in [Release Build Details](#release-build-details)
   - Runtime `protoc`: fetched automatically by the staging scripts as a pinned, self-contained
     binary for the app bundle. `PROTOC` can override this development-staging input. This runtime
     binary is separate from the host compiler required on `PATH` by release-build preflight;
     setting `PROTOC` alone does not satisfy that host-tool check (and `jq` is also required).
2. Build the required jobworkerp runner plugins as shared libraries for the target OS and place them in a plugin directory:
   - [`llama-cpp-runner`](https://github.com/jobworkerp-rs/llama-cpp-runner) for local LLM execution
   - [`mm-embedding-runner`](https://github.com/jobworkerp-rs/mm-embedding-runner) for embedding generation

   Local LLM execution supports only Qwen 3.5/3.6-family and Gemma 4-family models. The
   default Local preset is the non-MTP Gemma 4 E2B IT QAT preset because Lookback summarizes before
   chat; MTP presets are available for chat-focused use. Gemma 4 MTP presets keep the QAT target
   GGUF as `model` and set `LlamaRunnerSettings.mtp.draft_model` to the draft GGUF.

   The current macOS bundle path uses `.dylib` files. Linux builds should use the corresponding shared library extension and resource mapping.
3. If the `memory-store` `front` binary is built with Lindera FTS support, place a compatible lindera 3.x IPADIC search dictionary at `dict/lindera/ipadic`, or set `LOOKBACK_LINDERA_SRC` to that directory. Lookback stages the files into the data directory, generates `lance_language_models/lindera/ipadic/config.yml` on each launch, and sets `LANCE_LANGUAGE_MODEL_HOME` for `memory-store`; it does not load the dictionary directly.
4. Start the app with explicit paths when the binaries are not available from the repository fallback locations:

```bash
LOOKBACK_JOBWORKERP_BIN=/path/to/all-in-one \
LOOKBACK_MEMORIES_BIN=/path/to/front \
LOOKBACK_CONDUCTOR_BIN=/path/to/conductor-main \
LOOKBACK_MEMORIES_IMPORT_BIN=/path/to/memories-import \
LOOKBACK_ISSUE_TRACKER_SERVER_BIN=/path/to/issue-tracker-server \
PROTOC=/path/to/protoc \
LOOKBACK_PLUGINS_SRC=/path/to/plugins \
pnpm tauri:dev
```

`pnpm tauri:dev` and `pnpm tauri dev` run
`scripts/stage-dev-external-bins.sh` before startup. The script stages real
binaries at the target-triple paths Tauri validates, such as
`src-tauri/bin/all-in-one-x86_64-unknown-linux-gnu` on Linux x86_64. Source
resolution mirrors app runtime resolution: environment override, `PATH`, then
workspace-relative fallback. When MCP is enabled, the sidecar starts with
`MCP_ENABLED=true`. Build `all-in-one` from the required jobworkerp revision
with `cargo build --release -p jobworkerp-main --bin all-in-one`, or point
`LOOKBACK_JOBWORKERP_BIN` at a compatible binary.

On Linux x86_64 the same staging step also builds `coding-agent-executor` in
debug mode and stages a development coding-agent package (executor, the staged
`all-in-one`, and a manifest recording the dirty working trees) at
`target/debug/coding-agent/`, which is the dev `resource_dir()`. Debug builds of
the app accept that package and the Settings diagnostic labels it as a
development package; it is not part of any release bundle. See
"Development staging" in
[docs/coding-agent-executor-install.md](coding-agent-executor-install.md) for
the requirements and overrides.

### Experimental Issues builds

Issues candidate extraction is enabled in debug builds. Standard release builds
hide the Issues UI and force extraction off, even when a user previously saved
an enabled policy, and do not bundle `issue-tracker-server`. To produce an
explicitly experimental release build, pass `--experimental-issues` to the
release script:

```bash
scripts/build-release.sh --profile linux-cuda --experimental-issues
```

The flag keeps three things together: it builds and bundles
`issue-tracker-server`, compiles the app with the `experimental-issues` Cargo
feature, and runs the Issue Tracker E2E gate (`scripts/test-issue-tracker-grpc-e2e.sh`).
A build with the feature but without the bundled server fails at startup, so do
not pass `--features experimental-issues` to `pnpm tauri build` on its own.
A manual `pnpm tauri build` uses the `externalBin` list in
`src-tauri/tauri.conf.json`, which keeps `issue-tracker-server` for development,
so it always bundles the server; use `build-release.sh` for a standard release.

`pnpm tauri build` (including `pnpm tauri:build`) does not perform development
staging or generate/reuse a development migration bundle. Before packaging, the
target-specific `front` and `memories-import` binaries and migration bundle must
already be staged from the same pinned Memories build, with a valid provenance
manifest whose artifact hashes match those exact files. The wrapper and Tauri's
build/bundle hooks reject missing provenance or mismatches. Use
`scripts/build-release.sh` for the pinned build-and-stage flow; for a manual
package, use `scripts/lib/write-migration-source-provenance.mjs` with the clean
pinned source, release-pinned ref/revision, and staged target binaries, then run
`pnpm verify:packaging-migration` before building. `pnpm tauri:dev` instead
builds the migration bundle from the sibling `../memories` checkout on every
start and swaps it in only when the content digest in `bundle-manifest.json`
differs from the staged bundle; do not use that path to prepare release package
inputs.

To inspect development staging without building or writing files, run:

```bash
scripts/stage-dev-external-bins.sh --dry-run
```

The same staging step also satisfies Tauri's `plugins/*.so*` and `*.dylib`
resource globs. Those globs are resolved from `src-tauri`, so the bundle staging
destination is `agent-app/src-tauri/plugins/`. Linux uses `plugins/*.so*` to
include both regular plugin `.so` files and versioned runtime libraries such as
`libcudart.so.12` and SONAME symlinks. If `LOOKBACK_PLUGINS_SRC` is set, it is searched
recursively. Otherwise, the script searches the established workspace layout at
`agent-app/../../plugins/cuda_runner/` and `agent-app/../../plugins/`, then
copies shared libraries into `agent-app/src-tauri/plugins/`. Dev and release
staging do not write to `agent-app/../plugins/`. The standalone
`issue-tracker-server` is staged separately as a target-triple external binary.
`LOOKBACK_ISSUE_TRACKER_SERVER_BIN` selects a prebuilt binary; otherwise dev
staging builds `issue-tracker-front --bin issue-tracker-server` from the sibling
`issue-tracker-server` checkout. `LOOKBACK_ISSUE_TRACKER_SERVER_DIR` overrides
that location; the legacy `LOOKBACK_ISSUE_TRACKER_RUNNER_DIR` is also accepted.
Lookback owns its loopback listener, runtime
configuration, health check, and private data root; it does not expose this
server as a jobworkerp worker, plugin, FunctionSet, or MCP tool.

At runtime, Lookback records the names and digests of libraries it stages in a
manifest in the runtime plugin directory. On the next staging pass, it removes
only libraries recorded in that manifest which are absent from the source. A
missing or invalid manifest disables removal and is recreated, preserving
user-provided shared libraries.

If macOS dev works but Linux fails with
`resource path bin/all-in-one-x86_64-unknown-linux-gnu doesn't exist`, the macOS
checkout likely already has `src-tauri/bin/*-aarch64-apple-darwin` or
`*-x86_64-apple-darwin`, while the Linux target-triple files have not been
staged. `scripts/build-release.sh` stages these files for release builds, but a
normal dev launch does not run the release script.

On Linux, `pnpm tauri:dev` starts the Vite development server while Tauri is
building the Rust crate. Vite excludes `src-tauri/` and `target/` from file
watching, so Cargo-created temporary directories such as
`target/debug/build/*/rustc*` are not scanned by Vite's watcher.

For Linux development and packaged launches, Lookback defaults to
`GDK_BACKEND=wayland,x11` and `WEBKIT_DISABLE_DMABUF_RENDERER=1`. GTK tries
native Wayland first and falls back to X11 when Wayland is unavailable. The
AppImage's linuxdeploy GTK hook from the pinned Tauri CLI (2.12.4) sets no
backend, which has the same effect; `scripts/build-release.sh` rewrites a hook from an
older CLI that forces X11 to the same rule. deb and rpm builds receive it from
the application before GTK initializes.
Explicit user values win: use `GDK_BACKEND=x11` to force X11, or
`GDK_BACKEND=wayland WEBKIT_DISABLE_DMABUF_RENDERER=0` to try native Wayland
with the DMA-BUF renderer enabled.

If startup stops with `Unknown system error -116` while scanning
`.../target/debug/build/.../rustc*`:

1. Confirm that `server.watch.ignored` in `vite.config.ts` includes `**/target/**`.
2. Stop any old Vite development server process, then start `pnpm tauri:dev` again.
3. If it still happens, check whether `CARGO_TARGET_DIR` points Rust build output
   to another path outside the repository `target/` directory.

### Linux AppImage First-Run Setup

If the AppImage cannot progress past the setup wizard's data-location step:

1. Start the AppImage from a terminal and look for errors mentioning `dialog`,
   `portal`, `permission`, or `validate_data_root`.
2. If `Choose…` does not open a directory picker, enter the path manually. The
   UI surfaces directory-picker startup failures and keeps manual validation
   available.
3. Confirm that the desktop environment has `xdg-desktop-portal` and a GTK
   portal backend installed, such as `xdg-desktop-portal-gtk` on Debian/Ubuntu.
   Tauri's Linux native dialogs can depend on the portal stack.
4. If `Next` stays disabled, confirm the path is absolute and either points to
   an existing writable directory or to a new path whose parent directory is
   writable.

If those variables are omitted, Lookback resolves binaries in this order:

1. Environment override.
2. Tauri `externalBin` next to the packaged executable.
3. `PATH`.
4. Workspace-relative fallback paths used by local development.

## Release Build Details

`scripts/build-release.sh --profile mac` (or `--profile linux-cuda`) automates the whole flow
below — clone, build with the right GPU features, stage binaries/plugins/lindera, and run Tauri
packaging. See the root README "Build From Source" for prerequisites and flags. The manual steps
below document what the script does, for partial or custom builds.

The release build must package backend binaries and resources before running Tauri packaging.

### Pinned public backend sources

Release builds use these public GitHub tags and resolved commits:

| Build input | Public repository | Tag | Peeled commit |
| --- | --- | --- | --- |
| `memory-store` (Memories) | [`jobworkerp-rs/memory-store`](https://github.com/jobworkerp-rs/memory-store) | `v0.0.24` | `8d365fc792489a98035bb11aaa3fe7e040a1177f` |
| `issue-tracker-server` | [`sutr-app/issue-tracker-server`](https://github.com/sutr-app/issue-tracker-server) | `v0.0.1` | `5974330a31ae19fb04db99841be73f305958d0d8` |

The `memory-store` name is a compatibility identifier for the Memories dependency; it does not mean the incompatible `memory-store/main` branch. The release builds `front`, `memories-import`, and the SQLite migration CLI from one checkout at the pinned revision. The generated migration provenance must describe that same source and the staged target binaries.

To review a different Memories source, set `LOOKBACK_MEMORY_STORE_REPO_URL`, `LOOKBACK_MEMORY_STORE_REF`, and `LOOKBACK_MEMORY_STORE_REVISION` together. The selected ref must contain the exact full revision. For an existing local checkout, use `LOOKBACK_MEMORY_STORE_SOURCE_DIR` with explicit `REF` and `REVISION`; the build verifies its origin and commit. Override the Issue Tracker source only with a reviewed repository, ref, and full SHA using `LOOKBACK_ISSUE_TRACKER_SERVER_REPO_URL`, `LOOKBACK_ISSUE_TRACKER_SERVER_REF`, and `LOOKBACK_ISSUE_TRACKER_SERVER_REVISION`. Provide credentials through Git's credential helper rather than embedding them in a URL.

The release-build preflight requires host `protoc` and `jq` on `PATH` on every target OS. The
runtime `protoc` staged into the app is downloaded separately; `PROTOC=/path/to/protoc` does not
replace the host `protoc` check. If `.build-deps/` contains dependency clones from an older Gitea
source, the build will not rewrite their origins. Use a new, clean build root for the public source,
for example `scripts/build-release.sh --profile mac --workdir "$HOME/.cache/lookback-public-deps"`.

1. Build release binaries for `all-in-one`, `front`, `conductor-main`, `memories-import`, `issue-tracker-server`, and `protoc`.
2. Copy the binaries into `src-tauri/bin/` using the exact external binary basenames configured in [../src-tauri/tauri.conf.json](../src-tauri/tauri.conf.json).
3. For Tauri packaging, ensure each binary is also available with the platform-triple suffix Tauri expects for the target platform. `LOOKBACK_JOBWORKERP_REF` may select a reviewed compatible jobworkerp revision.
4. Build runner plugin shared libraries from [`llama-cpp-runner`](https://github.com/jobworkerp-rs/llama-cpp-runner) and [`mm-embedding-runner`](https://github.com/jobworkerp-rs/mm-embedding-runner) for the target OS, then place them under `src-tauri/plugins/`. Build `issue-tracker-server` from the `issue-tracker-front` crate and stage it in `src-tauri/bin/` with its target-triple suffix. With `--experimental-issues`, a packaging build using `--only` must include `issue-tracker-server`; use `--skip-frontend` for a partial backend-only build.
5. If the packaged `memory-store` `front` build needs Lindera FTS, populate `dict/lindera/ipadic`
   with the IPADIC search dictionary. Nothing under `dict/` is committed — generate it with
   `scripts/build-release.sh --lindera-only`, which downloads the lindera 3.0.7 release dictionary
   and stages the IPADIC `COPYING` license beside it. The runtime generates `config.yml` in the
   data directory. This is also useful before `pnpm tauri:dev` if you want morphological FTS in
   development (otherwise the sidecar falls back to the ngram tokenizer).
6. Run `pnpm tauri:build`.

When `LOOKBACK_RELEASE_VERSION` is set, `scripts/build-release.sh` applies that tag to
`src-tauri/tauri.conf.json` before Tauri packaging. Tags may use the normal `v` prefix, so
`v0.0.3` produces bundle assets with version `0.0.3` instead of the development default in the
checked-in Tauri config.

### Linux AppImage Post-Processing

`scripts/build-release.sh` extracts each generated Linux AppImage, patches the GTK runtime hook
created by linuxdeploy, and repackages the image. The GTK plugin that the Tauri CLI uses for that
hook needs `gtk-query-immodules-3.0` and `gdk-pixbuf-query-loaders` on the build host; without them
the GTK module caches are missing and AppImage bundling fails. The stock linuxdeploy hook pins
`GTK_IM_MODULE_FILE` to the bundled `immodules.cache`; that cache does not include host fcitx/ibus
modules, so Japanese input can fall back to XIM and freeze WebKitGTK text fields on some desktops.

The post-processing step points `GTK_IM_MODULE_FILE` at the host GTK input-method cache when that
cache exposes fcitx/ibus, restores the host GTK module path, and derives `GTK_IM_MODULE` from
`XMODIFIERS=@im=fcitx` / `@im=ibus` when the variable is otherwise unset. If launchers omit
`XMODIFIERS`, it also infers fcitx/ibus from `fcitx5-remote` or the running `fcitx5` /
`ibus-daemon` process. When using the host fcitx/ibus cache, it preloads the host GLib family and
its direct dependencies so GTK can initialize IM modules built against a newer host GLib than the
bundled runtime. It falls back to the bundled cache when no host fcitx/ibus cache is available.
CUDA builds also remove host-versioned NVIDIA driver libraries from the same extracted root. After
changing AppImage hook handling, run:

```bash
bash scripts/test-appimage-hooks.sh
```

The GitHub Actions Linux jobs run on the self-hosted `lookback-linux-cuda` runner, which gives jobs
no root access and cannot mount FUSE. Its image ships without NCCL, so dependency build scripts do
not auto-enable it. Every system tool the Linux jobs use must come from the image; the
`Verify runner image tools` step of the `test` job lists them and checks them before any job uploads,
so a missing runner-image dependency fails the run early. Ask the runner administrator to add a
missing package to the image; the workflow cannot install packages. The Linux build jobs set
`APPIMAGE_EXTRACT_AND_RUN=1` so linuxdeploy and `appimagetool` extract themselves instead of
mounting through FUSE.
The runner is shared, so the jobs on it get a read-only `GITHUB_TOKEN`, check out without persisting
it, and reference no secrets, on tag pushes as well. They hand the bundles over as workflow artifacts,
and the GitHub-hosted `publish-linux` job attaches them to the GitHub Release.
CUDA plugins link to the host-provided NVIDIA driver (`libcuda.so.1`), which must not be bundled.
When the build container has no real driver library in `ldconfig`, `scripts/build-release.sh` points
linuxdeploy at the CUDA toolkit stub through a temporary `LD_LIBRARY_PATH`. The existing AppImage
post-processing still removes any `libcuda.so*` copy before publishing.
The CUDA AppImage job sets `LOOKBACK_TAURI_VERBOSE=1` so linuxdeploy stderr is visible in GitHub
Actions logs. If bundling fails, `scripts/build-release.sh` also dumps the AppDir tree and staged
plugin `ldd` output.

### Verifying a Release Build Without Publishing

A manual run of the release workflow builds every bundle, and signs and notarizes the macOS DMG, like
a release without publishing anything. Use it after a change outside the repository: a new
`lookback-linux-cuda` image, a change on the `lookback-macos` host, or rotated Apple certificates or
secrets. It runs the workflow and scripts currently on the public `main` branch:

1. In the public GitHub repository, open Actions > Release and choose Run workflow on the default
   branch. The workflow must already contain `workflow_dispatch`, which reaches GitHub only through a
   release sync.
2. The run executes `test`, `build-macos`, `sign-macos`, `build-cuda`, and `build`, and skips `publish-macos` and
   `publish-linux`. It does not stamp a release version, keeps no bundles (only the
   unsigned macOS app handed to `sign-macos`, for one day), and does not upload to a GitHub Release.
3. If `Verify runner image tools` fails, send the listed tools to the runner administrator.

### GitHub Actions Signed macOS Release

The public repository's `.github/workflows/release.yml` runs on tag pushes. After `test`, the macOS
build and Linux CUDA build can run in parallel; the Linux CPU build waits for CUDA. The build
jobs keep a read-only token; the GitHub-hosted `publish-macos` and `publish-linux` jobs attach the
bundles to the release. The macOS
build job uses the `self-hosted`, `macOS`, `lookback-macos` runner labels and installs host `protoc` and
`jq` with Homebrew; the Linux jobs take them from the runner image.

macOS signing is split across two jobs so that the signing key never shares a machine with the
dependency code the build runs. `build-macos` runs on the self-hosted runner without any secret: it
builds the verified unsigned `.app` (`scripts/build-release.sh --profile mac
--macos-unsigned-intermediate --bundle app`), packs it together with the build inputs the finalizer
checks it against (`src-tauri/bin`, `src-tauri/plugins`, `src-tauri/migration-bundle`, `dict`,
`resources/coding-agent`) into one tar, because artifacts drop symlinks and file modes, and publishes
the tar's SHA-256 as a job output. `sign-macos` runs on a GitHub-hosted macOS runner in the
`release-macos` environment, which alone holds the Apple secrets. It checks out the same commit,
verifies the checksum, refuses any archive entry outside those paths (so the archive cannot replace the
scripts it runs), unpacks it, and then signs, notarizes, and verifies as described below. A manual run
signs and notarizes too; only keeping the DMG for publishing is limited to a tag push.
The lint job also stages empty external-binary/plugin placeholders and creates the empty migration
resource directory solely to let `tauri-build` validate paths; they are not release artifacts.

For macOS, `scripts/build-release.sh` asks Tauri to assemble an unsigned `.app` so Tauri
cannot rewrite external-binary bytes before verification. `sign-macos` then runs
`scripts/finalize-macos-bundle.sh --dmg`, which validates the source and packaged migration provenance,
Atlas lock, and packaged resource hashes before signing. It signs the verified nested Mach-O files and
plugins first, records the signed migration/Atlas hashes while preserving their upstream source hashes,
then seals the outer `.app`. Only after those checks does it create and sign the DMG. This replaces
pre-Tauri plugin/migration signing; Linux `.so` plugins and CUDA runtime staging are unchanged.
`sign-macos` imports the `.p12` into a run-scoped keychain, verifies the signing identity, and checks
notarization credentials with `notarytool history` before signing. The finalize step receives only
`APPLE_SIGNING_IDENTITY`, and the last step deletes the run's keychain even when the job fails.
Every action in `release.yml` is pinned to a commit SHA with the version in a trailing comment
(`uses: owner/action@<sha> # vX.Y.Z`), so a moved tag cannot change the code that runs next to the Apple
secrets. To update one, resolve the new tag with `git ls-remote https://github.com/<owner>/<action>
'refs/tags/<tag>^{}'` and replace both the SHA and the comment; `scripts/test-public-release-workflow.mjs`
rejects an unpinned reference.

The workflow submits the finalized DMG with `notarytool submit --wait`, staples it, and then validates
the sealed app's migration provenance against the actual packaged resource tree, all relevant code
signatures, the stapled ticket, and Gatekeeper assessment before uploading the artifact. The finalizer's
optional `--notarize` mode uses an existing `APPLE_NOTARY_KEYCHAIN_PROFILE`; the release workflow uses its
own notarization step with an App Store Connect API key instead of an Apple ID and app-specific password,
because the key can be scoped to the team and revoked on its own. Only the step that writes the key to a
file (readable only by the job user) receives its contents, and the final cleanup deletes that file.

`.github/` (the release workflow and the actionlint runner labels) is maintained in the upstream source
repository and published to the public repository by every release sync, which overwrites direct edits
on the public side. Every job is limited to GitHub with `if: github.server_url == 'https://github.com'`.
`node scripts/test-public-release-workflow.mjs` verifies that guard together with the release
dependencies, signing Secrets, provenance checks, and artifact flow.

On macOS release builds, jobworkerp uses the signed `.app/Contents/Resources/plugins` directory directly
for both plugin discovery and dependent-dylib resolution. It never loads the copied data-directory plugins,
preventing an older plugin signed by another team (or a CI placeholder) from being scanned. Linux keeps
user-managed `.so` plugins.

To sign and notarize from the public repository:

1. Create a `Developer ID Application` certificate in Apple Developer.
2. On the Mac that created the CSR, export the certificate with its private key as `.p12`.
3. Base64-encode the `.p12`.

   ```bash
   openssl base64 -A -in certificate.p12 -out certificate-base64.txt
   ```

4. In the public repository, open Settings > Environments, create `release-macos`, limit its
   deployment branches and tags to `main` (manual runs) and `v*` (releases), and register these
   environment secrets. Do not keep them as repository secrets: only `sign-macos`, which runs on a
   GitHub-hosted runner, should receive them.

   | Secret | Purpose |
   | --- | --- |
   | `APPLE_CERTIFICATE` | Base64-encoded `.p12` contents |
   | `APPLE_CERTIFICATE_PASSWORD` | Password used when exporting the `.p12` |
   | `APPLE_SIGNING_IDENTITY` | `Developer ID Application: ...` signing identity |
   | `APPLE_NOTARY_KEY` | Contents of the App Store Connect API key (`AuthKey_<KEY_ID>.p8`) |
   | `APPLE_NOTARY_KEY_ID` | Key ID of that API key |
   | `APPLE_NOTARY_ISSUER_ID` | Issuer ID shown above the team keys |

   To create the API key, open App Store Connect > Users and Access > Integrations > App Store Connect
   API > Team Keys (the Account Holder or an Admin can do this), add a key with the Developer access
   role, and download the `.p8`. Apple lets you download it only once; register it and delete the
   local copy. To rotate it, add a new key, update the three secrets, and revoke the old key.

5. Push the release tag. The macOS and CUDA jobs start after `test`; the CPU job starts after CUDA.
6. If `build-macos` fails, inspect `Build unsigned macOS app` for build or provenance errors. If
   `sign-macos` fails, its steps show whether the hand-off check, certificate import, provenance
   validation, codesign, or notarization failed.

After changing the release workflow, run `node scripts/test-public-release-workflow.mjs` and
`bash scripts/test-release-workflow.sh` to verify the GitHub-only guard, the macOS job, Linux dependency,
signing Secrets, preflight auth checks, and DMG upload target. After changing
macOS plugin signing, run `bash scripts/test-build-release-macos-signing.sh` to verify that `.dylib`
files and both migration executables are signed, and Linux remains a no-op. Run
`bash scripts/test-verify-atlas-lock.sh` to verify the upstream and signed Atlas checksum guard.

### Connection Targets and Remote Diagnostics

Settings > Connection chooses Local sidecar or Remote server separately for jobworkerp, memories,
the Issue tracker and conductor. The app starts only the services set to Local. Combinations the
backend rejects are disabled in the card, with the reason shown:

- A remote jobworkerp needs a remote memories and, while Issue tracking is enabled, a remote Issue
  tracker, because a remote jobworkerp cannot reach this machine's sidecars.
- A remote conductor needs a remote jobworkerp. A local conductor with a remote jobworkerp is fine;
  periodic runs are enqueued to the remote jobworkerp.

A `connection.json` in the earlier single-`mode` shape is read with that mode applied to jobworkerp
and memories. The file is not rewritten on load; the next save writes the new shape. Issue tracker
data and conductor schedulers are separate between local and remote and are not migrated.

Saving validates the URL syntax and the host/port/tls split workflows need. Use `Test connection` to
actually dial the targets. When jobworkerp or conductor reach a service at a different address
(e.g. behind a proxy), override it under "Addresses as jobworkerp and conductor see them".

#### Using a remote jobworkerp

1. Set memories (and the Issue tracker) to Remote and enter their URLs. The Issue tracker also needs
   its gateway control and worker URLs.
2. Set jobworkerp to Remote and enter its URL. For the periodic import, enter the memories-import,
   opencode DB and working-directory paths under "Paths on the jobworkerp host".
3. Check reachability with `Test connection`, then save with Save and apply at the bottom of the screen.
4. Review "Remote jobworkerp readiness" and press `Register workers`.
5. Set the missing channels, plugin runners and environment on the remote jobworkerp from the shown
   settings, restart it, and press `Check again`. See "リモート jobworkerp で使う" in
   `workers/README.md` for what is required.
6. Startup also inspects readiness and shows "remote jobworkerp needs setup" in the sidebar when
   something is missing. If it reports a different app version after an update, repeat step 4.
   An unreachable remote jobworkerp does not stop the app from opening; the sidebar shows "remote
   jobworkerp unreachable" so the connection can be fixed in Settings.

While jobworkerp is remote, the coding agent and chat Issue tools are unavailable, and Timezone edits
are disabled; set `TZ` on the remote side.

#### Remote memories pages or searches look empty

1. Click `Test connection` and confirm both jobworkerp and memories are reachable.
2. For Semantic / Hybrid search, set Settings > Embedding model to the same embedding model and vector
   dimension as the remote server. While memories is remote, local article embeddings are not
   generated; only query embedding depends on this local setting. This change does not reset or
   regenerate the local embedding index.
3. On failure, check the on-screen error or `<data-root>/log/lookback.log`. Dial failures are logged as
   `jobworkerp connection failed (<url>)` or `memories connection failed (<url>)`.
4. For more detail, launch with `LOOKBACK_RUST_LOG=debug` and repeat the operation.
5. If the connection test succeeds but the list is still empty, the connection worked. Check that the
   remote memories instance has data for the `user_id` Lookback is querying.

## Local embedding configuration and recovery

Lookback does not rename, delete, or restore memories vector-storage directories.
Local changes to the embedding model or vector dimension require a service-owned
migration, which the current memories API does not provide. Such changes are
rejected before settings are saved. Changes within the same embedding space and
remote query-embedding configuration remain available.

First-run setup retains the effective local embedding configuration instead of
switching models based on the UI language. For storage or schema startup errors,
inspect the logs and follow the memories-side configuration or recovery procedure.

## Environment Variables

### Sidecar log defaults

The resident `jobworkerp`, `memories`, `conductor`, and `issue-tracker` sidecars select their
default `RUST_LOG` filter from the build profile:

- `pnpm tauri dev` (a debug build) uses an `info` baseline.
- Release bundles such as the macOS DMG use a `warn` baseline to keep routine
  progress out of the packaged application's log files.

Both profiles keep the `app=warn`, `worker_app=warn`, and `lance=warn` target
filters because those targets can emit very large request or storage traces.
Set `LOOKBACK_RUST_LOG` to replace the complete filter in either profile, for
example `LOOKBACK_RUST_LOG=debug pnpm tauri dev`. This setting applies to the
three resident sidecars; the short-lived `memories-import` process is not
controlled by this policy.

### Log rotation and retention

Lookback keeps the active application and sidecar output files under
`<data-root>/log/`. The active names (`lookback.log` and
`<sidecar>.<stdout|stderr>.log`) are stable, so the in-app log viewer continues
to show the current file after a rotation. A writer rotates an active file on
the UTC date boundary or before a write that would exceed 10 MiB. The previous
file receives a UTC timestamp suffix and is never edited after it is closed.

The retention policy differs only for the capacity limit:

- Both debug (`pnpm tauri dev`) and release builds remove closed, managed log
  archives older than 14 days.
- Release builds additionally remove the oldest closed archives when managed
  logs exceed 256 MiB in total. Active files are not deleted by this sweep.
- Debug builds do not delete logs because of total size; use the normal
  rotation to retain the history while investigating development issues.

Short-lived native tracing files produced by sidecars or `memories-import` are
written below `log/native/<launch-id>/`. The parent-captured stdout/stderr files
are the canonical logs. Native files are considered for the same 14-day sweep
only after their process has exited; they are not renamed or deleted while a
child is running.

Migration attempt logs (`database-migration-*.log`) are closed audit files and
follow the 14-day age policy. Release builds also include them in the 256 MiB
capacity sweep. Unknown files, directories, symlinks, and active files are not
deleted by log maintenance.

To inspect rotated logs:

1. Resolve the data root from the Settings page or the `LOOKBACK_*` launch
configuration, then list `<data-root>/log/` and its `native/` subdirectory. For
an unavailable Issue Tracker, inspect `issue-tracker.stdout.log` and
`issue-tracker.stderr.log` first.
2. Read the stable active file first. If the relevant event predates the last
   rotation, select the timestamped archive for that stream; archives are
   ordinary text/JSONL files and can be copied while the app is running.
3. For release cleanup diagnostics, check `lookback.log` for the best-effort
   retention result. A cleanup failure does not stop sidecar startup or normal
   application work; retrying at the next maintenance interval is expected.

Common development overrides:

| Variable | Purpose |
| --- | --- |
| `LOOKBACK_JOBWORKERP_BIN` | Path to `all-in-one` |
| `LOOKBACK_MEMORIES_BIN` | Path to `front` |
| `LOOKBACK_CONDUCTOR_BIN` | Path to `conductor-main` |
| `LOOKBACK_MEMORIES_IMPORT_BIN` | Path to `memories-import` |
| `LOOKBACK_ISSUE_TRACKER_SERVER_BIN` | Path to a prebuilt `issue-tracker-server` for dev staging |
| `LOOKBACK_ISSUE_TRACKER_SERVER_DIR` | Override for the sibling `issue-tracker-server` checkout used by dev staging |
| `LOOKBACK_ISSUE_TRACKER_MAINTENANCE_INTERVAL_SECS` | Issue Tracker maintenance interval in seconds (default `5`; values below `1` are clamped to `1`; invalid values fall back to `5`) |
| `PROTOC` | Path to `protoc` |
| `LOOKBACK_PLUGINS_SRC` | Source directory for plugin shared libraries |
| `LOOKBACK_LINDERA_SRC` | Source directory for IPADIC search dictionary files staged for `memory-store` |
| `LOOKBACK_WORKERS_DIR` | Override for the worker/workflow YAML bundle |
| `LOOKBACK_ENV_FILE` | `.env` template forwarded to backend processes |
| `LOOKBACK_RUST_LOG` | Backend process log filter override |
| `LOOKBACK_FORCE_SETUP_WIZARD` | Force the first-run setup wizard in development |

LLM and embedding settings are normally managed from the Settings page. External LLM API keys are stored in the OS credential store, such as Keychain. The corresponding `LOOKBACK_LLM_*` and `LOOKBACK_EMBEDDING_*` variables are primarily development overrides.

## Testing and Linting

Run the standard checks before committing:

```bash
pnpm test
pnpm lint
pnpm build
cargo test -p lookback-tauri -- --test-threads=1
cargo clippy -p lookback-tauri --all-targets -- -D warnings
```

Some integration tests require real backend binaries and plugin paths. When a test file documents required `LOOKBACK_*` variables, set those variables explicitly before running that test.
