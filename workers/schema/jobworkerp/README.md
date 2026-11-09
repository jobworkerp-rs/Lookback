# jobworkerp workflow schema

このディレクトリは、CI とローカル検証で使う jobworkerp の workflow schema を同梱する。

- `workflow.yaml` は upstream の正規 schema。
- `workflow_minimal_fix.json` は upstream runner が実行時にロードする互換 schema。

更新元は release sidecar と同じ `LOOKBACK_JOBWORKERP_REF`（既定値: upstream `main`）である。更新は `scripts/sync-jobworkerp-vendored.sh`、CI での同期確認は同スクリプトの `--check` を使う。特定 revision/branch を検証・出荷する場合は、sidecar build と同期の両方に同じ `LOOKBACK_JOBWORKERP_REF` を渡す。
