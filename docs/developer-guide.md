# Developer Guide

This guide contains development-focused setup, launch, test, and environment details for Lookback. For the user-facing overview and first-run tutorial, see [../README.md](../README.md).

## Repository Layout

```text
src/                 React UI, hooks, API wrappers, and Vitest tests
src-tauri/           Tauri application, Rust commands, backend process lifecycle, gRPC clients
workers/             Lookback worker and workflow YAML bundle
proto/               Vendored protobuf definitions used by the Rust gRPC client
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
   - `protoc`: fetched automatically by the staging scripts from the official protobuf release
     (a self-contained binary); set `PROTOC` to override with your own self-contained protoc
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
PROTOC=/path/to/protoc \
LOOKBACK_PLUGINS_SRC=/path/to/plugins \
pnpm tauri:dev
```

`pnpm tauri:dev` and `pnpm tauri dev` run
`scripts/stage-dev-external-bins.sh` before startup. The script stages real
binaries at the target-triple paths Tauri validates, such as
`src-tauri/bin/all-in-one-x86_64-unknown-linux-gnu` on Linux x86_64. Source
resolution mirrors app runtime resolution: environment override, `PATH`, then
workspace-relative fallback.

This step also stages the official Memories SQLite migration bundle at
`src-tauri/migration-bundle/`. When `LOOKBACK_MEMORIES_DB_MIGRATE_BUNDLE` is
set, the script validates that bundle strictly and stops without falling back
to generation if it is invalid. Without the override, it validates
`../memories/target/memories-db-migrate-sqlite/`. If that output is missing or
invalid, an already-staged bundle is reused when it satisfies the current
release contract. Otherwise, the script runs the same checkout's official
`scripts/build-memories-db-migrate-sqlite.sh` into a temporary directory and
activates it only after successful verification. The first run can take longer
while compiling the coordinator or obtaining Atlas. Concurrent dev launches
are serialized, and generation or verification failures preserve the existing
staged bundle.

To inspect the resolution and build plan without building or writing files,
run:

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
staging do not write to `agent-app/../plugins/`.

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

For Linux dev launches, `pnpm tauri dev` defaults to `GDK_BACKEND=x11` and
`WEBKIT_DISABLE_DMABUF_RENDERER=1` to avoid WebKitGTK / GDK exits such as
`Gdk-Message: Error 71 ... dispatching to Wayland display`. Explicit user
values win, so `GDK_BACKEND=wayland WEBKIT_DISABLE_DMABUF_RENDERER=0 pnpm tauri dev`
still tries the Wayland path.

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

1. Build release binaries for `all-in-one`, `front`, `conductor-main`, `memories-import`, and `protoc`.
2. Copy the binaries into `src-tauri/bin/` using the exact external binary basenames configured in [../src-tauri/tauri.conf.json](../src-tauri/tauri.conf.json).
3. For Tauri packaging, ensure each binary is also available with the platform-triple suffix Tauri expects for the target platform.
4. Build runner plugin shared libraries from [`llama-cpp-runner`](https://github.com/jobworkerp-rs/llama-cpp-runner) and [`mm-embedding-runner`](https://github.com/jobworkerp-rs/mm-embedding-runner) for the target OS, then place them under `plugins/` at the repository root.
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
created by linuxdeploy, and repackages the image. The stock linuxdeploy hook pins
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

The CUDA GitHub Actions job temporarily renames `nccl.h` / `libnccl*` before building so dependency
build scripts do not auto-enable NCCL. The step restores those paths with an `EXIT` trap and skips
previous `*.disabled-for-build` names, so reruns on a self-hosted runner do not accumulate hidden
NCCL files.
Linux release runners (CPU and CUDA) must provide `jq` to read Atlas pinned-release data while creating the
`memory-store` SQLite migration bundle, plus `appimagetool`, `patchelf`, `desktop-file-utils`, and
`xdg-open` (provided by `xdg-utils`) before Tauri runs linuxdeploy. The workflow checks these tools explicitly so a missing runner-image dependency
fails before the long release build reaches AppImage bundling.
CUDA plugins link to the host-provided NVIDIA driver (`libcuda.so.1`), which must not be bundled.
When the build container has no real driver library in `ldconfig`, `scripts/build-release.sh` points
linuxdeploy at the CUDA toolkit stub through a temporary `LD_LIBRARY_PATH`. The existing AppImage
post-processing still removes any `libcuda.so*` copy before publishing.
The CUDA AppImage job sets `LOOKBACK_TAURI_VERBOSE=1` so linuxdeploy stderr is visible in GitHub
Actions logs. If bundling fails, `scripts/build-release.sh` also dumps the AppDir tree and staged
plugin `ldd` output.

### GitHub Actions Disk Cleanup

The GitHub-hosted Linux release job runs `scripts/ci-free-disk-space.sh` before Tauri deb/AppImage
bundling. It removes large preinstalled directories that this release build does not use, including
Android SDK, .NET, GHC, CodeQL, and language tool caches, then prints `df -h` before and after cleanup.

To investigate disk pressure:

1. Open the `Free runner disk space` step in the GitHub Actions `Build Linux bundles (cpu)` job.
2. Compare the free space in `Disk usage (before)` and `Disk usage (after)`.
3. If bundling still fails with `No space left on device`, inspect later job logs for growth under
   `target/`, `.build-deps/`, and `dict/`.
4. Add more cleanup targets only when they are GitHub runner standard directories unused by this build,
   by extending `cleanup_paths` in `scripts/ci-free-disk-space.sh`.
5. Run `bash scripts/test-ci-free-disk-space.sh` after changes to verify root-prefix and dry-run behavior.

### GitHub Actions Signed macOS Release

The public repository's `.github/workflows/release.yml` runs on tag pushes in this order: `test`,
`build-macos`, `build-cuda`, then the Linux CPU `build`. `build-macos` uses the `self-hosted`,
`macOS`, `lookback-macos` runner labels to execute `scripts/build-release.sh --profile mac` and
uploads the generated DMG to the same GitHub Release.
For the macOS profile, the script explicitly signs `src-tauri/plugins/*.dylib` and the migration
coordinator (`memories-db-migrate`) with `APPLE_SIGNING_IDENTITY` before Tauri packaging. The fixed
`atlas/bin/atlas` binary is deliberately not individually signed: `atlas-tool.lock.json` pins its exact
SHA-256. The build and the packaged `.app` are both checked against that lock before release. This path is
macOS-only and does not affect Linux `.so` plugins or CUDA runtime staging. Before
importing the certificate, the workflow deletes stale
`signing_temp*.keychain-db` files left by earlier self-hosted runner attempts, then imports the
`.p12` into a run-scoped keychain with `apple-actions/import-codesign-certs`. It verifies the signing
identity in the keychain and verifies notarization credentials with `notarytool history` before
running `scripts/build-release.sh --profile mac`; without these checks, explicit plugin signing can
fail with `The specified item could not be found in the keychain`, or invalid Apple ID / Team ID /
app-specific password values can fail only after a long build.
After Tauri build, the workflow verifies the migration coordinator signature and the packaged Atlas SHA-256,
explicitly runs
`notarytool submit --wait` and `stapler staple` on the DMG, then validates the stapled ticket and
Gatekeeper assessment before uploading the release asset.

To sign and notarize from the public repository:

1. Create a `Developer ID Application` certificate in Apple Developer.
2. On the Mac that created the CSR, export the certificate with its private key as `.p12`.
3. Base64-encode the `.p12`.

   ```bash
   openssl base64 -A -in certificate.p12 -out certificate-base64.txt
   ```

4. Register these GitHub Secrets.

   | Secret | Purpose |
   | --- | --- |
   | `APPLE_CERTIFICATE` | Base64-encoded `.p12` contents |
   | `APPLE_CERTIFICATE_PASSWORD` | Password used when exporting the `.p12` |
   | `APPLE_SIGNING_IDENTITY` | `Developer ID Application: ...` signing identity |
   | `APPLE_ID` | Apple ID for notarization |
   | `APPLE_PASSWORD` | Apple ID app-specific password |
   | `APPLE_TEAM_ID` | Apple Developer Team ID |

5. Push the release tag. The Linux CPU job starts after the macOS job succeeds.
6. If the macOS job fails, inspect `Build signed macOS bundles` to locate whether certificate import,
   codesign, or notarization failed.

After changing the public workflow, run `bash scripts/test-release-workflow.sh` to verify the macOS
job, Linux dependency, signing secrets, preflight auth checks, and DMG upload target. After changing
macOS plugin signing, run `bash scripts/test-build-release-macos-signing.sh` to verify that `.dylib`
files plus the migration coordinator are signed, Atlas remains unsigned, and Linux remains a no-op. Run
`bash scripts/test-verify-atlas-lock.sh` to verify the pinned Atlas checksum guard.

### Remote memories Diagnostics

When Settings > Connection uses Remote server, saving validates the URL syntax and the memories
workflow callback host/port/tls split. Use `Test connection` in the same card to actually dial the
configured jobworkerp and memories gRPC endpoints.

If remote memories pages or searches look empty:

1. Click `Test connection` and confirm both jobworkerp and memories are reachable.
2. For Semantic / Hybrid search, set Settings > Embedding model to the same embedding model and vector
   dimension as the remote server. Remote server mode does not generate local article embeddings; only
   query embedding depends on this local setting. This change does not reset or regenerate the local
   embedding index.
3. On failure, check the on-screen error or `<data-root>/log/lookback.log`. Dial failures are logged as
   `jobworkerp connection failed (<url>)` or `memories connection failed (<url>)`.
4. For more detail, launch with `LOOKBACK_RUST_LOG=debug` and repeat the operation.
5. If the connection test succeeds but the list is still empty, the connection worked. Check that the
   remote memories instance has data for the `user_id` Lookback is querying.

## Environment Variables

### Sidecar log defaults

The resident `jobworkerp`, `memories`, and `conductor` sidecars select their
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
   configuration, then list `<data-root>/log/` and its `native/` subdirectory.
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
