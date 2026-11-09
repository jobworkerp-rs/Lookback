# jobworkerp runner proto（テスト用）

このディレクトリは、テストが runner の定義として使う jobworkerp の proto を同梱する。CI はこのリポジトリだけを checkout するため、隣の jobworkerp checkout は参照しない。

更新元と手順は `workers/schema/jobworkerp/README.md` と同じで、`scripts/sync-jobworkerp-vendored.sh` で更新し、CI では同スクリプトの `--check` で同期を確認する。
