#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "${TMP_DIR}"' EXIT

# shellcheck source=lib/build-common.sh
source "${SCRIPT_DIR}/lib/build-common.sh"
# shellcheck source=lib/build-deps.sh
source "${SCRIPT_DIR}/lib/build-deps.sh"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

test_cuda_stub_dir_contains_libcuda_so_1() {
  local stub_dir="${TMP_DIR}/cuda-stubs"
  mkdir -p "${stub_dir}"
  printf 'stub\n' >"${stub_dir}/libcuda.so"

  CUDA_DRIVER_STUB_DIR="${stub_dir}"
  GPU=cuda
  PLATFORM=linux
  BUNDLE=appimage

  local prepared
  prepared=$(prepare_cuda_driver_stub_dir)
  [[ -n "${prepared}" ]] || fail "expected a prepared stub directory"
  [[ -L "${prepared}/libcuda.so.1" ]] || fail "expected libcuda.so.1 symlink"
  [[ "$(readlink "${prepared}/libcuda.so.1")" == "${stub_dir}/libcuda.so" ]] \
    || fail "libcuda.so.1 should point to CUDA driver stub"
}

test_non_cuda_build_does_not_prepare_stub() {
  GPU=cpu
  PLATFORM=linux
  BUNDLE=appimage
  CUDA_DRIVER_STUB_DIR="${TMP_DIR}/missing"

  local prepared
  prepared=$(prepare_cuda_driver_stub_dir)
  [[ -z "${prepared}" ]] || fail "did not expect stub directory for non-CUDA build"
}

test_cuda_stub_dir_contains_libcuda_so_1
test_non_cuda_build_does_not_prepare_stub

echo "CUDA driver stub tests passed"
