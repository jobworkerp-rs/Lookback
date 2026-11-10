#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmpdir="$(mktemp -d)"
trap 'rm -rf "${tmpdir}"' EXIT

verbose_log="${tmpdir}/verbose.log"
normal_log="${tmpdir}/normal.log"

DRY_RUN=1 LOOKBACK_TAURI_VERBOSE=1 \
  bash "${ROOT_DIR}/scripts/build-release.sh" \
    --profile linux-cuda \
    --bundle appimage \
    --only none \
    --no-submodule-update \
    --workdir "${tmpdir}/deps" >"${verbose_log}" 2>&1

grep -Fq 'pnpm tauri build -v --bundles appimage' "${verbose_log}" \
  || { echo "expected verbose tauri build command" >&2; exit 1; }

DRY_RUN=1 \
  bash "${ROOT_DIR}/scripts/build-release.sh" \
    --profile linux-cuda \
    --bundle appimage \
    --only none \
    --no-submodule-update \
    --workdir "${tmpdir}/deps" >"${normal_log}" 2>&1

if grep -Fq 'pnpm tauri build -v --bundles appimage' "${normal_log}"; then
  echo "did not expect verbose tauri build command without LOOKBACK_TAURI_VERBOSE" >&2
  exit 1
fi

grep -Fq 'pnpm tauri build --bundles appimage' "${normal_log}" \
  || { echo "expected normal tauri build command" >&2; exit 1; }

echo "tauri verbose build tests passed"
