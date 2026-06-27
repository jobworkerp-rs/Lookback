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
export GTK_IM_MODULE_FILE="$APPDIR//usr/lib/x86_64-linux-gnu/gtk-3.0/3.0.0/immodules.cache"
export GDK_PIXBUF_MODULE_FILE="$APPDIR//usr/lib/x86_64-linux-gnu/gdk-pixbuf-2.0/2.10.0/loaders.cache"
EOF

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
assert_contains "${hook}" 'export GTK_IM_MODULE_FILE="$APPDIR//usr/lib/x86_64-linux-gnu/gtk-3.0/3.0.0/immodules.cache"'
assert_no_exact_line "${hook}" 'export GTK_IM_MODULE_FILE="$APPDIR//usr/lib/x86_64-linux-gnu/gtk-3.0/3.0.0/immodules.cache"'
assert_contains "${hook}" 'export GDK_PIXBUF_MODULE_FILE='

before=$(mktemp)
cp "${hook}" "${before}"
if patch_linuxdeploy_gtk_hook_for_host_ime "${root}"; then
  fail "expected already-patched hook to report no change"
fi
cmp -s "${before}" "${hook}" || fail "expected already-patched hook to stay unchanged"

missing_root="${tmpdir}/missing-root"
if patch_linuxdeploy_gtk_hook_for_host_ime "${missing_root}"; then
  fail "expected missing hook to report no change"
fi

printf 'PASS: AppImage GTK hook patch keeps host IME modules available\n'
