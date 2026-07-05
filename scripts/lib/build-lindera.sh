#!/usr/bin/env bash
# Lindera IPADIC dictionary generation for the Lookback release build.
#
# Sourced by scripts/build-release.sh. The dictionary is NOT shipped in the
# repo; it is downloaded from the Lindera release that matches the `memories`
# sidecar's resolved lindera crate version. The on-disk layout MUST be the
# lindera 3.x format that lance-index's lindera dependency expects, including
# metadata.json. Older 0.44.x dictionaries are incompatible.
#
# Relies on caller globals: AGENT_APP, WORKDIR.

LINDERA_VERSION="3.0.7"
LINDERA_IPADIC_ZIP="lindera-ipadic-${LINDERA_VERSION}.zip"
LINDERA_IPADIC_DIR="lindera-ipadic"
IPADIC_TARBALL="mecab-ipadic-2.7.0-20070801.tar.gz"
IPADIC_SRC_DIR="mecab-ipadic-2.7.0-20070801"

# stage_lindera STRATEGY
#   release -> download the Lindera release IPADIC dictionary (default)
#   cli     -> legacy alias for release
#   skip -> do nothing (front must then be built without --features lindera)
stage_lindera() {
  local strategy=$1
  case "${strategy}" in
    skip) log "lindera: skip (front must be built without --features lindera)"; return 0 ;;
    release|cli)  ;;
    *)    die "unknown lindera strategy: ${strategy}" ;;
  esac

  local dict_dir="${AGENT_APP}/dict/lindera/ipadic"
  local cache="${WORKDIR}/.lindera-cache"
  run mkdir -p "${cache}" "${dict_dir}"

  # 1. Lindera 3.x IPADIC release dictionary. This zip is the runtime
  #    dictionary itself, not the CLI binary.
  local dict_zip="${cache}/${LINDERA_IPADIC_ZIP}"
  if [[ -f "${dict_zip}" ]]; then
    log "lindera IPADIC cached: ${dict_zip}"
  else
    local url="https://github.com/lindera/lindera/releases/download/v${LINDERA_VERSION}/${LINDERA_IPADIC_ZIP}"
    log "fetch lindera IPADIC dictionary ${LINDERA_VERSION}"
    run curl -fsSL -o "${dict_zip}" "${url}"
  fi

  # 2. MeCab IPADIC source license. The release dictionary zip does not ship
  #    COPYING, but redistributed IPADIC-derived files must carry it.
  if [[ -d "${cache}/${IPADIC_SRC_DIR}" ]]; then
    log "IPADIC source license cached"
  else
    log "fetch IPADIC source license"
    run sh -c 'cd "$1" && curl -fsSL -o "$2" "https://lindera.dev/$2" && tar xzf "$2"' \
      _ "${cache}" "${IPADIC_TARBALL}"
  fi

  # 3. Stage the 3.x dictionary files in place and keep the license beside them.
  log "stage IPADIC dictionary -> ${dict_dir}"
  run sh -c 'rm -rf "$3" && mkdir -p "$1" "$3" && unzip -q -o "$2" -d "$1" && cp -R "$1/$4/." "$3"/ && cp "$5" "$3/COPYING"' \
    _ "${cache}" "${dict_zip}" "${dict_dir}" "${LINDERA_IPADIC_DIR}" "${cache}/${IPADIC_SRC_DIR}/COPYING"

  # 4. Verify format: lindera 3.x dictionaries carry metadata.json. Its
  #    absence means an older 0.44.x dictionary leaked in.
  if [[ "${DRY_RUN:-0}" != "1" ]]; then
    [[ -f "${dict_dir}/dict.da" && -f "${dict_dir}/char_def.bin" && -f "${dict_dir}/metadata.json" ]] \
      || die "lindera build produced no dictionary in ${dict_dir}"
    log "lindera dictionary OK (3.x format, metadata.json present)"
  fi
}
