#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=lib/appimage-hooks.sh
source "${SCRIPT_DIR}/lib/appimage-hooks.sh"

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  exit 1
}

assert_contains() {
  local file=$1 expected=$2
  grep -Fq "${expected}" "${file}" || fail "expected ${file} to contain: ${expected}"
}

assert_not_contains() {
  local file=$1 unexpected=$2
  ! grep -Fq "${unexpected}" "${file}" || fail "expected ${file} not to contain: ${unexpected}"
}

assert_no_exact_line() {
  local file=$1 unexpected=$2
  ! grep -Fxq "${unexpected}" "${file}" || fail "expected ${file} not to contain exact line: ${unexpected}"
}

tmpdir=$(mktemp -d)
trap 'rm -rf "${tmpdir}"' EXIT

root="${tmpdir}/squashfs-root"
hook="${root}/apprun-hooks/linuxdeploy-plugin-gtk.sh"
mkdir -p "$(dirname "${hook}")"
cat > "${hook}" <<'EOF'
export APPDIR="${APPDIR:-"$(dirname "$(realpath "$0")")"}"
export GTK_PATH="$APPDIR//usr/lib/x86_64-linux-gnu/gtk-3.0:/usr/lib/x86_64-linux-gnu/gtk-3.0"
export GTK_IM_MODULE_FILE="$APPDIR//usr/lib/gtk-3.0/3.0.0/immodules.cache"
export GDK_BACKEND=x11 # linuxdeploy's stock Wayland workaround
export GDK_PIXBUF_MODULE_FILE="$APPDIR//usr/lib/x86_64-linux-gnu/gdk-pixbuf-2.0/2.10.0/loaders.cache"
EOF

patch_linuxdeploy_gtk_hook_for_wayland_backend "${root}" \
  || fail "expected linuxdeploy GTK backend patch to report a change"

assert_contains "${hook}" 'export GDK_BACKEND="${GDK_BACKEND:-wayland,x11}"'
assert_no_exact_line "${hook}" 'export GDK_BACKEND=x11'

patch_linuxdeploy_gtk_hook_for_host_ime "${root}" \
  || fail "expected linuxdeploy GTK hook patch to report a change"

assert_contains "${hook}" 'LOOKBACK_HOST_GTK_IM_MODULE_FILE=""'
assert_contains "${hook}" 'grep -Eq "im-(fcitx|fcitx5|ibus)\\.so"'
assert_contains "${hook}" 'export GTK_IM_MODULE_FILE="$LOOKBACK_HOST_GTK_IM_MODULE_FILE"'
assert_contains "${hook}" 'export GTK_PATH="/usr/lib/gtk-3.0:/usr/lib64/gtk-3.0:/usr/lib/x86_64-linux-gnu/gtk-3.0:${GTK_PATH:-}"'
assert_contains "${hook}" 'LOOKBACK_HOST_GLIB_PRELOAD=""'
assert_contains "${hook}" '/usr/lib/libpcre2-8.so.0 \'
assert_contains "${hook}" '/usr/lib/libmount.so.1 \'
assert_contains "${hook}" 'export LD_PRELOAD="$LOOKBACK_HOST_GLIB_PRELOAD${LD_PRELOAD:+:$LD_PRELOAD}"'
assert_contains "${hook}" '*@im=fcitx*) export GTK_IM_MODULE=fcitx ;;'
assert_contains "${hook}" 'command -v fcitx5-remote >/dev/null 2>&1 && fcitx5-remote >/dev/null 2>&1'
assert_contains "${hook}" 'pgrep -x -u "$(id -u)" fcitx5 >/dev/null 2>&1'
assert_contains "${hook}" 'pgrep -x -u "$(id -u)" ibus-daemon >/dev/null 2>&1'
assert_contains "${hook}" 'Lookback GTK IME: GTK_IM_MODULE=${GTK_IM_MODULE:-}'
assert_contains "${hook}" 'Lookback GTK IME: LD_PRELOAD=${LD_PRELOAD:-}'
assert_contains "${hook}" 'export GTK_IM_MODULE_FILE="$APPDIR//usr/lib/gtk-3.0/3.0.0/immodules.cache"'
assert_no_exact_line "${hook}" 'export GTK_IM_MODULE_FILE="$APPDIR//usr/lib/gtk-3.0/3.0.0/immodules.cache"'
assert_contains "${hook}" 'export GDK_PIXBUF_MODULE_FILE='

before=$(mktemp)
cp "${hook}" "${before}"
if patch_linuxdeploy_gtk_hook_for_wayland_backend "${root}"; then
  fail "expected already-patched backend hook to report no change"
fi
cmp -s "${before}" "${hook}" || fail "expected already-patched backend hook to stay unchanged"

if patch_linuxdeploy_gtk_hook_for_host_ime "${root}"; then
  fail "expected already-patched hook to report no change"
fi
cmp -s "${before}" "${hook}" || fail "expected already-patched hook to stay unchanged"

# The hook written by the Tauri CLI 2.12.4 GTK plugin.
tauri_root="${tmpdir}/tauri-2.12-root"
tauri_hook="${tauri_root}/apprun-hooks/linuxdeploy-plugin-gtk.sh"
mkdir -p "$(dirname "${tauri_hook}")"
printf '%s\n' '# export GDK_BACKEND=x11 # Monitor this closely. AppImage used to crash on Wayland!' >"${tauri_hook}"
cp "${tauri_hook}" "${before}"
if patch_linuxdeploy_gtk_hook_for_wayland_backend "${tauri_root}"; then
  fail "expected a hook that forces no backend to report no change"
else
  status=$?
  [[ "${status}" == 1 ]] || fail "expected a hook that forces no backend to be accepted, got status ${status}"
fi
cmp -s "${before}" "${tauri_hook}" || fail "expected a hook that forces no backend to stay unchanged"

debian_root="${tmpdir}/debian-root"
debian_hook="${debian_root}/apprun-hooks/linuxdeploy-plugin-gtk.sh"
mkdir -p "$(dirname "${debian_hook}")"
printf '%s\n' \
  'export GTK_IM_MODULE_FILE="$APPDIR//usr/lib/x86_64-linux-gnu/gtk-3.0/3.0.0/immodules.cache"' \
  > "${debian_hook}"
patch_linuxdeploy_gtk_hook_for_host_ime "${debian_root}" \
  || fail "expected Debian multiarch GTK cache path to be patched"
assert_contains "${debian_hook}" 'LOOKBACK_HOST_GTK_IM_MODULE_FILE=""'
assert_contains "${debian_hook}" 'export GTK_IM_MODULE_FILE="$APPDIR//usr/lib/x86_64-linux-gnu/gtk-3.0/3.0.0/immodules.cache"'
assert_no_exact_line "${debian_hook}" 'export GTK_IM_MODULE_FILE="$APPDIR//usr/lib/x86_64-linux-gnu/gtk-3.0/3.0.0/immodules.cache"'

missing_root="${tmpdir}/missing-root"
if patch_linuxdeploy_gtk_hook_for_host_ime "${missing_root}"; then
  fail "expected missing hook to report no change"
fi

if patch_linuxdeploy_gtk_hook_for_wayland_backend "${missing_root}"; then
  fail "expected missing backend hook to fail"
else
  status=$?
  [[ "${status}" == 2 ]] || fail "expected missing backend hook status 2, got ${status}"
fi

invalid_root="${tmpdir}/invalid-root"
invalid_hook="${invalid_root}/apprun-hooks/linuxdeploy-plugin-gtk.sh"
mkdir -p "$(dirname "${invalid_hook}")"
printf '%s\n' 'export GDK_BACKEND=wayland' >"${invalid_hook}"
if patch_linuxdeploy_gtk_hook_for_wayland_backend "${invalid_root}"; then
  fail "expected unexpected backend hook format to fail"
else
  status=$?
  [[ "${status}" == 2 ]] || fail "expected unexpected backend hook format status 2, got ${status}"
fi

printf 'PASS: AppImage GTK hook patch keeps host IME modules available\n'
