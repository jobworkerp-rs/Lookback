#!/usr/bin/env bash
# Fetch the official prebuilt protoc and stage it as a Tauri externalBin.
#
# Sourced (not executed) by scripts/stage-dev-external-bins.sh (dev) and
# scripts/lib/build-deps.sh (release). Relies on build-common.sh helpers
# (log/warn/die/run) being already sourced by the caller.
#
# Why the OFFICIAL release tarball instead of `$(which protoc)`:
# Homebrew's protoc dynamically links @rpath/libprotoc.dylib, libprotobuf.dylib
# and ~70 absl dylibs, so copying the bare binary into the app bundle yields a
# `dyld: Library not loaded` crash when a child process spawns it to compile
# runner_settings_proto at worker-registration time. The protobuf project's
# prebuilt protoc depends only on macOS system libraries (libSystem,
# CoreFoundation, libc++) — it is a self-contained single binary that runs with
# no extra dylibs, so no install_name_tool/codesign/bundle gymnastics are needed.

# Pinned protoc version. Override with LOOKBACK_PROTOC_VERSION for an offline
# mirror or to track a child's protobuf bump. The runtime protoc only has to
# compile proto3, so an exact match with the children's tonic-prost-build is not
# required.
PROTOC_VERSION="${LOOKBACK_PROTOC_VERSION:-35.1}"

# protoc_asset_name TRIPLE -> the GitHub release asset filename for that target.
protoc_asset_name() {
  local triple=$1
  case "${triple}" in
    aarch64-apple-darwin)        printf 'protoc-%s-osx-aarch_64.zip\n'   "${PROTOC_VERSION}" ;;
    x86_64-apple-darwin)         printf 'protoc-%s-osx-x86_64.zip\n'     "${PROTOC_VERSION}" ;;
    x86_64-unknown-linux-gnu)    printf 'protoc-%s-linux-x86_64.zip\n'   "${PROTOC_VERSION}" ;;
    aarch64-unknown-linux-gnu)   printf 'protoc-%s-linux-aarch_64.zip\n' "${PROTOC_VERSION}" ;;
    *) die "no official protoc asset for triple: ${triple}" ;;
  esac
}

# _protoc_reports_pinned_version BIN -> 0 if BIN runs and prints the pinned
# version. A broken bundled copy (dyld error) returns non-zero and is treated as
# absent, forcing a re-fetch.
_protoc_reports_pinned_version() {
  local bin=$1
  [[ -x "${bin}" ]] || return 1
  "${bin}" --version 2>/dev/null | grep -qx "libprotoc ${PROTOC_VERSION}"
}

# fetch_protoc_bin TRIPLE DEST_PATH
# Idempotently place a self-contained official protoc at DEST_PATH. Skips the
# download when DEST_PATH already runs and reports the pinned version.
fetch_protoc_bin() {
  local triple=$1 dest=$2
  local asset url tmp
  asset=$(protoc_asset_name "${triple}")
  url="https://github.com/protocolbuffers/protobuf/releases/download/v${PROTOC_VERSION}/${asset}"

  if [[ "${DRY_RUN:-0}" == "1" ]]; then
    run curl -fsSL "${url}" -o "<tmp>/${asset}"
    run unzip -q "<tmp>/${asset}" -d "<tmp>"
    run install -m755 "<tmp>/bin/protoc" "${dest}"
    return 0
  fi

  if _protoc_reports_pinned_version "${dest}"; then
    log "protoc ${PROTOC_VERSION} already staged: ${dest}"
    return 0
  fi

  tmp=$(mktemp -d)
  # shellcheck disable=SC2064  # expand tmp now so the trap cleans the right dir.
  trap "rm -rf '${tmp}'" RETURN

  log "fetch official protoc ${PROTOC_VERSION} for ${triple}"
  curl -fsSL "${url}" -o "${tmp}/${asset}" || die "download failed: ${url}"
  unzip -q "${tmp}/${asset}" -d "${tmp}" || die "unzip failed: ${tmp}/${asset}"
  [[ -f "${tmp}/bin/protoc" ]] || die "asset missing bin/protoc: ${asset}"
  mkdir -p "$(dirname "${dest}")"
  install -m755 "${tmp}/bin/protoc" "${dest}"

  _protoc_reports_pinned_version "${dest}" \
    || die "staged protoc does not run / version mismatch: ${dest}"
}
