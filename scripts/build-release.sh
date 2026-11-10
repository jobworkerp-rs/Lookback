#!/usr/bin/env bash
# Lookback release build orchestrator.
#
# Clones (or reuses) the five public dependency repositories, builds their
# binaries / plugins with the right features for the chosen platform + GPU
# backend, stages everything where Tauri expects it, regenerates the lindera
# dictionary, and runs `pnpm tauri build`.
#
# Two primary use cases are presets:
#   scripts/build-release.sh --profile mac          # Metal + DMG/.app
#   scripts/build-release.sh --profile linux-cuda   # CUDA + deb/AppImage
#
# Environment:
#   DRY_RUN=1   Print every side-effecting command without running it.
#   VERBOSE=1   (reserved)
#   LOOKBACK_RELEASE_VERSION
#                 Optional git tag (for example v0.0.3) used as the Tauri
#                 bundle version after stripping the leading v.
#   LOOKBACK_TAURI_VERBOSE=1
#                 Pass -v to the Tauri CLI so linuxdeploy stderr is surfaced.
#   LOOKBACK_MEMORY_STORE_REPO_URL
#                 Override the memory-store clone source.
#
# Run with -h for the full option list.

set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
AGENT_APP=$(cd "${SCRIPT_DIR}/.." && pwd)

# shellcheck source=lib/build-common.sh
source "${SCRIPT_DIR}/lib/build-common.sh"
# shellcheck source=lib/protoc-fetch.sh
source "${SCRIPT_DIR}/lib/protoc-fetch.sh"
# shellcheck source=lib/build-deps.sh
source "${SCRIPT_DIR}/lib/build-deps.sh"
# shellcheck source=lib/build-lindera.sh
source "${SCRIPT_DIR}/lib/build-lindera.sh"
# shellcheck source=lib/appimage-hooks.sh
source "${SCRIPT_DIR}/lib/appimage-hooks.sh"

# Defaults -----------------------------------------------------------------
PROFILE=""
PLATFORM=""
GPU=""
BUNDLE=""
WORKDIR="${AGENT_APP}/.build-deps"
SKIP_CLONE=0
SKIP_FRONTEND=0
ONLY_REPOS=""
LINDERA=release
LINDERA_ONLY=0
JOBS=""
NO_SUBMODULE=0
MACOS_UNSIGNED_INTERMEDIATE=0
EXPERIMENTAL_ISSUES=0
ISSUE_TRACKER_TAURI_ARGS=()

usage() {
  cat >&2 <<'EOF'
Usage: scripts/build-release.sh [options]

Presets:
  --profile mac          => --platform mac   --gpu metal --bundle dmg,app
  --profile linux-cuda   => --platform linux --gpu cuda  --bundle deb,appimage

Options:
  --platform mac|linux     Target platform (default: auto from uname)
  --gpu metal|cuda|cpu     GPU backend (default: mac->metal, linux->cuda)
  --bundle <list>          Tauri bundle targets, comma-separated, or "all"
  --workdir <dir>          Clone/build root (default: <agent-app>/.build-deps)
  --skip-clone             Reuse existing clones, do not fetch/pull
  --skip-frontend          Build/stage backend only, skip pnpm tauri build
  --only <repos>           Comma list: jobworkerp,memory-store,conductor,llama,mm,issue-tracker-server
                           (issue-tracker-server only with --experimental-issues)
  --lindera release|cli|skip
                           Dictionary strategy (default: release; cli is a legacy alias)
  --lindera-only           Generate only the lindera dictionary, then exit
                           (useful for `pnpm tauri:dev` without a full build)
  --jobs N                 Parallel cargo jobs (sets CARGO_BUILD_JOBS)
  --no-submodule-update    Skip submodule init/update (faster re-runs)
  --macos-unsigned-intermediate
                           Build an unsigned .app only; never create a DMG
  --experimental-issues    Enable the experimental Issue Tracker: build and
                           bundle issue-tracker-server, compile Lookback with
                           the experimental-issues feature, and run its E2E gate
  -h, --help               Show this help

Env: DRY_RUN=1 prints commands without executing.
EOF
}

# Argument parsing ---------------------------------------------------------
while [[ $# -gt 0 ]]; do
  case "$1" in
    --profile)  PROFILE=$2; shift 2 ;;
    --platform) PLATFORM=$2; shift 2 ;;
    --gpu)      GPU=$2; shift 2 ;;
    --bundle)   BUNDLE=$2; shift 2 ;;
    --workdir)  WORKDIR=$2; shift 2 ;;
    --skip-clone) SKIP_CLONE=1; shift ;;
    --skip-frontend) SKIP_FRONTEND=1; shift ;;
    --only)     ONLY_REPOS=$2; shift 2 ;;
    --lindera)  LINDERA=$2; shift 2 ;;
    --lindera-only) LINDERA_ONLY=1; shift ;;
    --jobs)     JOBS=$2; shift 2 ;;
    --no-submodule-update) NO_SUBMODULE=1; shift ;;
    --macos-unsigned-intermediate) MACOS_UNSIGNED_INTERMEDIATE=1; shift ;;
    --experimental-issues) EXPERIMENTAL_ISSUES=1; shift ;;
    -h|--help)  usage; exit 0 ;;
    *) die "unknown option: $1 (see --help)" ;;
  esac
done

# Resolve preset, then apply explicit overrides on top of it. ---------------
case "${PROFILE}" in
  "") ;;
  mac)        : "${PLATFORM:=mac}";   : "${GPU:=metal}"; : "${BUNDLE:=dmg,app}" ;;
  linux-cuda) : "${PLATFORM:=linux}"; : "${GPU:=cuda}";  : "${BUNDLE:=deb,appimage}" ;;
  *) die "unknown profile: ${PROFILE} (use mac|linux-cuda)" ;;
esac

# Platform default from host.
if [[ -z "${PLATFORM}" ]]; then
  case "$(uname -s)" in
    Darwin) PLATFORM=mac ;;
    Linux)  PLATFORM=linux ;;
    *) die "unsupported host OS: $(uname -s)" ;;
  esac
fi

# GPU default from platform.
[[ -z "${GPU}" ]] && GPU=$([[ "${PLATFORM}" == mac ]] && echo metal || echo cuda)

# Validate enums.
case "${PLATFORM}" in mac|linux) ;; *) die "invalid --platform: ${PLATFORM}" ;; esac
case "${GPU}" in metal|cuda|cpu) ;; *) die "invalid --gpu: ${GPU}" ;; esac
[[ "${PLATFORM}" == linux && "${GPU}" == metal ]] && die "metal is macOS-only"

# Bundle default (all applicable for the platform).
[[ -z "${BUNDLE}" ]] && BUNDLE=$([[ "${PLATFORM}" == mac ]] && echo "dmg,app" || echo "deb,appimage")

TRIPLE=$(detect_triple "${PLATFORM}")
LIBEXT=$([[ "${PLATFORM}" == mac ]] && echo dylib || echo so)
BIN_DIR="${AGENT_APP}/src-tauri/bin"
PLUGINS_DIR="${AGENT_APP}/src-tauri/plugins"

[[ -n "${JOBS}" ]] && export CARGO_BUILD_JOBS="${JOBS}"

export AGENT_APP BIN_DIR PLUGINS_DIR TRIPLE LIBEXT GPU WORKDIR \
  SKIP_CLONE NO_SUBMODULE ONLY_REPOS EXPERIMENTAL_ISSUES

# Preflight ----------------------------------------------------------------
preflight() {
  log "preflight: platform=${PLATFORM} gpu=${GPU} triple=${TRIPLE} bundle=${BUNDLE}"
  require_bundle_issue_tracker_dependencies
  # DRY_RUN only validates the command plan; skip host capability checks so a
  # machine without the toolchain can still preview the build.
  [[ "${DRY_RUN:-0}" == "1" ]] && { warn "DRY_RUN: skipping host capability checks"; return 0; }
  require_cmd git cargo rustc node pnpm cmake pkg-config curl unzip tar
  require_host_protobuf_tools
  rustc_at_least 1 85 || die "rustc >= 1.85 required for edition 2024 (have $(rustc --version))"

  # Ensure the target toolchain is installed when rustup is available.
  if command -v rustup >/dev/null 2>&1; then
    rustup target list --installed 2>/dev/null | grep -qx "${TRIPLE}" \
      || run rustup target add "${TRIPLE}"
  fi

  if [[ "${PLATFORM}" == mac ]]; then
    xcode-select -p >/dev/null 2>&1 || die "Xcode Command Line Tools required (xcode-select --install)"
    if [[ "${SKIP_FRONTEND}" != "1" ]]; then
      if [[ "${MACOS_UNSIGNED_INTERMEDIATE}" == "1" ]]; then
        [[ ",${BUNDLE}," != *",dmg,"* ]] \
          || die "--macos-unsigned-intermediate only permits --bundle app; unsigned output is not distributable"
      else
        [[ -n "${APPLE_SIGNING_IDENTITY:-}" ]] \
          || die "APPLE_SIGNING_IDENTITY is required for macOS release packaging (use --macos-unsigned-intermediate for an app-only local build)"
      fi
    fi
  fi
  if [[ "${PLATFORM}" == linux && ",${BUNDLE}," == *",appimage,"* ]]; then
    # appimagetool repackages Linux AppImages after runtime hook and driver-lib
    # cleanup. Only required when building one.
    require_cmd appimagetool patchelf
  fi
  if [[ "${GPU}" == cuda ]]; then
    command -v nvcc >/dev/null 2>&1 || die "CUDA toolkit (nvcc) required for --gpu cuda"
    # objdump reads DT_SONAME so the staged CUDA runtime keeps its symlink chain.
    require_cmd objdump
    if ! ldconfig -p 2>/dev/null | grep -qE 'libcudnn|libnccl'; then
      warn "libcudnn/libnccl not found via ldconfig; CUDA runtime may be incomplete"
    fi
  fi
}

# Patch verification: ensure the Linux .so glob is present in tauri.conf.json.
verify_tauri_conf() {
  # Plugin resource globs are per-platform (a *.so glob errors on macOS where no
  # .so exists, and vice versa), so they live in tauri.<platform>.conf.json which
  # tauri-build auto-merges. Verify the relevant platform file carries its glob.
  local conf
  if [[ "${PLATFORM}" == linux ]]; then
    conf="${AGENT_APP}/src-tauri/tauri.linux.conf.json"
    grep -q '"plugins/\*\.so\*"' "${conf}" 2>/dev/null \
      || die "tauri.linux.conf.json lacks the 'plugins/*.so*' resource glob (Linux plugins and versioned runtime libs would not bundle)"
  else
    conf="${AGENT_APP}/src-tauri/tauri.macos.conf.json"
    grep -q '"plugins/\*\.dylib"' "${conf}" 2>/dev/null \
      || die "tauri.macos.conf.json lacks the 'plugins/*.dylib' resource glob (macOS plugins would not bundle)"
  fi
}

dump_appimage_diagnostics() {
  [[ "${PLATFORM}" == linux && ",${BUNDLE}," == *",appimage,"* ]] || return 0
  [[ "${DRY_RUN:-0}" == "1" ]] && return 0

  warn "AppImage bundling failed; dumping diagnostic context"
  local bundle_dir="${AGENT_APP}/target/release/bundle"
  local path
  for path in "${bundle_dir}/appimage" "${bundle_dir}/appimage_deb" "${PLUGINS_DIR}"; do
    if [[ -e "${path}" ]]; then
      log "diagnostic tree: ${path}"
      find "${path}" -maxdepth 4 -printf '%M %s %p -> %l\n' 2>/dev/null | sort >&2 || true
    else
      warn "diagnostic path missing: ${path}"
    fi
  done

  if command -v ldd >/dev/null 2>&1; then
    local so
    for so in "${PLUGINS_DIR}"/*.so "${PLUGINS_DIR}"/*.so.*; do
      [[ -e "${so}" && ! -L "${so}" ]] || continue
      log "ldd: ${so}"
      ldd "${so}" >&2 || true
    done
  fi
}

# Frontend build -----------------------------------------------------------
# The app resolves the bundled Issue Tracker server at startup only when the
# experimental-issues feature is compiled in, so the feature and the sidecar
# must ship together. Without the feature the server is dropped from the
# externalBin list that tauri.conf.json keeps for development.
set_issue_tracker_tauri_args() {
  if issue_tracker_bundled; then
    ISSUE_TRACKER_TAURI_ARGS=(--features experimental-issues)
    return 0
  fi
  local external_bins
  external_bins=$(node -e '
    const config = require(process.argv[1]);
    const externalBin = config.bundle.externalBin.filter((bin) => bin !== "bin/issue-tracker-server");
    process.stdout.write(JSON.stringify({ bundle: { externalBin } }));
  ' "${AGENT_APP}/src-tauri/tauri.conf.json")
  ISSUE_TRACKER_TAURI_ARGS=(--config "${external_bins}")
}

# tauri_build_command: fill TAURI_COMMAND with the exact `tauri build` argv, so
# the logged command and the executed one cannot drift apart.
tauri_build_command() {
  TAURI_COMMAND=(pnpm tauri build)
  [[ "${LOOKBACK_TAURI_VERBOSE:-0}" != "1" ]] || TAURI_COMMAND+=(-v)
  TAURI_COMMAND+=(--bundles "${tauri_bundles}")
  # Prevent Tauri from rewriting externalBin bytes during macOS assembly. The
  # finalizer signs those exact, provenance-checked copies afterward. The
  # explicit CLI option is required: empty signing identities alone do not
  # disable Tauri's certificate-based signing path.
  [[ "${PLATFORM}" != "mac" ]] || TAURI_COMMAND+=(--no-sign)
  TAURI_COMMAND+=("${ISSUE_TRACKER_TAURI_ARGS[@]}")
}

run_tauri_build() {
  local cuda_stub_dir=$1
  tauri_build_command
  if [[ "${PLATFORM}" == "mac" ]]; then
    (cd "${AGENT_APP}" && env \
      APPLE_SIGNING_IDENTITY= TAURI_SIGNING_IDENTITY= \
      APPLE_CERTIFICATE= APPLE_CERTIFICATE_PASSWORD= \
      "${TAURI_COMMAND[@]}")
  elif [[ -n "${cuda_stub_dir}" ]]; then
    (cd "${AGENT_APP}" && env \
      LD_LIBRARY_PATH="${cuda_stub_dir}${LD_LIBRARY_PATH:+:${LD_LIBRARY_PATH}}" \
      "${TAURI_COMMAND[@]}")
  else
    (cd "${AGENT_APP}" && "${TAURI_COMMAND[@]}")
  fi
}

build_frontend() {
  [[ "${SKIP_FRONTEND}" == "1" ]] && { log "skip frontend (--skip-frontend)"; return 0; }
  # tauri.conf.json bundles "../dict/" as a resource; the dir is no longer in
  # git (the dictionary is generated, not committed). Ensure it exists so the
  # resource glob doesn't fail when lindera was skipped — an empty dict just
  # ships nothing and the sidecar falls back to the ngram tokenizer.
  run mkdir -p "${AGENT_APP}/dict"
  # `stage_binaries` / `stage_plugins` above own the release artifacts. Do
  # not let the direct Tauri wrapper replace them with developer-workspace
  # outputs while bundling.
  export LOOKBACK_EXTERNAL_ARTIFACTS_PRESTAGED=1
  log "frontend: pnpm install + tauri build (${BUNDLE})"
  run sh -c 'cd "$1" && pnpm install --frozen-lockfile' _ "${AGENT_APP}"
  if [[ -n "${LOOKBACK_RELEASE_VERSION:-}" ]]; then
    run node "${SCRIPT_DIR}/ci-apply-release-version.mjs" \
      "${LOOKBACK_RELEASE_VERSION}" \
      "${AGENT_APP}/src-tauri/tauri.conf.json"
  fi
  local tauri_bundles=${BUNDLE}
  if [[ "${PLATFORM}" == "mac" ]]; then
    # Tauri must only assemble the unsigned .app. Final signing mutates nested
    # Mach-O bytes, so the DMG is created later from the sealed app.
    tauri_bundles=app
    log "Tauri assembles an unsigned macOS app; signing is deferred until provenance checks complete"
  fi
  set_issue_tracker_tauri_args
  tauri_build_command
  if [[ "${DRY_RUN:-0}" == "1" ]]; then
    log "   $ cd ${AGENT_APP} && ${TAURI_COMMAND[*]}"
  else
    log "   $ ${TAURI_COMMAND[*]}"
    local cuda_stub_dir="" status=0
    cuda_stub_dir=$(prepare_cuda_driver_stub_dir)
    run_tauri_build "${cuda_stub_dir}" || status=$?
    [[ -n "${cuda_stub_dir}" ]] && rm -rf "${cuda_stub_dir}"
    if [[ "${status}" != "0" ]]; then
      dump_appimage_diagnostics
      return "${status}"
    fi
  fi

  if [[ "${PLATFORM}" == "mac" ]]; then
    finalize_macos_release_bundle
  fi
}

finalize_macos_release_bundle() {
  [[ "${PLATFORM}" == "mac" ]] || return 0
  local app="${AGENT_APP}/target/release/bundle/macos/Lookback.app"
  local args=(--app "${app}" --source-root "${AGENT_APP}" --triple "${TRIPLE}")
  local dmg="${AGENT_APP}/target/release/bundle/dmg/Lookback.dmg"
  if [[ ",${BUNDLE}," == *",dmg,"* ]]; then
    args+=(--dmg "${dmg}")
  fi
  if [[ "${MACOS_UNSIGNED_INTERMEDIATE}" == "1" ]]; then
    args+=(--unsigned-intermediate)
  fi
  if issue_tracker_bundled; then
    args+=(--with-issue-tracker)
  fi
  if [[ "${DRY_RUN:-0}" == "1" ]]; then
    log "DRY_RUN: finalize unsigned macOS app and requested DMG after Tauri assembly"
    return 0
  fi
  run bash "${SCRIPT_DIR}/finalize-macos-bundle.sh" "${args[@]}"
}

# Run only after the standalone Issue Tracker server has been staged. This
# ensures the bundled external binary can serve the private gRPC integration.
verify_issue_tracker_grpc_server() {
  [[ "${SKIP_FRONTEND}" == "1" ]] && { log "skip Issue tracker E2E (--skip-frontend)"; return 0; }
  issue_tracker_bundled || { log "skip Issue tracker E2E (not bundled without --experimental-issues)"; return 0; }
  log "Issue Tracker gRPC E2E"
  run env \
    LOOKBACK_ISSUE_TRACKER_SERVER_BIN="${BIN_DIR}/issue-tracker-server-${TRIPLE}" \
    LOOKBACK_SIDECAR_BIN_DIR="${BIN_DIR}" \
    bash "${SCRIPT_DIR}/test-issue-tracker-grpc-e2e.sh"
}

# NVIDIA user-mode DRIVER libraries that must NEVER ship inside the bundle:
# they are versioned against the host's kernel module, so a build-image copy
# triggers CUDA_ERROR_SYSTEM_DRIVER_MISMATCH. linuxdeploy collects them via the
# plugins' ldd graph, so strip them back out of the generated AppImage. The CUDA
# toolkit RUNTIME (cudart/cublas/…) is intentionally NOT listed — it is shipped
# alongside the plugins (see stage_cuda_runtime) and is safe to bundle.
APPIMAGE_DRIVER_LIB_GLOBS=(
  'libcuda.so*'
  'libnvidia-*.so*'
  'libnvcuvid.so*'
  'libGLX_nvidia.so*'
  'libEGL_nvidia.so*'
  'libGLESv2_nvidia.so*'
)

# patch_linux_appimages: extract each built AppImage, patch linuxdeploy runtime
# hooks, delete CUDA driver libs that should stay host-provided, and repackage
# only when something changed. No-op on non-Linux builds or when no AppImage was
# produced.
patch_linux_appimages() {
  [[ "${PLATFORM}" == linux ]] || return 0
  [[ "${DRY_RUN:-0}" == "1" ]] && { log "DRY_RUN: skip AppImage runtime patching"; return 0; }
  local appdir="${AGENT_APP}/target/release/bundle/appimage"
  [[ -d "${appdir}" ]] || return 0

  require_cmd appimagetool
  local img
  for img in "${appdir}"/*.AppImage; do
    [[ -f "${img}" ]] || continue
    log "patch AppImage runtime from $(basename "${img}")"
    local work; work=$(mktemp -d)
    # `--appimage-extract` always writes ./squashfs-root under $PWD.
    ( cd "${work}" && "${img}" --appimage-extract >/dev/null )
    local root="${work}/squashfs-root"

    local changed=0
    local backend_patch_status=0
    if patch_linuxdeploy_gtk_hook_for_wayland_backend "${root}"; then
      log "  default GTK backend to Wayland with X11 fallback"
      changed=1
    else
      backend_patch_status=$?
      if [[ "${backend_patch_status}" == 2 ]]; then
        die "AppImage GTK hook has an unrecognized GDK_BACKEND format; refusing to package an X11-forced AppImage"
      fi
    fi

    if patch_linuxdeploy_gtk_hook_for_host_ime "${root}"; then
      log "  allow host GTK IME modules for WebKitGTK text input"
      changed=1
    fi

    if [[ "${GPU}" == cuda ]]; then
      local glob f
      for glob in "${APPIMAGE_DRIVER_LIB_GLOBS[@]}"; do
        while IFS= read -r f; do
          log "  remove bundled driver lib: ${f#"${root}/"}"
          rm -f "${f}"; changed=1
        done < <(find "${root}" -type f -name "${glob}" 2>/dev/null)
        # Dangling symlinks (e.g. libcuda.so.1 -> libcuda.so.NNN) too.
        while IFS= read -r f; do rm -f "${f}"; changed=1; done \
          < <(find "${root}" -type l -name "${glob}" 2>/dev/null)
      done
    fi

    if [[ "${changed}" == 1 ]]; then
      run rm -f "${img}"
      run sh -c 'ARCH=x86_64 appimagetool "$1" "$2"' _ "${root}" "${img}"
    else
      log "  no AppImage runtime changes needed"
    fi
    rm -rf "${work}"
  done
}

report() {
  # src-tauri is a workspace member, so cargo/tauri emit bundles into the
  # workspace-root target (agent-app/target), NOT src-tauri/target.
  local out="${AGENT_APP}/target/release/bundle"
  log "done. artifacts under: ${out}/{dmg,macos,deb,appimage}/"
}

# Orchestrate --------------------------------------------------------------
main() {
  # Dictionary-only mode: skip the heavy clone/build/package steps. Lets a
  # developer populate dict/lindera/ipadic before `pnpm tauri:dev`.
  if [[ "${LINDERA_ONLY}" == "1" ]]; then
    [[ "${DRY_RUN:-0}" == "1" ]] || require_cmd curl unzip tar
    run mkdir -p "${WORKDIR}"
    stage_lindera release
    return 0
  fi

  preflight
  verify_tauri_conf
  run mkdir -p "${WORKDIR}"
  build_all
  stage_binaries
  stage_plugins
  verify_fixed_atlas_lock
  stage_lindera "${LINDERA}"
  verify_issue_tracker_grpc_server
  build_frontend
  patch_linux_appimages
  report
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main
fi
