#!/usr/bin/env bash
# Dependency repository clone + build + stage for the Lookback release build.
#
# Sourced by scripts/build-release.sh. Relies on helpers from build-common.sh
# and on these caller-provided globals:
#   WORKDIR     clone/build root
#   AGENT_APP   absolute path to the agent-app repo (this checkout)
#   BIN_DIR     <AGENT_APP>/src-tauri/bin
#   PLUGINS_DIR <AGENT_APP>/src-tauri/plugins
#   TRIPLE      target triple (e.g. aarch64-apple-darwin)
#   LIBEXT      dylib | so
#   GPU         metal | cuda | cpu
#   SKIP_CLONE NO_SUBMODULE   "1"/"0"
#   ONLY_REPOS  comma list filter ("" = all)

# Logical repos: jobworkerp memory-store conductor llama mm.
# macOS ships bash 3.2 (no associative arrays), so URL/dir lookups are
# case-based rather than `declare -A`.

# repo_url NAME -> public git URL (branch=main).
repo_url() {
  case "$1" in
    jobworkerp)   echo "https://github.com/jobworkerp-rs/jobworkerp-rs" ;;
    memory-store) echo "https://github.com/jobworkerp-rs/memory-store" ;;
    conductor)    echo "https://github.com/jobworkerp-rs/jobworkerp-conductor" ;;
    llama)        echo "https://github.com/jobworkerp-rs/llama-cpp-runner" ;;
    mm)           echo "https://github.com/jobworkerp-rs/mm-embedding-runner" ;;
    *) die "unknown repo: $1" ;;
  esac
}

# repo_subdir NAME -> clone directory name under WORKDIR.
repo_subdir() {
  case "$1" in
    jobworkerp)   echo "jobworkerp-rs" ;;
    memory-store) echo "memory-store" ;;
    conductor)    echo "jobworkerp-conductor" ;;
    llama)        echo "llama-cpp-runner" ;;
    mm)           echo "mm-embedding-runner" ;;
    *) die "unknown repo: $1" ;;
  esac
}

# want_repo NAME -> 0 (build it) / 1 (skip), honouring --only.
want_repo() {
  [[ -z "${ONLY_REPOS}" ]] && return 0
  [[ ",${ONLY_REPOS}," == *",$1,"* ]] && return 0
  return 1
}

# repo_path NAME -> absolute clone dir.
repo_path() { echo "${WORKDIR}/$(repo_subdir "$1")"; }

# is_own_git_repo DIR -> 0 if DIR is the top level of its OWN git repo.
# A plain `rev-parse --git-dir` is not enough: when WORKDIR lives inside the
# agent-app checkout (the default), a dependency dir that only holds a restored
# target/ resolves to the PARENT repo's .git and would be mistaken for a real
# clone (then `pull --ff-only` fails on the parent's detached HEAD). Require the
# git top level to be DIR itself.
is_own_git_repo() {
  local dir=$1 top
  top=$(git -C "${dir}" rev-parse --show-toplevel 2>/dev/null) || return 1
  [[ "$(cd "${dir}" 2>/dev/null && pwd -P)" == "${top}" ]]
}

# clone_or_pull NAME
clone_or_pull() {
  local name=$1 url dir
  url=$(repo_url "${name}")
  dir=$(repo_path "${name}")
  if is_own_git_repo "${dir}"; then
    # A real git checkout already exists: reuse or fast-forward.
    if [[ "${SKIP_CLONE}" == "1" ]]; then
      log "reuse existing clone: ${dir}"
    else
      log "pull ${name}"
      run git -C "${dir}" pull --ff-only
    fi
  elif [[ -d "${dir}" ]]; then
    # The directory exists but is not a git repo. This happens in CI when a
    # build-cache action restores <dir>/target before the source is cloned —
    # `git clone` would fail with "destination path already exists". Initialise
    # the repo in place so the restored target/ is preserved.
    log "init-in-place ${name} -> ${dir} (non-git dir, e.g. restored cache)"
    run git -C "${dir}" init -q
    # Idempotent remote setup (add the first time, update on a re-run).
    run git -C "${dir}" remote remove origin 2>/dev/null || true
    run git -C "${dir}" remote add origin "${url}"
    # Full (non-shallow) fetch so init_submodules can resolve every submodule's
    # pinned commit; a shallow superproject can miss those.
    run git -C "${dir}" fetch origin main
    run git -C "${dir}" checkout -q -f -B main FETCH_HEAD
  else
    log "clone ${name} -> ${dir}"
    run git clone --branch main "${url}" "${dir}"
  fi
}

# init_submodules NAME
# Initialises submodules recursively but never touches the conductor's
# `frontend` submodule — it is not a workspace member and is not needed to build
# conductor-main. Restricting conductor's init to its `modules` paths keeps the
# clone from failing on that submodule.
init_submodules() {
  local name=$1 dir
  [[ "${NO_SUBMODULE}" == "1" ]] && { log "skip submodules: ${name}"; return 0; }
  dir=$(repo_path "${name}")
  log "submodules ${name}"
  if [[ "${name}" == "conductor" ]]; then
    # conductor's `frontend` submodule lives on a private host and is not a
    # workspace member; init only the `modules/*` it actually compiles against.
    run git -C "${dir}" submodule update --init --recursive -- modules
  else
    run git -C "${dir}" submodule update --init --recursive
  fi
}

# gpu_features -> the cargo --features flag (or empty) for the GPU backend.
# llama-cpp-runner uses the candle GPU feature only. mm-embedding-runner also
# needs the matching ONNX Runtime EP for ModernBERT checkpoints.
gpu_features() {
  case "${GPU}" in
    metal) echo "--features metal" ;;
    cuda)  echo "--features cuda" ;;
    cpu)   echo "" ;;
  esac
}

mm_gpu_features() {
  case "${GPU}" in
    metal) echo "--features metal,onnx-coreml" ;;
    cuda)  echo "--features cuda,onnx-cuda" ;;
    cpu)   echo "" ;;
  esac
}

# cargo_build DIR ARGS...
# Runs `cargo build` inside DIR. CARGO_BUILD_JOBS (from --jobs) is honoured by
# cargo via the environment, so no extra flag is needed here.
cargo_build() {
  local dir=$1; shift
  run sh -c 'cd "$1" && shift && exec cargo build "$@"' _ "${dir}" "$@"
}

# Per-repo build functions. Each leaves outputs in <dir>/target/release.
build_jobworkerp() {
  log "build all-in-one"
  cargo_build "$(repo_path jobworkerp)" --release -p jobworkerp-main --bin all-in-one
}

build_memory_store() {
  local dir; dir=$(repo_path memory-store)
  log "build front (lindera) + memories-import"
  cargo_build "${dir}" --release -p grpc-admin --bin front --features lindera
  cargo_build "${dir}" --release -p agent-chat-import --bin memories-import
}

build_conductor() {
  log "build conductor-main"
  cargo_build "$(repo_path conductor)" --release -p conductor-main
}

build_llama() {
  local feat; feat=$(gpu_features)
  log "build llama-cpp plugin (${GPU})"
  # shellcheck disable=SC2086
  cargo_build "$(repo_path llama)" --release -p jobworkerp-llama-cpp-plugin ${feat}
}

build_mm() {
  local feat; feat=$(mm_gpu_features)
  log "build mm-embedding plugin (${GPU})"
  # shellcheck disable=SC2086
  cargo_build "$(repo_path mm)" --release ${feat}
}

# build_one NAME -> dispatch to the per-repo build function.
build_one() {
  case "$1" in
    jobworkerp)   build_jobworkerp ;;
    memory-store) build_memory_store ;;
    conductor)    build_conductor ;;
    llama)        build_llama ;;
    mm)           build_mm ;;
  esac
}

# build_all: clone + submodules + build every selected repo.
build_all() {
  local name
  for name in jobworkerp memory-store conductor llama mm; do
    want_repo "${name}" || { log "skip ${name} (--only)"; continue; }
    clone_or_pull "${name}"
    init_submodules "${name}"
    build_one "${name}"
  done
}

# stage_binaries: copy the 5 externalBin targets with the platform-triple
# suffix Tauri expects. protoc is fetched from the official protobuf release
# (a self-contained binary) rather than the host's — see lib/protoc-fetch.sh.
stage_binaries() {
  log "stage binaries -> ${BIN_DIR}"
  run mkdir -p "${BIN_DIR}"
  local jwp mem cond
  jwp=$(repo_path jobworkerp); mem=$(repo_path memory-store); cond=$(repo_path conductor)
  want_repo jobworkerp   && install_file "${jwp}/target/release/all-in-one"       "${BIN_DIR}/all-in-one-${TRIPLE}"
  want_repo memory-store && install_file "${mem}/target/release/front"            "${BIN_DIR}/front-${TRIPLE}"
  want_repo conductor    && install_file "${cond}/target/release/conductor-main"  "${BIN_DIR}/conductor-main-${TRIPLE}"
  want_repo memory-store && install_file "${mem}/target/release/memories-import"  "${BIN_DIR}/memories-import-${TRIPLE}"
  fetch_protoc_bin "${TRIPLE}" "${BIN_DIR}/protoc-${TRIPLE}"
}

# CUDA runtime shared libraries the plugins link against (cudart, cublas, …).
# Deliberately EXCLUDES libcuda.so.1 and the libnvidia-* user-mode DRIVER libs:
# those must match the host's kernel module exactly, and bundling a build-image
# copy yields `CUDA_ERROR_SYSTEM_DRIVER_MISMATCH` at runtime. The driver is
# always backward compatible, so a CUDA 12-built plugin resolves the host's
# (newer) libcuda.so.1 fine — we only ship the toolkit runtime, never the driver.
CUDA_RUNTIME_SONAME_GLOBS=(
  'libcudart.so.*'
  'libcublas.so.*'
  'libcublasLt.so.*'
  'libnvrtc.so.*'
  'libnvrtc-builtins.so.*'
  'libcurand.so.*'
)

find_cuda_driver_stub() {
  local search_dirs=()
  [[ -n "${CUDA_DRIVER_STUB_DIR:-}" ]] && search_dirs+=("${CUDA_DRIVER_STUB_DIR}")
  search_dirs+=(
    /usr/local/cuda/lib64/stubs
    /usr/local/cuda/targets/x86_64-linux/lib/stubs
    /opt/cuda/lib64/stubs
    /usr/lib/x86_64-linux-gnu/stubs
  )

  local dir match
  for dir in "${search_dirs[@]}"; do
    [[ -d "${dir}" ]] || continue
    match=$(find -L "${dir}" -maxdepth 1 -type f -name 'libcuda.so' -print -quit 2>/dev/null || true)
    if [[ -n "${match}" ]]; then
      printf '%s\n' "${match}"
      return 0
    fi
  done
  return 1
}

prepare_cuda_driver_stub_dir() {
  [[ "${GPU}" == "cuda" && "${PLATFORM}" == "linux" && ",${BUNDLE}," == *",appimage,"* ]] || return 0
  if ldconfig -p 2>/dev/null | grep -q 'libcuda\.so\.1'; then
    return 0
  fi

  local stub tmp
  stub=$(find_cuda_driver_stub) \
    || die "libcuda.so.1 is not available and no CUDA driver stub was found; set CUDA_DRIVER_STUB_DIR for AppImage bundling"
  tmp=$(mktemp -d)
  ln -sf "${stub}" "${tmp}/libcuda.so.1"
  log "use CUDA driver stub for linuxdeploy dependency scan: ${stub}"
  printf '%s\n' "${tmp}"
}

# stage_cuda_runtime: copy the CUDA toolkit runtime libs into PLUGINS_DIR so the
# plugins' `$ORIGIN` RUNPATH resolves them WITHOUT relying on linuxdeploy's
# ldd-walk (which would also drag in the driver). Searches the standard CUDA
# toolkit lib dirs; CUDA_LIB_DIR overrides for a non-standard install.
stage_cuda_runtime() {
  [[ "${GPU}" == "cuda" ]] || return 0
  log "stage cuda runtime -> ${PLUGINS_DIR}"
  local search_dirs=()
  [[ -n "${CUDA_LIB_DIR:-}" ]] && search_dirs+=("${CUDA_LIB_DIR}")
  search_dirs+=(
    /usr/local/cuda/lib64
    /usr/local/cuda/targets/x86_64-linux/lib
    /opt/cuda/lib64
    /usr/lib/x86_64-linux-gnu
  )
  local glob dir match staged=0
  for glob in "${CUDA_RUNTIME_SONAME_GLOBS[@]}"; do
    local found=0
    for dir in "${search_dirs[@]}"; do
      [[ -d "${dir}" ]] || continue
      # Copy the real versioned file AND keep its soname symlink chain so the
      # plugin's DT_NEEDED (e.g. libcudart.so.12) resolves in PLUGINS_DIR.
      # `-L`: CUDA lib dirs are often symlinks (e.g. lib64 -> targets/.../lib),
      # which a symlink-unaware find would skip entirely.
      while IFS= read -r match; do
        [[ -e "${match}" ]] || continue
        _stage_cuda_lib_with_soname "${match}"
        found=1; staged=1
      done < <(find -L "${dir}" -maxdepth 1 -type f -name "${glob}" 2>/dev/null)
      [[ "${found}" == 1 ]] && break
    done
    [[ "${found}" == 1 ]] || warn "cuda runtime lib not found for glob: ${glob}"
  done
  [[ "${staged}" == 1 ]] || warn "no CUDA runtime libs staged; set CUDA_LIB_DIR"
}

# _stage_cuda_lib_with_soname SRC: install the real .so plus a symlink at its
# DT_SONAME (e.g. libcudart.so.12 -> libcudart.so.12.4.127) so DT_NEEDED resolves.
_stage_cuda_lib_with_soname() {
  local src=$1 base soname
  base=$(basename "${src}")
  install_file "${src}" "${PLUGINS_DIR}/${base}"
  if [[ "${DRY_RUN:-0}" != "1" ]]; then
    soname=$(objdump -p "${src}" 2>/dev/null | awk '/SONAME/{print $2}')
    if [[ -n "${soname}" && "${soname}" != "${base}" ]]; then
      run ln -sf "${base}" "${PLUGINS_DIR}/${soname}"
    fi
  fi
}

# stage_plugins: copy the 2 plugin libs workers actually use
# (LLMPromptRunner + MultimodalEmbeddingRunner), then the CUDA runtime they
# link against so the plugins are self-contained in PLUGINS_DIR.
stage_plugins() {
  log "stage plugins -> ${PLUGINS_DIR}"
  run mkdir -p "${PLUGINS_DIR}"
  local llama mm
  llama=$(repo_path llama); mm=$(repo_path mm)
  want_repo llama && install_file "${llama}/target/release/libjobworkerp_llama_cpp_plugin.${LIBEXT}" "${PLUGINS_DIR}/libjobworkerp_llama_cpp_plugin.${LIBEXT}"
  want_repo mm    && install_file "${mm}/target/release/libmm_embedding_runner.${LIBEXT}"            "${PLUGINS_DIR}/libmm_embedding_runner.${LIBEXT}"
  stage_cuda_runtime
  # On Linux, surface unresolved shared-library deps early (warn, non-fatal).
  # libcuda.so.1 is the host driver and must not ship. linuxdeploy still needs
  # it resolvable during AppImage dependency scanning, so build-release.sh adds
  # a CUDA stub via LD_LIBRARY_PATH for that phase only.
  if [[ "${LIBEXT}" == "so" && "${DRY_RUN:-0}" != "1" ]]; then
    local f
    for f in "${PLUGINS_DIR}"/*.so; do
      [[ -f "${f}" ]] || continue
      if ldd "${f}" 2>/dev/null | grep -v 'libcuda\.so' | grep -q 'not found'; then
        warn "unresolved shared libs in ${f} (excluding host-provided libcuda):"
        ldd "${f}" | grep 'not found' | grep -v 'libcuda\.so' >&2
      fi
    done
  fi
}

# sign_macos_plugins: sign staged plugin dylibs before Tauri packages the app.
# Tauri signs externalBin sidecars itself, but resource dylibs are safer when
# they already carry the same hardened-runtime Developer ID signature.
sign_macos_plugins() {
  [[ "${PLATFORM}" == "mac" ]] || return 0
  [[ "${DRY_RUN:-0}" == "1" ]] && { log "DRY_RUN: skip explicit macOS plugin signing"; return 0; }

  local dylibs=()
  local f
  for f in "${PLUGINS_DIR}"/*.dylib; do
    [[ -f "${f}" ]] || continue
    dylibs+=("${f}")
  done
  ((${#dylibs[@]} > 0)) || { warn "no macOS plugin dylibs staged for signing"; return 0; }

  if [[ -z "${APPLE_SIGNING_IDENTITY:-}" ]]; then
    warn "skip explicit macOS plugin signing: APPLE_SIGNING_IDENTITY is not set"
    return 0
  fi

  require_cmd codesign
  log "sign macOS plugin dylibs (${#dylibs[@]})"
  for f in "${dylibs[@]}"; do
    run codesign --force --options runtime --timestamp --sign "${APPLE_SIGNING_IDENTITY}" "${f}"
  done
}
