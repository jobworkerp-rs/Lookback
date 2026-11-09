#!/usr/bin/env bash
# Build and stage a pinned Linux/KVM runtime + coding-agent executor bundle.

set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
AGENT_APP=$(cd "${SCRIPT_DIR}/.." && pwd -P)
TARGET=""
RUNTIME_SOURCE=""
ARTIFACT_DIR=""
BUILD_ROOT=""
CREATED_BUILD_ROOT=0
STAGING_DIR=""

die() {
  printf 'error: %s\n' "$1" >&2
  exit 1
}

usage() {
  cat <<'EOF'
Usage: scripts/build-coding-agent-executor.sh --target x86_64-unknown-linux-gnu --runtime-source DIR [--artifact-dir DIR]

Requires explicit, reviewed source URL/ref/full-SHA pins:
  LOOKBACK_CODING_AGENT_APP_REPO_URL / _REF / _REVISION
  LOOKBACK_CODING_AGENT_RUNTIME_URL / _REF / _REVISION
  LOOKBACK_CODING_AGENT_CLIENT_REVISION

The source checkouts must already exist locally, match their pins, and be clean.
This command does not fetch, install toolchains, read .env files, or use the
user's Cargo/Rustup home. Cargo runs offline with CARGO_HOME under /tmp/opencode.
EOF
}

while (($# > 0)); do
  case "$1" in
    --target)
      [[ $# -ge 2 ]] || die "--target requires a value"
      TARGET=$2
      shift 2
      ;;
    --runtime-source)
      [[ $# -ge 2 ]] || die "--runtime-source requires a value"
      RUNTIME_SOURCE=$2
      shift 2
      ;;
    --artifact-dir)
      [[ $# -ge 2 ]] || die "--artifact-dir requires a value"
      ARTIFACT_DIR=$2
      shift 2
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *) die "unknown option" ;;
  esac
done

[[ -n "${TARGET}" && -n "${RUNTIME_SOURCE}" ]] \
  || { usage >&2; die "--target and --runtime-source are required"; }
[[ "$(uname -s)" == "Linux" ]] \
  || die "coding-agent runtime artifacts are Linux/KVM only; build the macOS UI artifact separately"
[[ "${TARGET}" == "x86_64-unknown-linux-gnu" ]] \
  || die "unsupported coding-agent executor target"

required_pins=(
  LOOKBACK_CODING_AGENT_APP_REPO_URL
  LOOKBACK_CODING_AGENT_APP_REF
  LOOKBACK_CODING_AGENT_APP_REVISION
  LOOKBACK_CODING_AGENT_RUNTIME_URL
  LOOKBACK_CODING_AGENT_RUNTIME_REF
  LOOKBACK_CODING_AGENT_RUNTIME_REVISION
  LOOKBACK_CODING_AGENT_CLIENT_REVISION
)
for pin in "${required_pins[@]}"; do
  [[ -n "${!pin:-}" ]] || die "coding-agent build requires explicit source URL, ref, and full SHA pins"
done

validate_revision() {
  [[ "$1" =~ ^([[:xdigit:]]{40}|[[:xdigit:]]{64})$ ]]
}

validate_ref() {
  local ref=$1 final_component
  final_component=${ref##*/}
  case "${ref}" in
    main|master|HEAD|latest|refs/heads/main|refs/heads/master|refs/tags/latest) \
      die "floating source refs are not accepted in coding-agent build mode" ;;
  esac
  case "${final_component}" in
    main|master|HEAD|latest) die "floating source refs are not accepted in coding-agent build mode" ;;
  esac
  GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null \
    git check-ref-format --branch "${ref}" >/dev/null 2>&1 \
    || die "configured source ref is invalid"
}

validate_url() {
  # This Node snippet intentionally uses JS template literals inside a single-quoted shell argument.
  # shellcheck disable=SC2016
  node -e '
    try {
      const raw = process.argv[1];
      const url = new URL(raw);
      if (!url.hostname || url.search || url.hash || url.password) process.exit(1);
      if (!["https:", "ssh:"].includes(url.protocol)) process.exit(1);
      if (!raw.startsWith(`${url.protocol}//`)) process.exit(1);
      const authority = raw.slice(raw.indexOf("://") + 3).split("/")[0];
      if (url.protocol === "https:" && authority.includes("@")) process.exit(1);
      if (url.protocol === "ssh:" && authority.includes("@") && !authority.startsWith("git@")) process.exit(1);
      if (!url.pathname.startsWith("/") || url.pathname.split("/").includes("..")) process.exit(1);
    } catch { process.exit(1); }
  ' "$1" >/dev/null 2>&1 || die "source URL must be a credential-free HTTPS/SSH URL"
}

for revision in \
  "${LOOKBACK_CODING_AGENT_APP_REVISION}" \
  "${LOOKBACK_CODING_AGENT_RUNTIME_REVISION}" \
  "${LOOKBACK_CODING_AGENT_CLIENT_REVISION}"; do
  validate_revision "${revision}" || die "source revision must be a full Git SHA"
done
validate_ref "${LOOKBACK_CODING_AGENT_APP_REF}"
validate_ref "${LOOKBACK_CODING_AGENT_RUNTIME_REF}"
validate_url "${LOOKBACK_CODING_AGENT_APP_REPO_URL}"
validate_url "${LOOKBACK_CODING_AGENT_RUNTIME_URL}"

git_safe() {
  local repository=$1
  shift
  GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_TERMINAL_PROMPT=0 \
    git -c core.fsmonitor=false -c core.hooksPath=/dev/null -C "${repository}" "$@"
}

verify_source_submodules_clean() {
  node --input-type=module - "$1" >/dev/null 2>&1 <<'NODE'
import fs from "node:fs";
import path from "node:path";
import { execFileSync } from "node:child_process";

const root = process.argv[2];
function git(repository, ...args) {
  return execFileSync("git", ["-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null", "-C", repository, ...args], {
    encoding: "buffer",
    env: { PATH: process.env.PATH, GIT_CONFIG_NOSYSTEM: "1", GIT_CONFIG_GLOBAL: "/dev/null", GIT_TERMINAL_PROMPT: "0" },
  });
}
function text(repository, ...args) {
  const output = git(repository, ...args).toString("utf8");
  return output.endsWith("\n") ? output.slice(0, -1) : output;
}
function safePath(value) {
  return value.length > 0 && value.length <= 512 && !path.isAbsolute(value) && !value.includes("\\") &&
    /^[A-Za-z0-9._/ -]+$/.test(value) && value.split("/").every((part) => part && part !== "." && part !== "..");
}
function gitlinks(repository) {
  return git(repository, "ls-tree", "-r", "-z", "HEAD").toString("utf8").split("\0").filter(Boolean).flatMap((record) => {
    const separator = record.indexOf("\t");
    if (separator < 0) throw new Error();
    const [mode, type, revision] = record.slice(0, separator).split(" ");
    if (mode !== "160000") return [];
    const submodulePath = record.slice(separator + 1);
    if (type !== "commit" || !/^(?:[a-f0-9]{40}|[a-f0-9]{64})$/.test(revision) || !safePath(submodulePath)) throw new Error();
    return [{ path: submodulePath, revision }];
  });
}
function visit(repository) {
  if (git(repository, "status", "--porcelain=v1", "--untracked-files=all", "--ignore-submodules=none").length !== 0) throw new Error();
  for (const entry of gitlinks(repository)) {
    let child = repository;
    for (const component of entry.path.split("/")) {
      child = path.join(child, component);
      if (fs.lstatSync(child).isSymbolicLink()) throw new Error();
    }
    if (!fs.statSync(child).isDirectory()) throw new Error();
    const canonicalChild = fs.realpathSync(child);
    if (fs.realpathSync(text(child, "rev-parse", "--show-toplevel")) !== canonicalChild ||
      text(child, "rev-parse", "HEAD") !== entry.revision) throw new Error();
    visit(child);
  }
}
try {
  visit(root);
} catch {
  process.exitCode = 1;
}
NODE
}

approved_path() {
  local input=$1
  [[ "${input}" == /* ]] || input="${AGENT_APP}/${input}"
  case "${input}" in
    "${AGENT_APP}"|"${AGENT_APP}"/*|/tmp/opencode|/tmp/opencode/*) ;;
    *) return 1 ;;
  esac
  case "${input}" in
    *"/../"*|*"/.."|*"/./"*|*"/."|*"//"*) return 1 ;;
  esac
  printf '%s\n' "${input}"
}

check_existing_path_components() {
  local input=$1 base remainder component cursor
  input=$(approved_path "${input}") || return 1
  case "${input}" in
    "${AGENT_APP}"|"${AGENT_APP}"/*) base=${AGENT_APP} ;;
    *) base=/tmp/opencode ;;
  esac
  [[ ! -L "${base}" ]] || return 1
  remainder=${input#"${base}"}
  cursor=${base}
  local -a components
  IFS=/ read -r -a components <<<"${remainder}"
  for component in "${components[@]}"; do
    [[ -n "${component}" ]] || continue
    cursor="${cursor}/${component}"
    [[ ! -L "${cursor}" ]] || return 1
    [[ -e "${cursor}" ]] || break
  done
  printf '%s\n' "${input}"
}

safe_directory() {
  local input=$1 approved canonical
  approved=$(check_existing_path_components "${input}") || return 1
  [[ -d "${approved}" ]] || return 1
  canonical=$(cd "${approved}" && pwd -P) || return 1
  [[ "${canonical}" == "${approved}" ]] || return 1
  printf '%s\n' "${canonical}"
}

safe_regular_source_file() {
  local relative=$2 current=$1 component index=0
  local -a components
  IFS=/ read -r -a components <<<"${relative}"
  for component in "${components[@]}"; do
    current="${current}/${component}"
    [[ ! -L "${current}" ]] || return 1
    index=$((index + 1))
    if [[ "${index}" -lt "${#components[@]}" ]]; then
      [[ -d "${current}" ]] || return 1
    else
      [[ -f "${current}" ]] || return 1
    fi
  done
}

reject_dotenv_inputs() {
  local repository=$1 dotenv_file
  dotenv_file=$(find "${repository}" \
    \( -type d \( -name .git -o -name target -o -name node_modules -o -name .build-deps \
      -o -name .pnpm-store -o -name artifacts -o -name dist \) -prune \) -o \
    \( \( -type f -o -type l \) -name .env -print -quit \))
  [[ -z "${dotenv_file}" ]] \
    || die "coding-agent source contains a .env file; builds refuse to expose it to Cargo"
}

verify_source_checkout() {
  local label=$1 repository=$2 configured_url=$3 ref=$4 revision=$5
  local origin head ref_commit status
  repository=$(safe_directory "${repository}") \
    || die "${label} source must be an existing checkout inside the approved development tree"
  git_safe "${repository}" rev-parse --show-toplevel >/dev/null 2>&1 \
    || die "${label} source is not a Git checkout"
  [[ "$(git_safe "${repository}" rev-parse --show-toplevel)" == "${repository}" ]] \
    || die "${label} source must be the root of its Git checkout"
  origin=$(git_safe "${repository}" remote get-url origin 2>/dev/null) \
    || die "${label} source has no origin remote"
  validate_url "${origin}"
  [[ "${origin}" == "${configured_url}" ]] \
    || die "${label} source origin does not match the configured pin"
  head=$(git_safe "${repository}" rev-parse HEAD) \
    || die "cannot read ${label} source revision"
  [[ "${head}" == "${revision}" ]] \
    || die "${label} checkout does not match the configured full SHA"
  ref_commit=$(git_safe "${repository}" rev-parse --verify --end-of-options "${ref}^{commit}" 2>/dev/null) \
    || die "${label} pinned ref is unavailable in the local checkout"
  git_safe "${repository}" merge-base --is-ancestor "${revision}" "${ref_commit}" >/dev/null 2>&1 \
    || die "${label} pinned ref does not contain the configured full SHA"
  status=$(git_safe "${repository}" status --porcelain --untracked-files=all --ignore-submodules=none) \
    || die "cannot verify ${label} source cleanliness"
  [[ -z "${status}" ]] \
    || die "${label} source tree is dirty; commit reviewed target sources before building"
  verify_source_submodules_clean "${repository}" \
    || die "${label} source tree is dirty or a submodule is uninitialized/mismatched"
  printf '%s\n' "${repository}"
}

APP_SOURCE=$(verify_source_checkout \
  "agent-app" "${AGENT_APP}" \
  "${LOOKBACK_CODING_AGENT_APP_REPO_URL}" \
  "${LOOKBACK_CODING_AGENT_APP_REF}" \
  "${LOOKBACK_CODING_AGENT_APP_REVISION}")
RUNTIME_SOURCE=$(verify_source_checkout \
  "jobworkerp runtime" "${RUNTIME_SOURCE}" \
  "${LOOKBACK_CODING_AGENT_RUNTIME_URL}" \
  "${LOOKBACK_CODING_AGENT_RUNTIME_REF}" \
  "${LOOKBACK_CODING_AGENT_RUNTIME_REVISION}")
reject_dotenv_inputs "${APP_SOURCE}"
reject_dotenv_inputs "${RUNTIME_SOURCE}"

[[ -f "${RUNTIME_SOURCE}/Cargo.toml" && ! -L "${RUNTIME_SOURCE}/Cargo.toml" ]] \
  || die "pinned runtime checkout has no regular Cargo.toml"
[[ -f "${APP_SOURCE}/Cargo.lock" && ! -L "${APP_SOURCE}/Cargo.lock" ]] \
  || die "pinned app source has no regular Cargo.lock"

if [[ -z "${ARTIFACT_DIR}" ]]; then
  ARTIFACT_DIR="${AGENT_APP}/artifacts/coding-agent/${TARGET}"
fi
ARTIFACT_DIR=$(approved_path "${ARTIFACT_DIR}") \
  || die "artifact output must stay inside the app checkout or /tmp/opencode"
artifact_parent=$(dirname "${ARTIFACT_DIR}")
artifact_parent=$(check_existing_path_components "${artifact_parent}") \
  || die "artifact output path must not traverse symlinks or leave the approved development tree"
mkdir -p "${artifact_parent}"
artifact_parent=$(safe_directory "${artifact_parent}") \
  || die "artifact output must stay inside the app checkout or /tmp/opencode"
ARTIFACT_DIR="${artifact_parent}/$(basename "${ARTIFACT_DIR}")"
[[ ! -e "${ARTIFACT_DIR}" && ! -L "${ARTIFACT_DIR}" ]] \
  || die "artifact output already exists; choose a new output directory"

BUILD_ROOT=$(mktemp -d /tmp/opencode/coding-agent-build.XXXXXX)
CREATED_BUILD_ROOT=1
mkdir -p "${BUILD_ROOT}/home" "${BUILD_ROOT}/target"
trap '[[ -z "${STAGING_DIR}" ]] || rm -rf "${STAGING_DIR}"; [[ "${CREATED_BUILD_ROOT}" != 1 ]] || rm -rf "${BUILD_ROOT}"' EXIT

RUSTUP_HOME=${RUSTUP_HOME:-}
[[ -n "${RUSTUP_HOME}" ]] \
  || die "set RUSTUP_HOME to an existing toolchain directory inside /tmp/opencode or the app checkout"
RUSTUP_HOME=$(safe_directory "${RUSTUP_HOME}") \
  || die "RUSTUP_HOME must be an existing directory inside /tmp/opencode or the app checkout"
CARGO_HOME=${LOOKBACK_CODING_AGENT_CARGO_HOME:-"${BUILD_ROOT}/cargo-home"}
if [[ ! -e "${CARGO_HOME}" ]]; then
  CARGO_HOME=$(check_existing_path_components "${CARGO_HOME}") \
    || die "Cargo cache must be inside /tmp/opencode or the app checkout without symlinked paths"
  mkdir -p "${CARGO_HOME}"
fi
CARGO_HOME=$(safe_directory "${CARGO_HOME}") \
  || die "Cargo cache must be inside /tmp/opencode or the app checkout"
[[ ! -e "${CARGO_HOME}/config" && ! -L "${CARGO_HOME}/config" \
  && ! -e "${CARGO_HOME}/config.toml" && ! -L "${CARGO_HOME}/config.toml" \
  && ! -e "${CARGO_HOME}/credentials" && ! -L "${CARGO_HOME}/credentials" \
  && ! -e "${CARGO_HOME}/credentials.toml" && ! -L "${CARGO_HOME}/credentials.toml" ]] \
  || die "coding-agent builds do not read Cargo config or credential files"
for config_file in \
  /tmp/opencode/.cargo/config /tmp/opencode/.cargo/config.toml \
  /tmp/.cargo/config /tmp/.cargo/config.toml \
  /.cargo/config /.cargo/config.toml; do
  [[ ! -e "${config_file}" && ! -L "${config_file}" ]] \
    || die "coding-agent builds do not read ambient Cargo configuration"
done

cargo_offline() {
  (
    cd "${BUILD_ROOT}/home"
    env -i \
      "PATH=${PATH:-/usr/bin:/bin}" \
      "HOME=${BUILD_ROOT}/home" \
      "CARGO_HOME=${CARGO_HOME}" \
      "CARGO_TARGET_DIR=${BUILD_ROOT}/target" \
      "RUSTUP_HOME=${RUSTUP_HOME}" \
      GIT_CONFIG_NOSYSTEM=1 \
      GIT_CONFIG_GLOBAL=/dev/null \
      GIT_TERMINAL_PROMPT=0 \
      CARGO_NET_OFFLINE=true \
      RUSTUP_AUTO_INSTALL=0 \
      cargo "$@"
  )
}

printf '%s\n' 'Building pinned jobworkerp runtime offline' >&2
cargo_offline build --offline --locked --release --target "${TARGET}" \
  --manifest-path "${RUNTIME_SOURCE}/Cargo.toml" \
  -p jobworkerp-main --bin all-in-one \
  >/dev/null
printf '%s\n' 'Building coding-agent executor offline' >&2
cargo_offline build --offline --locked --release --target "${TARGET}" \
  --manifest-path "${APP_SOURCE}/coding-agent-executor/Cargo.toml" \
  -p coding-agent-executor --bin coding-agent-executor \
  >/dev/null

runtime_binary="${BUILD_ROOT}/target/${TARGET}/release/all-in-one"
executor_binary="${BUILD_ROOT}/target/${TARGET}/release/coding-agent-executor"
[[ -f "${runtime_binary}" && ! -L "${runtime_binary}" ]] \
  || die "pinned runtime build did not produce the expected binary"
[[ -f "${executor_binary}" && ! -L "${executor_binary}" ]] \
  || die "executor build did not produce the expected binary"

STAGING_DIR=$(mktemp -d "${artifact_parent}/.coding-agent-artifacts.XXXXXX")
mkdir -p "${STAGING_DIR}/runtime" "${STAGING_DIR}/bin"
install -m 755 "${runtime_binary}" "${STAGING_DIR}/runtime/all-in-one"
install -m 755 "${executor_binary}" "${STAGING_DIR}/bin/coding-agent-executor"

# The manifest writer is shared with development staging; release mode passes
# every pin explicitly and refuses dirty or floating sources.
env -i \
  "PATH=${PATH:-/usr/bin:/bin}" \
  "HOME=${BUILD_ROOT}/home" \
  node "${SCRIPT_DIR}/lib/coding-agent-manifest.mjs" \
  --app-root "${APP_SOURCE}" \
  --runtime-root "${RUNTIME_SOURCE}" \
  --artifact-root "${STAGING_DIR}" \
  --target "${TARGET}" \
  --app-url "${LOOKBACK_CODING_AGENT_APP_REPO_URL}" \
  --app-ref "${LOOKBACK_CODING_AGENT_APP_REF}" \
  --app-revision "${LOOKBACK_CODING_AGENT_APP_REVISION}" \
  --runtime-url "${LOOKBACK_CODING_AGENT_RUNTIME_URL}" \
  --runtime-ref "${LOOKBACK_CODING_AGENT_RUNTIME_REF}" \
  --runtime-revision "${LOOKBACK_CODING_AGENT_RUNTIME_REVISION}" \
  --client-revision "${LOOKBACK_CODING_AGENT_CLIENT_REVISION}" \
  >/dev/null

(cd "${AGENT_APP}" && node "${SCRIPT_DIR}/verify-coding-agent-artifacts.mjs" \
  --manifest "${STAGING_DIR}/coding-agent-artifact-manifest.json" \
  --artifact-root "${STAGING_DIR}" \
  --config-root "${APP_SOURCE}" --provenance-only --json) >/dev/null \
  || die "staged coding-agent artifact failed provenance verification; feature remains disabled"

mv "${STAGING_DIR}" "${ARTIFACT_DIR}"
STAGING_DIR=""
(cd "${AGENT_APP}" && node "${SCRIPT_DIR}/verify-coding-agent-artifacts.mjs" \
  --manifest "${ARTIFACT_DIR}/${MANIFEST_NAME:-coding-agent-artifact-manifest.json}" \
  --artifact-root "${ARTIFACT_DIR}" \
  --config-root "${APP_SOURCE}" --provenance-only --json) >/dev/null \
  || die "published coding-agent artifact failed post-stage provenance verification"

printf 'Coding-agent artifact staged at %s; installed runtime compatibility remains unverified and execution remains disabled.\n' \
  "${ARTIFACT_DIR}"
