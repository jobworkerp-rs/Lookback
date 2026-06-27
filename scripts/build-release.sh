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
LINDERA=cli
LINDERA_ONLY=0
JOBS=""
NO_SUBMODULE=0

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
  --only <repos>           Comma list: jobworkerp,memory-store,conductor,llama,mm
  --lindera cli|skip       Dictionary strategy (default: cli)
  --lindera-only           Generate only the lindera dictionary, then exit
                           (useful for `pnpm tauri:dev` without a full build)
  --jobs N                 Parallel cargo jobs (sets CARGO_BUILD_JOBS)
  --no-submodule-update    Skip submodule init/update (faster re-runs)
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
  SKIP_CLONE NO_SUBMODULE ONLY_REPOS

# Preflight ----------------------------------------------------------------
preflight() {
  log "preflight: platform=${PLATFORM} gpu=${GPU} triple=${TRIPLE} bundle=${BUNDLE}"
  # DRY_RUN only validates the command plan; skip host capability checks so a
  # machine without the toolchain can still preview the build.
  [[ "${DRY_RUN:-0}" == "1" ]] && { warn "DRY_RUN: skipping host capability checks"; return 0; }
  require_cmd git cargo rustc node pnpm cmake pkg-config curl unzip tar
  # protoc is fetched from the official protobuf release (curl + unzip, already
  # required above) and shipped as a self-contained externalBin, so the host
  # does NOT need protoc installed — see lib/protoc-fetch.sh.
  rustc_at_least 1 85 || die "rustc >= 1.85 required for edition 2024 (have $(rustc --version))"

  # Ensure the target toolchain is installed when rustup is available.
  if command -v rustup >/dev/null 2>&1; then
    rustup target list --installed 2>/dev/null | grep -qx "${TRIPLE}" \
      || run rustup target add "${TRIPLE}"
  fi

  if [[ "${PLATFORM}" == mac ]]; then
    xcode-select -p >/dev/null 2>&1 || die "Xcode Command Line Tools required (xcode-select --install)"
  fi
  if [[ "${PLATFORM}" == linux && ",${BUNDLE}," == *",appimage,"* ]]; then
    # appimagetool repackages Linux AppImages after runtime hook and driver-lib
    # cleanup. Only required when building one.
    command -v appimagetool >/dev/null 2>&1 \
      || die "appimagetool required to patch the generated AppImage"
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
    grep -q '"plugins/\*\.so"' "${conf}" 2>/dev/null \
      || die "tauri.linux.conf.json lacks the 'plugins/*.so' resource glob (Linux plugins would not bundle)"
    grep -q '"plugins/\*\.so\.\*"' "${conf}" 2>/dev/null \
      || die "tauri.linux.conf.json lacks the 'plugins/*.so.*' resource glob (versioned Linux runtime libs would not bundle)"
  else
    conf="${AGENT_APP}/src-tauri/tauri.macos.conf.json"
    grep -q '"plugins/\*\.dylib"' "${conf}" 2>/dev/null \
      || die "tauri.macos.conf.json lacks the 'plugins/*.dylib' resource glob (macOS plugins would not bundle)"
  fi
}

# Frontend build -----------------------------------------------------------
build_frontend() {
  [[ "${SKIP_FRONTEND}" == "1" ]] && { log "skip frontend (--skip-frontend)"; return 0; }
  # tauri.conf.json bundles "../dict/" as a resource; the dir is no longer in
  # git (the dictionary is generated, not committed). Ensure it exists so the
  # resource glob doesn't fail when lindera was skipped — an empty dict just
  # ships nothing and the sidecar falls back to the ngram tokenizer.
  run mkdir -p "${AGENT_APP}/dict"
  log "frontend: pnpm install + tauri build (${BUNDLE})"
  run sh -c 'cd "$1" && pnpm install --frozen-lockfile' _ "${AGENT_APP}"
  run sh -c 'cd "$1" && exec pnpm tauri build --bundles "$2"' _ "${AGENT_APP}" "${BUNDLE}"
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
    stage_lindera cli
    return 0
  fi

  preflight
  verify_tauri_conf
  run mkdir -p "${WORKDIR}"
  build_all
  stage_binaries
  stage_plugins
  stage_lindera "${LINDERA}"
  build_frontend
  patch_linux_appimages
  report
}

main
