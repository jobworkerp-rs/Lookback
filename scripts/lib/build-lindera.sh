#!/usr/bin/env bash
# Lindera IPADIC dictionary generation for the Lookback release build.
#
# Sourced by scripts/build-release.sh. The dictionary is NOT shipped in the
# repo; it is regenerated here from the MeCab IPADIC source CSV using the
# lindera 0.44.1 CLI. The on-disk layout MUST be the 0.44.1 format that
# lance-index's lindera dependency expects — the prebuilt dictionary zips from
# lindera v1.0.0+ are an incompatible newer format (they ship a metadata.json
# and fail to deserialize). See memory-store docs/lindera-testing.md.
#
# Relies on caller globals: AGENT_APP, TRIPLE, WORKDIR.

LINDERA_VERSION="0.44.1"
IPADIC_TARBALL="mecab-ipadic-2.7.0-20070801.tar.gz"
IPADIC_SRC_DIR="mecab-ipadic-2.7.0-20070801"

# stage_lindera STRATEGY
#   cli  -> download CLI + IPADIC CSV, build the dictionary (default)
#   skip -> do nothing (front must then be built without --features lindera)
stage_lindera() {
  local strategy=$1
  case "${strategy}" in
    skip) log "lindera: skip (front must be built without --features lindera)"; return 0 ;;
    cli)  ;;
    *)    die "unknown lindera strategy: ${strategy}" ;;
  esac

  local dict_dir="${AGENT_APP}/dict/lindera/ipadic"
  local cache="${WORKDIR}/.lindera-cache"
  run mkdir -p "${cache}" "${dict_dir}"

  # 1. lindera 0.44.1 CLI (the release artifact is the CLI binary only — it
  #    carries no dictionary; the dictionary is what we build below).
  local cli="${cache}/lindera"
  if [[ -x "${cli}" ]]; then
    log "lindera CLI cached: ${cli}"
  else
    local zip="lindera-ipadic-${TRIPLE}-v${LINDERA_VERSION}.zip"
    local url="https://github.com/lindera/lindera/releases/download/v${LINDERA_VERSION}/${zip}"
    log "fetch lindera CLI ${LINDERA_VERSION} (${TRIPLE})"
    run sh -c 'cd "$1" && curl -fsSL -o "$2" "$3" && unzip -o "$2" && chmod +x lindera' \
      _ "${cache}" "${zip}" "${url}"
  fi

  # 2. MeCab IPADIC source CSV.
  if [[ -d "${cache}/${IPADIC_SRC_DIR}" ]]; then
    log "IPADIC source cached"
  else
    log "fetch IPADIC source CSV"
    run sh -c 'cd "$1" && curl -fsSL -o "$2" "https://lindera.dev/$2" && tar xzf "$2"' \
      _ "${cache}" "${IPADIC_TARBALL}"
  fi

  # 3. Build the 0.44.1-compatible dictionary in place.
  log "build IPADIC dictionary -> ${dict_dir}"
  run sh -c 'cd "$1" && exec ./lindera build --dictionary-kind ipadic "$2" "$3"' \
    _ "${cache}" "${cache}/${IPADIC_SRC_DIR}" "${dict_dir}"

  # 4. Copy the license from the IPADIC source next to the generated dictionary.
  #    `lindera build` emits only the binary files; the license must travel with
  #    the dictionary. Sourcing it from the same tarball (not a committed copy)
  #    guarantees the COPYING always matches whatever dictionary was built — so
  #    switching to a different dictionary kind would carry its own license, and
  #    an ngram-only build that never generates this dir carries none.
  run cp "${cache}/${IPADIC_SRC_DIR}/COPYING" "${dict_dir}/COPYING"

  # 5. Verify format: the 0.44.1 loader produces no metadata.json. Its presence
  #    means a v1.0.0+ (incompatible) dictionary leaked in.
  if [[ "${DRY_RUN:-0}" != "1" ]]; then
    [[ -f "${dict_dir}/dict.da" && -f "${dict_dir}/char_def.bin" ]] \
      || die "lindera build produced no dictionary in ${dict_dir}"
    [[ -e "${dict_dir}/metadata.json" ]] \
      && die "metadata.json present in ${dict_dir} — wrong (v1.0.0+) dictionary format"
    log "lindera dictionary OK (0.44.1 format, no metadata.json)"
  fi
}
