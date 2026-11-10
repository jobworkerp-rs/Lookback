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

# Logical repos: jobworkerp memory-store conductor llama mm issue-tracker-server.
# `memory-store` remains the build/cache identifier used by existing pipelines;
# its current Memories source is the LLM-Memories repository.
# macOS ships bash 3.2 (no associative arrays), so URL/dir lookups are
# case-based rather than `declare -A`.

MEMORIES_DEFAULT_REPOSITORY="https://github.com/jobworkerp-rs/memory-store.git"
MEMORIES_DEFAULT_REF="v0.0.24"
MEMORIES_DEFAULT_REVISION="8d365fc792489a98035bb11aaa3fe7e040a1177f"
ISSUE_TRACKER_DEFAULT_REPOSITORY="https://github.com/sutr-app/issue-tracker-server.git"
ISSUE_TRACKER_DEFAULT_REF="v0.0.1"
ISSUE_TRACKER_DEFAULT_REVISION="5974330a31ae19fb04db99841be73f305958d0d8"

# shellcheck source=verified-migration-bundle.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/verified-migration-bundle.sh"

# repo_url NAME -> git URL. The release dependencies use their public origins.
repo_url() {
  case "$1" in
    jobworkerp)
      if [[ "${LOOKBACK_CODING_AGENT_BUILD:-0}" == "1" ]]; then
        echo "${LOOKBACK_CODING_AGENT_RUNTIME_URL:-}"
      else
        echo "https://github.com/jobworkerp-rs/jobworkerp-rs"
      fi
      ;;
    memory-store) echo "${LOOKBACK_MEMORY_STORE_REPO_URL:-${MEMORIES_DEFAULT_REPOSITORY}}" ;;
    conductor)    echo "https://github.com/jobworkerp-rs/jobworkerp-conductor" ;;
    llama)        echo "https://github.com/jobworkerp-rs/llama-cpp-runner" ;;
    mm)           echo "https://github.com/jobworkerp-rs/mm-embedding-runner" ;;
    issue-tracker-server) echo "${LOOKBACK_ISSUE_TRACKER_SERVER_REPO_URL:-${ISSUE_TRACKER_DEFAULT_REPOSITORY}}" ;;
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
    issue-tracker-server) echo "issue-tracker-server" ;;
    *) die "unknown repo: $1" ;;
  esac
}

# repo_branch NAME -> ref to clone for a build dependency. Keep jobworkerp on
# upstream main by default so packaged sidecars receive compatible runner fixes;
# release or compatibility jobs can still select an explicit ref.
repo_branch() {
  case "$1" in
    jobworkerp)
      if [[ "${LOOKBACK_CODING_AGENT_BUILD:-0}" == "1" ]]; then
        echo "${LOOKBACK_CODING_AGENT_RUNTIME_REF:-}"
      else
        echo "${LOOKBACK_JOBWORKERP_REF:-main}"
      fi
      ;;
    memory-store) echo "${LOOKBACK_MEMORY_STORE_REF:-${MEMORIES_DEFAULT_REF}}" ;;
    issue-tracker-server) echo "${LOOKBACK_ISSUE_TRACKER_SERVER_REF:-${ISSUE_TRACKER_DEFAULT_REF}}" ;;
    *) echo "main" ;;
  esac
}

repo_revision() {
  case "$1" in
    jobworkerp)
      [[ "${LOOKBACK_CODING_AGENT_BUILD:-0}" == "1" ]] \
        && echo "${LOOKBACK_CODING_AGENT_RUNTIME_REVISION:-}" \
        || echo ""
      ;;
    memory-store) echo "${LOOKBACK_MEMORY_STORE_REVISION:-${MEMORIES_DEFAULT_REVISION}}" ;;
    issue-tracker-server) echo "${LOOKBACK_ISSUE_TRACKER_SERVER_REVISION:-${ISSUE_TRACKER_DEFAULT_REVISION}}" ;;
    *) echo "" ;;
  esac
}

# Defaults name immutable release tags. Explicit ref overrides retain the
# historical branch workflow unless the caller explicitly supplies refs/tags/.
repo_ref_kind() {
  local name=$1 ref override_name
  case "${name}" in
    memory-store)
      override_name=LOOKBACK_MEMORY_STORE_REF
      ;;
    issue-tracker-server)
      override_name=LOOKBACK_ISSUE_TRACKER_SERVER_REF
      ;;
    *) echo "branch"; return 0 ;;
  esac
  ref=${!override_name:-}
  if [[ -z "${ref}" ]]; then
    echo "tag"
  elif [[ "${ref}" == refs/tags/* ]]; then
    echo "tag"
  else
    echo "branch"
  fi
}

pinned_ref_name() {
  local ref=$1 kind=$2
  case "${kind}:${ref}" in
    tag:refs/tags/*) printf '%s\n' "${ref}" ;;
    tag:*) printf 'refs/tags/%s\n' "${ref}" ;;
    branch:refs/heads/*) printf '%s\n' "${ref}" ;;
    branch:*) printf 'refs/heads/%s\n' "${ref}" ;;
    *) return 1 ;;
  esac
}

# Resolve a pinned ref to the fetched remote-tracking branch when available.
# Cached clones can retain a stale local branch with the same name; that local
# ref must not shadow the branch just fetched from the configured origin.
pinned_dependency_ref_name() {
  local dir=$1 ref=$2 kind=$3 ref_name remote_ref
  ref_name=$(pinned_ref_name "${ref}" "${kind}") || return 1
  if [[ "${kind}" == "branch" ]] && is_own_git_repo "${dir}"; then
    remote_ref="refs/remotes/origin/${ref#refs/heads/}"
    if git -C "${dir}" show-ref --verify --quiet "${remote_ref}"; then
      ref_name=${remote_ref}
    fi
  fi
  printf '%s\n' "${ref_name}"
}

coding_agent_build_enabled() {
  [[ "${LOOKBACK_CODING_AGENT_BUILD:-0}" == "1" ]]
}

# Coding artifacts are an opt-in Linux/KVM build, not the UI's externalBin set.
# Require local reviewed inputs so this path never fetches mutable source refs.
validate_coding_agent_build_config() {
  coding_agent_build_enabled || return 0
  local name value
  for name in \
    LOOKBACK_CODING_AGENT_RUNTIME_URL \
    LOOKBACK_CODING_AGENT_RUNTIME_REF \
    LOOKBACK_CODING_AGENT_RUNTIME_REVISION \
    LOOKBACK_CODING_AGENT_APP_REPO_URL \
    LOOKBACK_CODING_AGENT_APP_REF \
    LOOKBACK_CODING_AGENT_APP_REVISION \
    LOOKBACK_CODING_AGENT_CLIENT_REVISION; do
    value=${!name:-}
    [[ -n "${value}" ]] \
      || die "coding-agent build requires explicit source URL, ref, and full SHA pins"
  done
  [[ "${SKIP_CLONE:-0}" == "1" ]] \
    || die "coding-agent build requires --skip-clone; source checkout must already be pinned locally"
  [[ "${ONLY_REPOS:-}" == "jobworkerp" ]] \
    || die "coding-agent build requires --only jobworkerp to avoid unrelated dependency builds"
  [[ -z "${PLATFORM:-}" || "${PLATFORM}" == "linux" ]] \
    || die "coding-agent executor artifacts require a Linux/KVM target"
  [[ -z "${TRIPLE:-}" || "${TRIPLE}" == "x86_64-unknown-linux-gnu" ]] \
    || die "unsupported coding-agent executor target"
  [[ "${LOOKBACK_CODING_AGENT_RUNTIME_REF}" != "main" \
    && "${LOOKBACK_CODING_AGENT_RUNTIME_REF}" != "master" \
    && "${LOOKBACK_CODING_AGENT_RUNTIME_REF}" != "HEAD" \
    && "${LOOKBACK_CODING_AGENT_RUNTIME_REF}" != "latest" ]] \
    || die "floating source refs are not accepted in coding-agent build mode"
  [[ "${LOOKBACK_CODING_AGENT_APP_REF}" != "main" \
    && "${LOOKBACK_CODING_AGENT_APP_REF}" != "master" \
    && "${LOOKBACK_CODING_AGENT_APP_REF}" != "HEAD" \
    && "${LOOKBACK_CODING_AGENT_APP_REF}" != "latest" ]] \
    || die "floating source refs are not accepted in coding-agent build mode"
  [[ "${LOOKBACK_CODING_AGENT_RUNTIME_REVISION}" =~ ^([[:xdigit:]]{40}|[[:xdigit:]]{64})$ \
    && "${LOOKBACK_CODING_AGENT_APP_REVISION}" =~ ^([[:xdigit:]]{40}|[[:xdigit:]]{64})$ \
    && "${LOOKBACK_CODING_AGENT_CLIENT_REVISION}" =~ ^([[:xdigit:]]{40}|[[:xdigit:]]{64})$ ]] \
    || die "coding-agent source revisions must be full Git SHAs"
  node -e '
    for (const raw of process.argv.slice(1)) {
      try {
        const url = new URL(raw);
        if (!url.hostname || url.password || url.search || url.hash) process.exit(1);
        if (!["https:", "ssh:"].includes(url.protocol)) process.exit(1);
        if (!raw.startsWith(`${url.protocol}//`)) process.exit(1);
        const authority = raw.slice(raw.indexOf("://") + 3).split("/")[0];
        if (url.protocol === "https:" && authority.includes("@")) process.exit(1);
        if (url.protocol === "ssh:" && authority.includes("@") && !authority.startsWith("git@")) process.exit(1);
      } catch { process.exit(1); }
    }
  ' "${LOOKBACK_CODING_AGENT_RUNTIME_URL}" "${LOOKBACK_CODING_AGENT_APP_REPO_URL}" \
    || die "coding-agent source URLs must be credential-free HTTPS/SSH URLs"
}

coding_agent_git() {
  local dir=$1
  shift
  GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_TERMINAL_PROMPT=0 \
    git -c core.fsmonitor=false -c core.hooksPath=/dev/null -C "${dir}" "$@"
}

coding_agent_git_tree_is_clean() {
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

validate_pinned_dependency_config() {
  local name=$1 ref revision url override_url_name override_ref_name
  case "${name}" in
    memory-store)
      override_url_name=LOOKBACK_MEMORY_STORE_REPO_URL
      override_ref_name=LOOKBACK_MEMORY_STORE_REF
      ;;
    issue-tracker-server)
      override_url_name=LOOKBACK_ISSUE_TRACKER_SERVER_REPO_URL
      override_ref_name=LOOKBACK_ISSUE_TRACKER_SERVER_REF
      ;;
    *) die "unknown pinned dependency: ${name}" ;;
  esac
  ref=$(repo_branch "${name}")
  revision=$(repo_revision "${name}")
  url=$(repo_url "${name}")
  [[ "${revision}" =~ ^[[:xdigit:]]{40}$ ]] \
    || die "${name} revision must be a full 40-character Git SHA-1"
  local ref_name ref_kind
  ref_kind=$(repo_ref_kind "${name}")
  ref_name=$(pinned_ref_name "${ref}" "${ref_kind}") \
    || die "${name} ref is invalid"
  git check-ref-format "${ref_name}" >/dev/null 2>&1 \
    || die "${name} ref is invalid"

  if [[ -n "${!override_url_name:-}" || -n "${!override_ref_name:-}" ]]; then
    if [[ "${name}" == "memory-store" ]]; then
      [[ -n "${LOOKBACK_MEMORY_STORE_REF:-}" && -n "${LOOKBACK_MEMORY_STORE_REVISION:-}" ]] \
        || die "custom Memories source requires LOOKBACK_MEMORY_STORE_REF and LOOKBACK_MEMORY_STORE_REVISION"
    elif [[ "${name}" == "issue-tracker-server" ]]; then
      [[ -n "${LOOKBACK_ISSUE_TRACKER_SERVER_REF:-}" && -n "${LOOKBACK_ISSUE_TRACKER_SERVER_REVISION:-}" ]] \
        || die "custom Issue Tracker source requires LOOKBACK_ISSUE_TRACKER_SERVER_REF and LOOKBACK_ISSUE_TRACKER_SERVER_REVISION"
    fi
  fi
  [[ -n "${url}" ]] || die "${name} repository URL must not be empty"
  validate_credential_free_git_url "${url}" \
    || die "${name} repository URL must be a credential-free HTTPS/SSH URL"
}

validate_credential_free_git_url() {
  node -e '
    const raw = process.argv[1];
    const scp = raw.match(/^(?:([^@/]+)@)?([^/:]+):([^?#]+)$/);
    if (scp && !raw.includes("://")) {
      if (scp[1] && scp[1] !== "git") process.exit(1);
      if (scp[3].includes("@") || /[?#]/.test(scp[3])) process.exit(1);
      process.exit(0);
    }
    try {
      const url = new URL(raw);
      if (!["https:", "ssh:"].includes(url.protocol) || !url.hostname ||
          url.password || url.search || url.hash) process.exit(1);
      if (url.protocol === "https:" && url.username) process.exit(1);
      if (url.protocol === "ssh:" && url.username && url.username !== "git") process.exit(1);
    } catch { process.exit(1); }
  ' "$1"
}

verify_pinned_dependency_checkout() {
  local dir=$1 expected_url=$2 ref=$3 revision=$4 kind=$5
  local actual_origin expected_origin head resolved ref_name ref_type worktree
  [[ -d "${dir}" ]] || die "pinned dependency source directory not found"
  is_own_git_repo "${dir}" || die "pinned dependency source must be the root of a Git checkout"
  [[ "${revision}" =~ ^[[:xdigit:]]{40}$ ]] \
    || die "pinned dependency revision must be a full 40-character Git SHA-1"
  ref_name=$(pinned_dependency_ref_name "${dir}" "${ref}" "${kind}") \
    || die "pinned dependency ref is invalid"
  git check-ref-format "${ref_name}" >/dev/null 2>&1 \
    || die "pinned dependency ref is invalid"

  actual_origin=$(git -C "${dir}" remote get-url origin 2>/dev/null) \
    || die "pinned dependency checkout has no origin remote"
  validate_credential_free_git_url "${actual_origin}" \
    || die "pinned dependency origin must be a credential-free HTTPS/SSH URL"
  actual_origin=$(safe_git_remote_url "${actual_origin}") \
    || die "pinned dependency origin is not a remote HTTPS/SSH URL"
  expected_origin=$(safe_git_remote_url "${expected_url}") \
    || die "configured pinned dependency URL is invalid"
  [[ "${actual_origin}" == "${expected_origin}" ]] \
    || die "pinned dependency checkout origin does not match its configured public source; use a separate --workdir"

  if [[ "${kind}" == "tag" ]]; then
    ref_type=$(git -C "${dir}" cat-file -t "${ref_name}" 2>/dev/null) \
      || die "pinned dependency release tag is missing: ${ref}"
    [[ "${ref_type}" == "tag" ]] \
      || die "pinned dependency release ref must be an annotated Git tag: ${ref}"
  fi
  resolved=$(git -C "${dir}" rev-parse --verify --end-of-options "${ref_name}^{commit}" 2>/dev/null) \
    || die "pinned dependency ref is unavailable in the checkout: ${ref}"
  if [[ "${kind}" == "tag" ]]; then
    [[ "${resolved}" == "${revision}" ]] \
      || die "pinned dependency tag does not peel to its configured full SHA"
  else
    git -C "${dir}" merge-base --is-ancestor "${revision}" "${resolved}" >/dev/null 2>&1 \
      || die "pinned dependency branch does not contain its configured full SHA"
  fi
  head=$(git -C "${dir}" rev-parse HEAD 2>/dev/null) \
    || die "cannot read pinned dependency source revision"
  [[ "${head}" == "${revision}" ]] \
    || die "pinned dependency checkout HEAD does not match its configured full SHA"
  worktree=$(git -C "${dir}" status --porcelain --untracked-files=all --ignore-submodules=none 2>/dev/null) \
    || die "cannot verify pinned dependency source cleanliness"
  [[ -z "${worktree}" ]] \
    || die "pinned dependency source tree is dirty; use a clean pinned checkout"
}

verify_fetched_pinned_dependency_ref() {
  local dir=$1 ref=$2 revision=$3 kind=$4
  local ref_name resolved ref_type
  ref_name=$(pinned_ref_name "${ref}" "${kind}") \
    || die "pinned dependency ref is invalid"
  if [[ "${kind}" == "branch" ]]; then
    ref_name="refs/remotes/origin/${ref#refs/heads/}"
  else
    ref_type=$(git -C "${dir}" cat-file -t "${ref_name}" 2>/dev/null) \
      || die "pinned dependency release tag is missing: ${ref}"
    [[ "${ref_type}" == "tag" ]] \
      || die "pinned dependency release ref must be an annotated Git tag: ${ref}"
  fi
  resolved=$(git -C "${dir}" rev-parse --verify --end-of-options "${ref_name}^{commit}" 2>/dev/null) \
    || die "pinned dependency ref is unavailable: ${ref}"
  if [[ "${kind}" == "tag" ]]; then
    [[ "${resolved}" == "${revision}" ]] \
      || die "pinned dependency tag does not peel to its configured full SHA"
  else
    git -C "${dir}" merge-base --is-ancestor "${revision}" "${resolved}" >/dev/null 2>&1 \
      || die "pinned dependency branch does not contain its configured full SHA"
  fi
}

checkout_pinned_dependency() {
  local dir=$1 url=$2 ref=$3 revision=$4 kind=$5 skip_clone=${6:-0}
  local expected_origin actual_origin ref_name fetch_ref
  [[ "${kind}" == "tag" || "${kind}" == "branch" ]] \
    || die "pinned dependency ref kind must be tag or branch"
  ref_name=$(pinned_ref_name "${ref}" "${kind}") \
    || die "pinned dependency ref is invalid"
  git check-ref-format "${ref_name}" >/dev/null 2>&1 \
    || die "pinned dependency ref is invalid"
  expected_origin=$(safe_git_remote_url "${url}") \
    || die "configured pinned dependency URL is invalid"
  if [[ "${DRY_RUN:-0}" == "1" ]]; then
    if is_own_git_repo "${dir}"; then
      verify_pinned_dependency_checkout "${dir}" "${url}" "${ref}" "${revision}" "${kind}"
    else
      log "DRY_RUN: would fetch ${url} ${ref_name} at ${revision}"
    fi
    return 0
  fi

  if is_own_git_repo "${dir}"; then
    actual_origin=$(git -C "${dir}" remote get-url origin 2>/dev/null) \
      || die "pinned dependency checkout has no origin remote"
    validate_credential_free_git_url "${actual_origin}" \
      || die "pinned dependency origin must be a credential-free HTTPS/SSH URL"
    actual_origin=$(safe_git_remote_url "${actual_origin}") \
      || die "pinned dependency checkout origin is not a remote HTTPS/SSH URL"
    [[ "${actual_origin}" == "${expected_origin}" ]] \
      || die "existing pinned dependency clone origin does not match the configured source; use a separate --workdir"
    local worktree
    worktree=$(git -C "${dir}" status --porcelain --untracked-files=all --ignore-submodules=none 2>/dev/null) \
      || die "cannot verify pinned dependency source cleanliness"
    [[ -z "${worktree}" ]] \
      || die "pinned dependency source tree is dirty; refusing to overwrite existing changes"
  elif [[ -e "${dir}" ]]; then
    log "init-in-place pinned dependency source -> ${dir} (preserve restored build cache)"
    git -C "${dir}" init -q 2>/dev/null \
      || die "could not initialize cached pinned dependency source directory"
    git -C "${dir}" remote add origin "${url}" 2>/dev/null \
      || die "could not configure pinned dependency origin"
  else
    mkdir -p "$(dirname "${dir}")"
    git init -q "${dir}" 2>/dev/null \
      || die "could not create pinned dependency source checkout"
    git -C "${dir}" remote add origin "${url}" 2>/dev/null \
      || die "could not configure pinned dependency origin"
  fi

  if [[ "${skip_clone}" == "1" ]]; then
    verify_pinned_dependency_checkout "${dir}" "${url}" "${ref}" "${revision}" "${kind}"
    return 0
  fi

  if [[ "${kind}" == "tag" ]]; then
    fetch_ref="+${ref_name}:${ref_name}"
  else
    fetch_ref="+${ref_name}:refs/remotes/origin/${ref#refs/heads/}"
  fi
  log "fetch pinned dependency ${expected_origin} ref ${ref_name}"
  git -C "${dir}" fetch --no-tags origin "${fetch_ref}" >/dev/null 2>&1 \
    || die "could not fetch pinned dependency ref ${ref_name}"
  verify_fetched_pinned_dependency_ref "${dir}" "${ref}" "${revision}" "${kind}"
  git -C "${dir}" cat-file -e "${revision}^{commit}" 2>/dev/null \
    || die "pinned dependency revision is not available from published ref ${ref_name}"
  git -C "${dir}" checkout -q --detach --force "${revision}" 2>/dev/null \
    || die "could not check out pinned dependency revision"
  verify_pinned_dependency_checkout "${dir}" "${url}" "${ref}" "${revision}" "${kind}"
}

verify_pinned_jobworkerp_checkout() {
  local dir=$1 ref=$2 revision=$3 url=$4 origin normalized_origin normalized_url head ref_commit status
  [[ -d "${dir}" ]] || die "pinned jobworkerp checkout is missing; coding builds never clone sources"
  [[ "$(coding_agent_git "${dir}" rev-parse --show-toplevel 2>/dev/null)" == \
    "$(cd "${dir}" && pwd -P)" ]] \
    || die "pinned jobworkerp source must be the root of a local Git checkout"
  case "${ref}" in
    main|master|HEAD|latest|refs/heads/main|refs/heads/master|refs/tags/latest) \
      die "floating source refs are not accepted in coding-agent build mode" ;;
  esac
  GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null \
    git check-ref-format --branch "${ref}" >/dev/null 2>&1 \
    || die "configured jobworkerp source ref is invalid"
  origin=$(coding_agent_git "${dir}" remote get-url origin 2>/dev/null) \
    || die "pinned jobworkerp checkout has no origin remote"
  node -e '
    const urls = process.argv.slice(1);
    for (const raw of urls) {
      try {
        const url = new URL(raw);
        if (!url.hostname || url.password || url.search || url.hash) process.exit(1);
        if (!["https:", "ssh:"].includes(url.protocol)) process.exit(1);
        if (!raw.startsWith(`${url.protocol}//`)) process.exit(1);
        const authority = raw.slice(raw.indexOf("://") + 3).split("/")[0];
        if (url.protocol === "https:" && authority.includes("@")) process.exit(1);
        if (url.protocol === "ssh:" && authority.includes("@") && !authority.startsWith("git@")) process.exit(1);
      } catch { process.exit(1); }
    }
  ' "${origin}" "${url}" >/dev/null 2>&1 \
    || die "jobworkerp origin or configured source URL is not credential-free"
  normalized_origin=$(safe_git_remote_url "${origin}") \
    || die "pinned jobworkerp origin URL is invalid"
  normalized_url=$(safe_git_remote_url "${url}") \
    || die "configured jobworkerp source URL is invalid"
  [[ "${normalized_origin}" == "${normalized_url}" ]] \
    || die "jobworkerp checkout origin does not match the configured source pin"
  head=$(coding_agent_git "${dir}" rev-parse HEAD) \
    || die "cannot read pinned jobworkerp revision"
  [[ "${head}" == "${revision}" ]] \
    || die "jobworkerp checkout HEAD does not match the configured full SHA"
  ref_commit=$(coding_agent_git "${dir}" rev-parse --verify --end-of-options "${ref}^{commit}" 2>/dev/null) \
    || die "jobworkerp pinned ref is unavailable in the local checkout"
  coding_agent_git "${dir}" merge-base --is-ancestor "${revision}" "${ref_commit}" >/dev/null 2>&1 \
    || die "jobworkerp pinned ref does not contain the configured full SHA"
  status=$(coding_agent_git "${dir}" status --porcelain --untracked-files=all --ignore-submodules=none) \
    || die "cannot verify jobworkerp source cleanliness"
  [[ -z "${status}" ]] \
    || die "jobworkerp source tree is dirty; commit reviewed runtime sources before building"
  coding_agent_git_tree_is_clean "${dir}" \
    || die "jobworkerp source tree is dirty or a submodule is uninitialized/mismatched; use a clean pinned checkout"
}

validate_memory_store_source_config() {
  local source_override="${LOOKBACK_MEMORY_STORE_SOURCE_DIR:-}"
  if [[ -n "${source_override}" ]]; then
    [[ -n "${LOOKBACK_MEMORY_STORE_REF:-}" && -n "${LOOKBACK_MEMORY_STORE_REVISION:-}" ]] \
      || die "LOOKBACK_MEMORY_STORE_SOURCE_DIR requires explicit LOOKBACK_MEMORY_STORE_REF and LOOKBACK_MEMORY_STORE_REVISION pins"
    [[ -d "${source_override}" ]] \
      || die "LOOKBACK_MEMORY_STORE_SOURCE_DIR is not a directory"
  fi
  validate_pinned_dependency_config memory-store
}

validate_issue_tracker_source_config() {
  validate_pinned_dependency_config issue-tracker-server
}

# The Issue Tracker is experimental and disabled in a normal release build, so
# its server is built and bundled only for an --experimental-issues release.
issue_tracker_bundled() {
  [[ "${EXPERIMENTAL_ISSUES:-0}" == "1" ]]
}

# want_repo NAME -> 0 (build it) / 1 (skip), honouring --only.
want_repo() {
  [[ "$1" != issue-tracker-server ]] || issue_tracker_bundled || return 1
  [[ -z "${ONLY_REPOS}" ]] && return 0
  [[ ",${ONLY_REPOS}," == *",$1,"* ]] && return 0
  return 1
}

# A packaged --experimental-issues release ships its Issue Tracker gRPC server.
# A partial backend build may omit it only when it does not invoke Tauri
# packaging.
require_bundle_issue_tracker_dependencies() {
  issue_tracker_bundled || return 0
  # DRY_RUN validates the build plan without creating a distributable bundle.
  # Permit partial repositories so command-plan tests can exercise packaging.
  [[ "${DRY_RUN:-0}" == "1" ]] && return 0
  [[ "${SKIP_FRONTEND:-0}" == "1" ]] && return 0
  [[ -z "${ONLY_REPOS:-}" || ",${ONLY_REPOS}," == *",issue-tracker-server,"* ]] && return 0
  die "--only must include issue-tracker-server when packaging Lookback; use --skip-frontend for a partial backend build"
}

require_host_protobuf_tools() {
  command -v protoc >/dev/null 2>&1 \
    || die "host protoc is required to build protobuf-dependent Rust crates; install the Protocol Buffers compiler"
  command -v jq >/dev/null 2>&1 \
    || die "host jq is required by the pinned migration bundle build scripts; install jq"
}

# repo_path NAME -> absolute source dir (or the dependency clone dir).
repo_path() {
  if [[ "$1" == "memory-store" && -n "${LOOKBACK_MEMORY_STORE_SOURCE_DIR:-}" ]]; then
    printf '%s\n' "${LOOKBACK_MEMORY_STORE_SOURCE_DIR}"
  else
    echo "${WORKDIR}/$(repo_subdir "$1")"
  fi
}

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

safe_git_remote_url() {
  node -e '
    const raw = process.argv[1];
    const scp = raw.match(/^(?:[^@/]+@)?([^:/]+):(.+)$/);
    if (scp && !raw.includes("://")) {
      process.stdout.write(`ssh://${scp[1]}/${scp[2]}`);
      process.exit(0);
    }
    try {
      const url = new URL(raw);
      if (!["https:", "http:", "ssh:"].includes(url.protocol) || !url.hostname) process.exit(1);
      url.username = "";
      url.password = "";
      url.search = "";
      url.hash = "";
      process.stdout.write(`${url.protocol}//${url.host}${url.pathname}`.replace(/\/$/, ""));
    } catch { process.exit(1); }
  ' "$1"
}

verify_memory_store_checkout() {
  local dir=$1 revision=$2 ref=$3 expected_origin=${4:-$(repo_url memory-store)}
  [[ -d "${dir}" ]] || die "Memories source directory not found"
  verify_pinned_dependency_checkout "${dir}" "${expected_origin}" "${ref}" \
    "${revision}" "$(repo_ref_kind memory-store)"
  node "${AGENT_APP}/scripts/verify-migration-release.mjs" --source "${dir}" \
    --app-assets "${AGENT_APP}" \
    || die "Memories source does not implement the expected schema/task contract"
}

clone_or_pull_memory_store() {
  validate_memory_store_source_config
  local dir revision ref url kind
  dir=$(repo_path memory-store)
  revision=$(repo_revision memory-store)
  ref=$(repo_branch memory-store)
  url=$(repo_url memory-store)
  kind=$(repo_ref_kind memory-store)
  checkout_pinned_dependency "${dir}" "${url}" "${ref}" "${revision}" \
    "${kind}" "${SKIP_CLONE}"
  [[ "${DRY_RUN:-0}" == "1" && ! -d "${dir}" ]] && return 0
  verify_memory_store_checkout "${dir}" "${revision}" "${ref}" "${url}"
}

# clone_or_pull NAME
clone_or_pull() {
  local name=$1 url dir branch
  if [[ "${name}" == "jobworkerp" ]] && coding_agent_build_enabled; then
    validate_coding_agent_build_config
    url=$(repo_url "${name}")
    dir=$(repo_path "${name}")
    branch=$(repo_branch "${name}")
    verify_pinned_jobworkerp_checkout "${dir}" "${branch}" \
      "$(repo_revision "${name}")" "${url}"
    log "reuse pinned coding-agent runtime source ${branch}@$(repo_revision "${name}")"
    return 0
  fi
  if [[ "${name}" == "memory-store" ]]; then
    clone_or_pull_memory_store
    return 0
  fi
  if [[ "${name}" == "issue-tracker-server" ]]; then
    validate_issue_tracker_source_config
    url=$(repo_url "${name}")
    dir=$(repo_path "${name}")
    branch=$(repo_branch "${name}")
    checkout_pinned_dependency "${dir}" "${url}" "${branch}" \
      "$(repo_revision "${name}")" "$(repo_ref_kind "${name}")" "${SKIP_CLONE}"
    return 0
  fi
  url=$(repo_url "${name}")
  dir=$(repo_path "${name}")
  branch=$(repo_branch "${name}")
  if is_own_git_repo "${dir}"; then
    # A real git checkout already exists: reuse or fast-forward.
    if [[ "${SKIP_CLONE}" == "1" ]]; then
      [[ "$(git -C "${dir}" branch --show-current)" == "${branch}" ]] \
        || die "reuse requested for ${name}, but ${dir} is not on ${branch}"
      log "reuse existing clone: ${dir}"
    else
      log "update ${name}@${branch}"
      run git -C "${dir}" fetch origin "${branch}"
      run git -C "${dir}" checkout -q "${branch}"
      run git -C "${dir}" pull --ff-only origin "${branch}"
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
    run git -C "${dir}" fetch origin "${branch}"
    run git -C "${dir}" checkout -q -f -B "${branch}" FETCH_HEAD
  else
    log "clone ${name} -> ${dir}"
    run git clone --branch "${branch}" "${url}" "${dir}"
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
  if [[ "${name}" == "jobworkerp" ]] && coding_agent_build_enabled; then
    local submodule_status
    local submodule_line
    submodule_status=$(coding_agent_git "${dir}" submodule status --recursive 2>/dev/null) \
      || die "cannot inspect pinned jobworkerp submodules"
    if [[ -n "${submodule_status}" ]]; then
      while IFS= read -r submodule_line; do
        [[ "${submodule_line:0:1}" == " " ]] \
          || die "pinned jobworkerp submodules must already be initialized at reviewed commits"
      done <<<"${submodule_status}"
    fi
    log "verify local pinned submodules: ${name}"
    return 0
  fi
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
  if coding_agent_build_enabled; then
    local args=(--target "${TRIPLE}" --runtime-source "$(repo_path jobworkerp)")
    [[ -z "${LOOKBACK_CODING_AGENT_ARTIFACT_DIR:-}" ]] \
      || args+=(--artifact-dir "${LOOKBACK_CODING_AGENT_ARTIFACT_DIR}")
    run bash "${AGENT_APP}/scripts/build-coding-agent-executor.sh" "${args[@]}"
    return 0
  fi
  cargo_build "$(repo_path jobworkerp)" --release -p jobworkerp-main --bin all-in-one
}

build_memory_store() {
  local dir staging_root staging backup destination revision ref kind source_origin
  dir=$(repo_path memory-store)
  revision=$(repo_revision memory-store)
  ref=$(repo_branch memory-store)
  kind=$(repo_ref_kind memory-store)
  source_origin=""
  [[ -n "${LOOKBACK_MEMORY_STORE_SOURCE_DIR:-}" ]] \
    || source_origin=$(repo_url memory-store)
  if [[ "${DRY_RUN:-0}" == "1" && ! -d "${dir}" \
    && -z "${LOOKBACK_MEMORY_STORE_SOURCE_DIR:-}" ]]; then
    log "DRY_RUN: source contract check requires the pinned Memories checkout"
  else
    verify_memory_store_checkout "${dir}" "${revision}" "${ref}" "${source_origin}"
  fi
  log "build front (lindera) + memories-import + official SQLite migration bundle"
  cargo_build "${dir}" --release -p grpc-admin --bin front --features lindera
  cargo_build "${dir}" --release -p agent-chat-import --bin memories-import
  destination="${AGENT_APP}/src-tauri/migration-bundle"
  if [[ "${DRY_RUN:-0}" == "1" ]]; then
    staging_root="${AGENT_APP}/src-tauri/.migration-bundle-build.dry-run.$$"
    staging="${staging_root}/bundle"
    run "${dir}/scripts/build-memories-db-migrate-sqlite.sh" "${staging}"
    run node "${AGENT_APP}/scripts/lib/write-migration-source-provenance.mjs" \
      "${dir}" "${staging}" "${ref}" "${revision}" \
      "${dir}/target/release/front" "${dir}/target/release/memories-import" \
      "$(pinned_dependency_ref_name "${dir}" "${ref}" "${kind}")"
    run node "${AGENT_APP}/scripts/verify-embedding-contract.mjs" \
      "${dir}" "${AGENT_APP}" --record-provenance \
      "${staging}/memories-source-provenance.json"
    run node "${AGENT_APP}/scripts/verify-migration-release.mjs" "${staging}" \
      --require-provenance --front "${dir}/target/release/front" \
      --import "${dir}/target/release/memories-import" --app-assets "${AGENT_APP}"
    return 0
  fi

  destination="${AGENT_APP}/src-tauri/migration-bundle"
  backup="${AGENT_APP}/src-tauri/.migration-bundle-previous"
  local lock="${destination}.lock"
  local verifier="${AGENT_APP}/scripts/verify-migration-release.mjs"
  mkdir -p "${AGENT_APP}/src-tauri"
  migration_bundle_lock_acquire "${lock}" "${destination}" "${backup}" \
    "${verifier}" 0 \
    || die "could not acquire or recover the Memories migration bundle lock"
  MEMORY_STORE_BUNDLE_DESTINATION=${destination}
  MEMORY_STORE_BUNDLE_BACKUP=${backup}
  MEMORY_STORE_BUNDLE_STAGING_ROOT=""
  MEMORY_STORE_BUNDLE_VERIFIER=${verifier}
  trap 'cleanup_memory_store_bundle_activation' EXIT
  trap 'exit 130' INT TERM
  migration_bundle_recover_previous "${destination}" "${backup}" \
    "${verifier}" 0 \
    || die "could not recover the previous Memories migration bundle activation"

  staging_root=$(mktemp -d "${AGENT_APP}/src-tauri/.migration-bundle-build.XXXXXX")
  MEMORY_STORE_BUNDLE_STAGING_ROOT=${staging_root}
  staging="${staging_root}/bundle"
  if ! run "${dir}/scripts/build-memories-db-migrate-sqlite.sh" "${staging}"; then
    die "official Memories migration bundle build failed"
  fi
  if ! run node "${AGENT_APP}/scripts/lib/write-migration-source-provenance.mjs" \
    "${dir}" "${staging}" "${ref}" "${revision}" \
    "${dir}/target/release/front" "${dir}/target/release/memories-import" \
    "$(pinned_dependency_ref_name "${dir}" "${ref}" "${kind}")"; then
    die "could not record verified Memories source provenance"
  fi
  if ! run node "${AGENT_APP}/scripts/verify-embedding-contract.mjs" \
    "${dir}" "${AGENT_APP}" --record-provenance \
    "${staging}/memories-source-provenance.json"; then
    die "could not record the pinned Memories embedding asset contract"
  fi
  if ! run node "${AGENT_APP}/scripts/verify-migration-release.mjs" "${staging}" \
    --require-provenance --front "${dir}/target/release/front" \
    --import "${dir}/target/release/memories-import" --app-assets "${AGENT_APP}"; then
    die "generated Memories migration bundle failed release verification"
  fi
  migration_bundle_activate "${staging}" "${destination}" "${backup}" \
    "${verifier}" 1 \
    || die "could not activate the verified Memories migration bundle"
  cleanup_memory_store_bundle_activation
  trap - EXIT INT TERM
}

cleanup_memory_store_bundle_activation() {
  if [[ -n "${MEMORY_STORE_BUNDLE_DESTINATION:-}" \
    && -n "${MEMORY_STORE_BUNDLE_BACKUP:-}" \
    && -n "${MIGRATION_BUNDLE_LOCK_DIR:-}" ]]; then
    migration_bundle_recover_previous \
      "${MEMORY_STORE_BUNDLE_DESTINATION}" "${MEMORY_STORE_BUNDLE_BACKUP}" \
      "${MEMORY_STORE_BUNDLE_VERIFIER}" 1 \
      || echo "Memories migration bundle recovery state was preserved for manual recovery" >&2
  fi
  [[ -z "${MEMORY_STORE_BUNDLE_STAGING_ROOT:-}" ]] \
    || rm -rf "${MEMORY_STORE_BUNDLE_STAGING_ROOT}"
  [[ -z "${MIGRATION_BUNDLE_LOCK_DIR:-}" ]] \
    || migration_bundle_lock_release "${MIGRATION_BUNDLE_LOCK_DIR}"
  MEMORY_STORE_BUNDLE_DESTINATION=""
  MEMORY_STORE_BUNDLE_BACKUP=""
  MEMORY_STORE_BUNDLE_STAGING_ROOT=""
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

build_issue_tracker_server() {
  local dir
  dir=$(repo_path issue-tracker-server)
  log "build issue-tracker-server"
  cargo_build "${dir}" --release -p issue-tracker-front --bin issue-tracker-server
}

# build_one NAME -> dispatch to the per-repo build function.
build_one() {
  case "$1" in
    jobworkerp)   build_jobworkerp ;;
    memory-store) build_memory_store ;;
    conductor)    build_conductor ;;
    llama)        build_llama ;;
    mm)           build_mm ;;
    issue-tracker-server) build_issue_tracker_server ;;
  esac
}

# build_all: clone + submodules + build every selected repo.
build_all() {
  local name
  validate_coding_agent_build_config
  for name in jobworkerp memory-store conductor llama mm issue-tracker-server; do
    want_repo "${name}" || { log "skip ${name} (--only, or not bundled without --experimental-issues)"; continue; }
    clone_or_pull "${name}"
    init_submodules "${name}"
    build_one "${name}"
  done
}

# stage_binaries: copy the externalBin targets with the platform-triple
# suffix Tauri expects. protoc is fetched from the official protobuf release
# (a self-contained binary) rather than the host's — see lib/protoc-fetch.sh.
stage_binaries() {
  if coding_agent_build_enabled; then
    log "coding-agent runtime/executor remain in their separate Linux/KVM artifact directory"
    return 0
  fi
  log "stage binaries -> ${BIN_DIR}"
  run mkdir -p "${BIN_DIR}"
  local jwp mem cond issue_tracker
  jwp=$(repo_path jobworkerp); mem=$(repo_path memory-store); cond=$(repo_path conductor)
  issue_tracker=$(repo_path issue-tracker-server)
  want_repo jobworkerp   && install_file "${jwp}/target/release/all-in-one"       "${BIN_DIR}/all-in-one-${TRIPLE}"
  want_repo memory-store && install_file "${mem}/target/release/front"            "${BIN_DIR}/front-${TRIPLE}"
  want_repo conductor    && install_file "${cond}/target/release/conductor-main"  "${BIN_DIR}/conductor-main-${TRIPLE}"
  want_repo memory-store && install_file "${mem}/target/release/memories-import"  "${BIN_DIR}/memories-import-${TRIPLE}"
  want_repo issue-tracker-server && install_file "${issue_tracker}/target/release/issue-tracker-server" "${BIN_DIR}/issue-tracker-server-${TRIPLE}"
  fetch_protoc_bin "${TRIPLE}" "${BIN_DIR}/protoc-${TRIPLE}"
  if [[ "${DRY_RUN:-0}" != "1" && "${SKIP_FRONTEND:-0}" != "1" ]]; then
    run node "${AGENT_APP}/scripts/verify-migration-release.mjs" \
      "${AGENT_APP}/src-tauri/migration-bundle" --require-provenance \
      --front "${BIN_DIR}/front-${TRIPLE}" \
      --import "${BIN_DIR}/memories-import-${TRIPLE}" --app-assets "${AGENT_APP}"
  fi
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

# stage_plugins: copy every plugin library used by bundled workers, then the
# CUDA runtime linked by GPU plugins so the bundle remains self-contained.
stage_plugins() {
  if coding_agent_build_enabled; then
    log "skip Tauri plugin staging in coding-agent artifact build mode"
    return 0
  fi
  log "stage plugins -> ${PLUGINS_DIR}"
  run mkdir -p "${PLUGINS_DIR}"
  # The self-hosted macOS checkout persists between jobs. Do not let an old
  # plugin (or CI placeholder) leak into a release bundle and later into the
  # app's data directory, where hardened runtime rejects a mismatched signer.
  run sh -c 'find "$1" -maxdepth 1 -type f -name "*.$2" -delete' _ "${PLUGINS_DIR}" "${LIBEXT}"
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

# sign_macos_files LABEL FILE...
# Tauri only signs known executable sidecars; code shipped as resources must
# carry its own hardened-runtime Developer ID signature before packaging.
sign_macos_files() {
  local label=$1
  shift
  [[ "${PLATFORM}" == "mac" ]] || return 0
  [[ "${DRY_RUN:-0}" == "1" ]] && { log "DRY_RUN: skip explicit macOS ${label} signing"; return 0; }

  (($# > 0)) || { warn "no macOS ${label} staged for signing"; return 0; }

  if [[ -z "${APPLE_SIGNING_IDENTITY:-}" ]]; then
    warn "skip explicit macOS ${label} signing: APPLE_SIGNING_IDENTITY is not set"
    return 0
  fi

  require_cmd codesign
  log "sign macOS ${label} ($#)"
  local f
  for f in "$@"; do
    run codesign --force --options runtime --timestamp --sign "${APPLE_SIGNING_IDENTITY}" "${f}"
  done
}

# sign_macos_plugins: sign staged plugin dylibs before Tauri packages the app.
sign_macos_plugins() {
  local dylibs=()
  local f
  for f in "${PLUGINS_DIR}"/*.dylib; do
    [[ -f "${f}" ]] || continue
    dylibs+=("${f}")
  done
  sign_macos_files "plugin dylibs" "${dylibs[@]}"
}

sign_macos_migration_binaries() {
  [[ "${PLATFORM}" == "mac" ]] || return 0

  local root="${AGENT_APP}/src-tauri/migration-bundle"
  local front="${BIN_DIR}/front-${TRIPLE}"
  local memories_import="${BIN_DIR}/memories-import-${TRIPLE}"
  local binaries=(
    "${root}/memories-db-migrate"
    "${root}/atlas/bin/atlas"
  )
  local binary
  for binary in "${binaries[@]}"; do
    [[ -f "${binary}" ]] || die "macOS migration bundle executable is missing: ${binary}"
  done
  if [[ -n "${APPLE_SIGNING_IDENTITY:-}" && "${DRY_RUN:-0}" != "1" ]]; then
    [[ -f "${front}" ]] || die "macOS Memories front binary is missing: ${front}"
    [[ -f "${memories_import}" ]] || die "macOS Memories import binary is missing: ${memories_import}"
    run node "${AGENT_APP}/scripts/verify-migration-release.mjs" "${root}" \
      --require-provenance --source-artifacts --front "${front}" \
      --import "${memories_import}" --app-assets "${AGENT_APP}" \
      || die "macOS migration binaries do not match verified source provenance before signing"
  fi
  sign_macos_files "migration bundle executables" "${binaries[@]}"
}

atlas_lock_platform() {
  case "${PLATFORM}" in
    mac) echo "darwin-arm64" ;;
    linux) echo "linux-amd64" ;;
    *) die "unsupported Atlas lock platform: ${PLATFORM}" ;;
  esac
}

# verify_fixed_atlas_lock: verify the downloaded Atlas before macOS signing.
verify_fixed_atlas_lock() {
  local platform; platform=$(atlas_lock_platform)
  run node "${AGENT_APP}/scripts/verify-atlas-lock.mjs" \
    "${AGENT_APP}/src-tauri/migration-bundle/atlas" "${platform}"
}

# record_signed_atlas_lock: retain the verified upstream hash while making the
# runtime lock match the Developer ID-signed Mach-O required by notarization.
record_signed_atlas_lock() {
  [[ "${PLATFORM}" == "mac" ]] || return 0
  local platform; platform=$(atlas_lock_platform)
  run node "${AGENT_APP}/scripts/record-signed-atlas-lock.mjs" \
    "${AGENT_APP}/src-tauri/migration-bundle/atlas" "${platform}"
}

# record_signed_migration_provenance: retain verified build hashes separately
# from the bytes distributed after macOS code signing.
record_signed_migration_provenance() {
  [[ "${PLATFORM}" == "mac" && -n "${APPLE_SIGNING_IDENTITY:-}" ]] || return 0
  local bundle="${AGENT_APP}/src-tauri/migration-bundle"
  local front="${BIN_DIR}/front-${TRIPLE}"
  local memories_import="${BIN_DIR}/memories-import-${TRIPLE}"
  local signed_artifacts=("$@")
  if ((${#signed_artifacts[@]} == 0)); then
    signed_artifacts=(memories_db_migrate)
  fi
  run node "${AGENT_APP}/scripts/lib/record-signed-migration-provenance.mjs" \
    "${bundle}" "${front}" "${memories_import}" "${signed_artifacts[@]}" \
    || die "could not record post-sign Memories artifact provenance"
  run node "${AGENT_APP}/scripts/verify-migration-release.mjs" "${bundle}" \
    --require-provenance --front "${front}" --import "${memories_import}" \
    --app-assets "${AGENT_APP}" \
    || die "signed Memories migration release failed verification"
}
