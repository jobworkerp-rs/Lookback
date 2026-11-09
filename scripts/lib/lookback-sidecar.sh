# shellcheck shell=bash disable=SC2034  # sets variables for the sourcing script
# Shared helpers for operator scripts that talk to a running Lookback app's
# sidecars (jobworkerp, Issue Tracker, Memories). Source after setting
# DATA_DIR and TIMEOUT_SEC and, optionally, JOBWORKERP_ADDRESS / MEMORIES_URL /
# TRACKER_PORT; call lookback_sidecar_init to fill in the rest.

die() {
  echo "error: $*" >&2
  exit 1
}

require_tools() {
  local tool
  for tool in "$@"; do
    command -v "${tool}" >/dev/null 2>&1 || die "${tool} is required on PATH"
  done
}

# Probes a sidecar process's listeners for the one serving `service`. The
# app lets the OS pick ports when the preferred one is busy, so neither the
# Issue Tracker nor jobworkerp has a fixed address.
discover_grpc_port() {
  local process=$1 service=$2 port
  for port in $(ss -ltnpH 2>/dev/null | grep "\"${process}\"" | sed -E 's/.*:([0-9]+) .*/\1/' | sort -u); do
    if grpcurl -plaintext "127.0.0.1:${port}" list 2>/dev/null | grep -q "${service}"; then
      echo "${port}"
      return 0
    fi
  done
  return 1
}

# Sets POLICY_JSON, LLM_WORKER and OUTPUT_LANGUAGE from the app's settings.
load_app_settings() {
  local policy_file="${DATA_DIR}/issue-auto-policy/current.json" llm_mode
  [[ -f "${policy_file}" ]] || die "policy file not found: ${policy_file}"
  POLICY_JSON=$(jq -c '.policy' "${policy_file}")
  llm_mode=$(jq -r '.mode // "local"' "${DATA_DIR}/llm-settings.json" 2>/dev/null || echo local)
  if [[ "${llm_mode}" == "external" ]]; then
    LLM_WORKER="memories-llm-external"
  else
    LLM_WORKER="memories-llm"
  fi
  OUTPUT_LANGUAGE=$(jq -r '.output_language // "ja"' "${DATA_DIR}/app-settings.json" 2>/dev/null || echo ja)
}

# Sets JOBWORKERP_ADDRESS (unless given) and TIMEOUT_MS.
resolve_jobworkerp_address() {
  local port
  if [[ -z "${JOBWORKERP_ADDRESS:-}" ]]; then
    # The sidecar prefers 9000 but falls back to a random port when 9000 is
    # taken (e.g. by a previous instance still shutting down), so discover it
    # from the running process before trusting the configured URL.
    if port=$(discover_grpc_port "all-in-one" "jobworkerp.service.JobService"); then
      JOBWORKERP_ADDRESS="http://127.0.0.1:${port}"
    else
      JOBWORKERP_ADDRESS=$(jq -r '.remote_jobworkerp_url // empty' "${DATA_DIR}/connection.json" 2>/dev/null || true)
      JOBWORKERP_ADDRESS=${JOBWORKERP_ADDRESS:-http://localhost:9000}
    fi
  fi
  TIMEOUT_MS=$((TIMEOUT_SEC * 1000))
}

# Sets MEMORIES_{URL,HOST,PORT,TLS} from --memories-url or the remote setting.
resolve_memories_endpoint() {
  local scheme hostport
  if [[ -z "${MEMORIES_URL:-}" ]]; then
    MEMORIES_URL=$(jq -r 'select(.mode == "remote") | .remote_memories_url // empty' "${DATA_DIR}/connection.json" 2>/dev/null || true)
    [[ -n "${MEMORIES_URL}" ]] \
      || die "memories URL unknown (local sidecar mode); pass --memories-url http://host:port"
  fi
  scheme=${MEMORIES_URL%%://*}
  hostport=${MEMORIES_URL#*://}
  hostport=${hostport%%/*}
  MEMORIES_HOST=${hostport%:*}
  MEMORIES_PORT=${hostport##*:}
  [[ "${MEMORIES_PORT}" =~ ^[0-9]+$ ]] || die "memories URL must include a port: ${MEMORIES_URL}"
  MEMORIES_TLS=false
  [[ "${scheme}" == "https" ]] && MEMORIES_TLS=true
  return 0
}

# Sets TRACKER_PORT (unless given) and TRACKER_ADDR.
resolve_tracker_endpoint() {
  if [[ -z "${TRACKER_PORT:-}" ]]; then
    TRACKER_PORT=$(discover_grpc_port "issue-tracker-s" "issue_tracker.rpc.v1.ManagerConsumerService") \
      || die "could not find the ManagerConsumerService port; is the Issue Tracker running? (--tracker-port)"
  fi
  TRACKER_HOST=127.0.0.1
  TRACKER_ADDR="${TRACKER_HOST}:${TRACKER_PORT}"
}

# Stops unless the saved policy lets workflows write resolutions.
require_resolution_apply() {
  local mode
  mode=$(jq -r '.resolution_mode // "DISABLED"' <<<"${POLICY_JSON}")
  [[ "${mode}" == "APPLY" ]] || die "resolution_mode is ${mode}; set it to APPLY in Settings before $1"
}

# Everything a script that enqueues an Issue workflow needs.
lookback_sidecar_init() {
  load_app_settings
  resolve_jobworkerp_address
  resolve_memories_endpoint
  resolve_tracker_endpoint
}

# Calls a Memories method with grpcurl, over TLS when the URL is https.
memories_grpc() {
  local method=$1 body=$2 transport=()
  [[ "${MEMORIES_TLS}" == "true" ]] || transport=(-plaintext)
  grpcurl "${transport[@]}" -d "${body}" "${MEMORIES_HOST}:${MEMORIES_PORT}" "${method}"
}

# Enqueues one WORKFLOW `run` job and waits for its DIRECT result. `--using
# run` is required because the runner also exposes `create`. RUST_LOG keeps
# the client's connection tracing out of the captured text.
run_workflow_job() {
  local worker=$1 input=$2 args
  # WORKFLOW `run` args carry the input as one JSON-encoded string field.
  args=$(jq -cn --arg input "${input}" '{ input: $input }')
  RUST_LOG="${RUST_LOG:-warn}" jobworkerp-client -a "${JOBWORKERP_ADDRESS}" -t "${TIMEOUT_MS}" \
    job enqueue -w "${worker}" --using run -t "${TIMEOUT_MS}" -a "${args}" 2>&1
}

# The workflow result is printed as `output: {json}`; returns that JSON or "".
workflow_output_json() {
  printf '%s\n' "$1" | sed -n 's/^output: //p' | tail -n1
}

# Connection and LLM settings shared by every workflow these scripts enqueue.
common_worker_input() {
  jq -cn --arg host "${MEMORIES_HOST}" --argjson port "${MEMORIES_PORT}" --argjson tls "${MEMORIES_TLS}" \
    --arg tracker_host "${TRACKER_HOST}" --argjson tracker_port "${TRACKER_PORT}" --arg llm "${LLM_WORKER}" --arg lang "${OUTPUT_LANGUAGE}" '
    { memories_grpc_host: $host, memories_grpc_port: $port, memories_grpc_tls: $tls,
      issue_tracker_grpc_host: $tracker_host, issue_tracker_grpc_port: $tracker_port,
      llm_worker_name: $llm, output_language: $lang }'
}
