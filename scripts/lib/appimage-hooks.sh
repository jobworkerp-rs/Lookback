#!/usr/bin/env bash
# Helpers for patching linuxdeploy-generated AppImage runtime hooks.
#
# Sourced by scripts/build-release.sh and shell tests.

patch_linuxdeploy_gtk_hook_for_wayland_backend() {
  local root=$1
  local hook="${root}/apprun-hooks/linuxdeploy-plugin-gtk.sh"
  if [[ ! -f "${hook}" ]]; then
    echo "missing linuxdeploy GTK backend hook: ${hook}" >&2
    return 2
  fi

  # shellcheck disable=SC2016 # This is the literal line written into the hook.
  local desired='export GDK_BACKEND="${GDK_BACKEND:-wayland,x11}"'
  grep -Fqx "${desired}" "${hook}" && return 1
  if ! grep -Eq '^export GDK_BACKEND=x11([[:space:]]*(#.*)?)?$' "${hook}"; then
    echo "unrecognized linuxdeploy GTK backend hook: ${hook}" >&2
    return 2
  fi

  local tmp="${hook}.tmp"
  awk -v desired="${desired}" '
    /^export GDK_BACKEND=x11([[:space:]]*(#.*)?)?$/ { print desired; next }
    { print }
  ' "${hook}" > "${tmp}"
  mv "${tmp}" "${hook}"
}

patch_linuxdeploy_gtk_hook_for_host_ime() {
  local root=$1
  local hook="${root}/apprun-hooks/linuxdeploy-plugin-gtk.sh"
  [[ -f "${hook}" ]] || return 1

  # linuxdeploy derives this path from pkg-config's libdir, which differs
  # across distributions (for example, /usr/lib vs. Debian multiarch paths).
  local immodules_cache_pattern='^export GTK_IM_MODULE_FILE="\$APPDIR/.*/gtk-3\.0/[[:digit:]]+\.[[:digit:]]+\.[[:digit:]]+/immodules\.cache"$'
  grep -Eq "${immodules_cache_pattern}" "${hook}" || return 1

  local tmp="${hook}.tmp"
  awk '
    /^export GTK_IM_MODULE_FILE="\$APPDIR\/.*\/gtk-3\.0\/[[:digit:]]+\.[[:digit:]]+\.[[:digit:]]+\/immodules\.cache"$/ {
      print "# Prefer the host GTK input-method cache when it exposes fcitx/ibus. The"
      print "# bundled linuxdeploy cache only contains generic modules and can route"
      print "# Japanese input through XIM, which freezes WebKitGTK text fields on some"
      print "# desktops or disables IME composition entirely."
      print "LOOKBACK_HOST_GTK_IM_MODULE_FILE=\"\""
      print "for candidate in \\"
      print "  /usr/lib/gtk-3.0/3.0.0/immodules.cache \\"
      print "  /usr/lib/x86_64-linux-gnu/gtk-3.0/3.0.0/immodules.cache \\"
      print "  /usr/lib64/gtk-3.0/3.0.0/immodules.cache"
      print "do"
      print "  if [ -r \"$candidate\" ] && grep -Eq \"im-(fcitx|fcitx5|ibus)\\\\.so\" \"$candidate\"; then"
      print "    LOOKBACK_HOST_GTK_IM_MODULE_FILE=\"$candidate\""
      print "    break"
      print "  fi"
      print "done"
      print "if [ -n \"$LOOKBACK_HOST_GTK_IM_MODULE_FILE\" ]; then"
      print "  export GTK_IM_MODULE_FILE=\"$LOOKBACK_HOST_GTK_IM_MODULE_FILE\""
      print "  export GTK_PATH=\"/usr/lib/gtk-3.0:/usr/lib64/gtk-3.0:/usr/lib/x86_64-linux-gnu/gtk-3.0:${GTK_PATH:-}\""
      print "  LOOKBACK_HOST_GLIB_PRELOAD=\"\""
      print "  for lib in \\"
      print "    /usr/lib/libglib-2.0.so.0 \\"
      print "    /usr/lib/libgobject-2.0.so.0 \\"
      print "    /usr/lib/libgio-2.0.so.0 \\"
      print "    /usr/lib/libgmodule-2.0.so.0 \\"
      print "    /usr/lib/libpcre2-8.so.0 \\"
      print "    /usr/lib/libffi.so.8 \\"
      print "    /usr/lib/libz.so.1 \\"
      print "    /usr/lib/libmount.so.1 \\"
      print "    /usr/lib/libblkid.so.1 \\"
      print "    /usr/lib/libsystemd.so.0 \\"
      print "    /usr/lib64/libglib-2.0.so.0 \\"
      print "    /usr/lib64/libgobject-2.0.so.0 \\"
      print "    /usr/lib64/libgio-2.0.so.0 \\"
      print "    /usr/lib64/libgmodule-2.0.so.0 \\"
      print "    /usr/lib64/libpcre2-8.so.0 \\"
      print "    /usr/lib64/libffi.so.8 \\"
      print "    /usr/lib64/libz.so.1 \\"
      print "    /usr/lib64/libmount.so.1 \\"
      print "    /usr/lib64/libblkid.so.1 \\"
      print "    /usr/lib64/libsystemd.so.0 \\"
      print "    /usr/lib/x86_64-linux-gnu/libglib-2.0.so.0 \\"
      print "    /usr/lib/x86_64-linux-gnu/libgobject-2.0.so.0 \\"
      print "    /usr/lib/x86_64-linux-gnu/libgio-2.0.so.0 \\"
      print "    /usr/lib/x86_64-linux-gnu/libgmodule-2.0.so.0 \\"
      print "    /usr/lib/x86_64-linux-gnu/libpcre2-8.so.0 \\"
      print "    /usr/lib/x86_64-linux-gnu/libffi.so.8 \\"
      print "    /usr/lib/x86_64-linux-gnu/libz.so.1 \\"
      print "    /usr/lib/x86_64-linux-gnu/libmount.so.1 \\"
      print "    /usr/lib/x86_64-linux-gnu/libblkid.so.1 \\"
      print "    /usr/lib/x86_64-linux-gnu/libsystemd.so.0"
      print "  do"
      print "    [ -r \"$lib\" ] && LOOKBACK_HOST_GLIB_PRELOAD=\"$LOOKBACK_HOST_GLIB_PRELOAD${LOOKBACK_HOST_GLIB_PRELOAD:+:}$lib\""
      print "  done"
      print "  if [ -n \"$LOOKBACK_HOST_GLIB_PRELOAD\" ]; then"
      print "    export LD_PRELOAD=\"$LOOKBACK_HOST_GLIB_PRELOAD${LD_PRELOAD:+:$LD_PRELOAD}\""
      print "  fi"
      print "  if [ -z \"${GTK_IM_MODULE:-}\" ]; then"
      print "    case \"${XMODIFIERS:-}\" in"
      print "      *@im=fcitx*) export GTK_IM_MODULE=fcitx ;;"
      print "      *@im=ibus*) export GTK_IM_MODULE=ibus ;;"
      print "    esac"
      print "  fi"
      print "  if [ -z \"${GTK_IM_MODULE:-}\" ]; then"
      print "    if command -v fcitx5-remote >/dev/null 2>&1 && fcitx5-remote >/dev/null 2>&1; then"
      print "      export GTK_IM_MODULE=fcitx"
      print "    elif command -v pgrep >/dev/null 2>&1 && pgrep -x -u \"$(id -u)\" fcitx5 >/dev/null 2>&1; then"
      print "      export GTK_IM_MODULE=fcitx"
      print "    elif command -v pgrep >/dev/null 2>&1 && pgrep -x -u \"$(id -u)\" ibus-daemon >/dev/null 2>&1; then"
      print "      export GTK_IM_MODULE=ibus"
      print "    fi"
      print "  fi"
      print "else"
      print "  " $0
      print "fi"
      print "if [ \"${LOOKBACK_DEBUG_IME:-}\" = \"1\" ]; then"
      print "  echo \"Lookback GTK IME: GTK_IM_MODULE=${GTK_IM_MODULE:-}\" >&2"
      print "  echo \"Lookback GTK IME: GTK_IM_MODULE_FILE=${GTK_IM_MODULE_FILE:-}\" >&2"
      print "  echo \"Lookback GTK IME: GTK_PATH=${GTK_PATH:-}\" >&2"
      print "  echo \"Lookback GTK IME: XMODIFIERS=${XMODIFIERS:-}\" >&2"
      print "  echo \"Lookback GTK IME: LD_PRELOAD=${LD_PRELOAD:-}\" >&2"
      print "fi"
      next
    }
    { print }
  ' "${hook}" > "${tmp}"
  mv "${tmp}" "${hook}"
  return 0
}
