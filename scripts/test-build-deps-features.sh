#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# shellcheck source=lib/build-common.sh
source "${SCRIPT_DIR}/lib/build-common.sh"
# shellcheck source=lib/build-deps.sh
source "${SCRIPT_DIR}/lib/build-deps.sh"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

GPU=metal
[[ "$(gpu_features)" == "--features metal" ]] || fail "llama metal features changed"
[[ "$(mm_gpu_features)" == "--features metal,onnx-coreml" ]] \
  || fail "mm metal build must include CoreML"

GPU=cuda
[[ "$(gpu_features)" == "--features cuda" ]] || fail "llama cuda features changed"
[[ "$(mm_gpu_features)" == "--features cuda,onnx-cuda" ]] \
  || fail "mm cuda build must include ONNX CUDA"

GPU=cpu
[[ -z "$(gpu_features)" ]] || fail "llama CPU build must not enable GPU features"
[[ -z "$(mm_gpu_features)" ]] || fail "mm CPU build must not enable GPU features"

echo "build dependency feature tests passed"
