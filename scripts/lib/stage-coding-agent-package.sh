#!/usr/bin/env bash
# Stage the development coding-agent package next to the other dev externals.
#
# Sourced by scripts/stage-dev-external-bins.sh after scripts/lib/build-common.sh.
# The app resolves its package at `<resource_dir>/coding-agent/`; during
# `tauri dev` the resource directory is the cargo target directory, so the
# package is written there directly and never enters a release bundle.
#
# Inputs (shell variables): AGENT_APP, TRIPLE, JOBWORKERP_BIN, DRY_RUN.
# Optional: TRIPLE_EXPLICIT=1 when `--triple` was passed (cargo then uses
# `target/<triple>/debug`), CARGO_TARGET_DIR, and
# LOOKBACK_CODING_AGENT_RUNTIME_SOURCE_DIR (jobworkerp runtime checkout; the
# sibling checkout that contains agent-app is the default).

CODING_AGENT_DEV_TARGET=x86_64-unknown-linux-gnu

coding_agent_dev_target_dir() {
  local target_dir="${CARGO_TARGET_DIR:-${AGENT_APP}/target}"
  if [[ "${TRIPLE_EXPLICIT:-0}" == "1" ]]; then
    target_dir="${target_dir}/${TRIPLE}"
  fi
  printf '%s\n' "${target_dir}/debug"
}

coding_agent_dev_package_dir() {
  printf '%s/coding-agent\n' "$(coding_agent_dev_target_dir)"
}

coding_agent_runtime_source_dir() {
  if [[ -n "${LOOKBACK_CODING_AGENT_RUNTIME_SOURCE_DIR:-}" ]]; then
    printf '%s\n' "${LOOKBACK_CODING_AGENT_RUNTIME_SOURCE_DIR}"
  else
    (cd "${AGENT_APP}/.." && pwd -P)
  fi
}

# The verifier only inspects artifact roots below the working directory or
# /tmp/opencode; a relocated CARGO_TARGET_DIR is staged but left unverified.
coding_agent_verify_dev_package() {
  local package_dir=$1
  case "${package_dir}" in
    "${AGENT_APP}"/*|/tmp/opencode/*) ;;
    *)
      warn "coding-agent package at ${package_dir} is outside the verifier's allowed roots; skipping verification"
      return 0
      ;;
  esac
  mkdir -p /tmp/opencode
  (cd "${AGENT_APP}" && node "${AGENT_APP}/scripts/verify-coding-agent-artifacts.mjs" \
    --manifest "${package_dir}/coding-agent-artifact-manifest.json" \
    --artifact-root "${package_dir}" \
    --config-root "${AGENT_APP}" \
    --provenance-only --allow-development-sources --json) >/dev/null \
    || die "staged development coding-agent package failed provenance verification"
}

stage_coding_agent_package() {
  local package_dir executor manifest staged_manifest runtime_source
  if [[ "$(uname -s)" != "Linux" || "${TRIPLE}" != "${CODING_AGENT_DEV_TARGET}" ]]; then
    log "coding-agent package is Linux ${CODING_AGENT_DEV_TARGET} only; skipping for ${TRIPLE}"
    return 0
  fi
  package_dir=$(coding_agent_dev_package_dir)
  executor="$(coding_agent_dev_target_dir)/coding-agent-executor"
  runtime_source=$(coding_agent_runtime_source_dir)
  if [[ "${DRY_RUN:-0}" == "1" ]]; then
    printf 'would build coding-agent-executor and stage coding-agent package -> %s\n' "${package_dir}"
    return 0
  fi
  [[ -d "${runtime_source}/.git" || -f "${runtime_source}/.git" ]] \
    || die "jobworkerp runtime checkout not found at ${runtime_source}; set LOOKBACK_CODING_AGENT_RUNTIME_SOURCE_DIR"

  local -a cargo_args=(build -p coding-agent-executor --manifest-path "${AGENT_APP}/Cargo.toml")
  [[ "${TRIPLE_EXPLICIT:-0}" != "1" ]] || cargo_args+=(--target "${TRIPLE}")
  run cargo "${cargo_args[@]}"
  [[ -f "${executor}" ]] || die "coding-agent-executor build output not found at ${executor}"

  # The native bootstrap rejects group/world-writable package directories, so
  # the modes are set explicitly instead of trusting the umask.
  install -d -m 755 "${package_dir}" "${package_dir}/bin" "${package_dir}/runtime"
  chmod 755 "${package_dir}" "${package_dir}/bin" "${package_dir}/runtime"
  install_file "${JOBWORKERP_BIN}" "${package_dir}/runtime/all-in-one"
  install_file "${executor}" "${package_dir}/bin/coding-agent-executor"

  manifest="${package_dir}/coding-agent-artifact-manifest.json"
  staged_manifest="${package_dir}/.coding-agent-artifact-manifest.staged.json"
  node "${AGENT_APP}/scripts/lib/coding-agent-manifest.mjs" \
    --app-root "${AGENT_APP}" \
    --runtime-root "${runtime_source}" \
    --artifact-root "${package_dir}" \
    --target "${TRIPLE}" \
    --allow-development-sources \
    --output "${staged_manifest}" \
    || die "could not record development coding-agent provenance"
  if [[ -f "${manifest}" ]] && cmp -s "${staged_manifest}" "${manifest}"; then
    rm -f "${staged_manifest}"
    log "staged coding-agent package is current: ${package_dir}"
  else
    mv -f "${staged_manifest}" "${manifest}"
    log "staged coding-agent package: ${package_dir}"
  fi
  coding_agent_verify_dev_package "${package_dir}"
}
