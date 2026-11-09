#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

exec cargo run --locked --manifest-path "${REPO_ROOT}/scripts/mcp-description-check/Cargo.toml" -- \
  "${REPO_ROOT}/workers/lookback-issue-gateway-workers.yaml"
