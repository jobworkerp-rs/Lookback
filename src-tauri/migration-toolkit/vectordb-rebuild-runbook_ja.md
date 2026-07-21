# LanceDB 再構築手順

## 目的と適用範囲

`memory_kind` を含む新しい LanceDB schema へ切り替えるための完全停止メンテナンス手順です。RDB の `thread.memory_kind` と `memory.memory_kind` が contract migration 済みであることを前提とします。専用の再構築 CLI は使わず、旧LanceDBの退避、空schemaの作成、既存Redispatch RPCによる再投入で再構築します。

本番DB・本番LanceDBへの実行は、この文書の変更だけでは承認されません。実行時は対象環境、停止時間、バックアップ保管先、ロールバック責任者を別途確定してください。

## 対象 store と再投入

| store | LanceDB URI | 再投入 RPC |
|---|---|---|
| `memory_vector` | `MEMORY_LANCEDB_URI` | `MemoryVectorService.RedispatchEmbeddings` |
| `thread_vector` | `THREAD_LANCEDB_URI`（未設定時は `MEMORY_LANCEDB_URI` 配下） | `ThreadVectorService.RedispatchEmbeddings` |
| `reflection_intent_vector` | `REFLECTION_LANCEDB_URI`（未設定時は `MEMORY_LANCEDB_URI/reflection_intent`） | `ReflectionVectorService.RedispatchReflectionEmbeddings(kind=INTENT)` |

検索・再投入の対象ユーザーは予約IDではなく実ユーザーIDです。種別は `memory_kind` で指定します。reflection search document を再投入する場合も予約 ID は使わず、対象実ユーザーと `MEMORY_KIND_REFLECTION` を指定します。

## 実行手順

1. 変更対象のRDBと全LanceDB URI、Redispatch RPCの到達先を記録する。RDBバックアップと、各LanceDBディレクトリの退避先を用意する。
2. 旧サーバー、embedding dispatcher、summary/personality/reflection を生成するワークフローを停止し、外部トラフィックを遮断する。以後、ロールバック完了またはトラフィック復帰まで新旧コードを同時に書き込ませない。
3. RDBをバックアップする。共有サーバーDBでは `migrate-memory-kind plan` を実行し、未解決があれば[移行仕様の修復手順](memory-kind-migration-plan_ja.md#plan-の未解決を修復する手順)で根拠不足・参照不整合を解消して再実行する。未解決が0件になってから`apply`、`verify`を順に実行し、監査JSONの失敗が0件であることを確認する。単独利用のクライアントDBにはこの手順を代用せず、[クライアント用 memory_kind 移行手順](memory-kind-client-migration_ja.md)を使用する。
4. SQLite は `infra/sql/sqlite/manual/012_contract_memory_kind.sql`、PostgreSQL は `infra/sql/postgres/manual/011_contract_memory_kind.sql` をトランザクションとして適用する。これらは宣言的な `NOT NULL` 制約とDEFAULTの削除だけを適用する。PostgreSQL版は旧契約で追加された値域CHECKも削除する。NULLが残れば制約違反で失敗するため、値を補正して続行しない。`UNSPECIFIED(0)` と未知のenum値は、将来の種別追加を阻害しないようDBの値域制約ではなくアプリケーション入力検証で拒否する。SQLite で制約違反になった場合は、同じ接続で `ROLLBACK` してから状態確認または再実行する。
5. 旧LanceDBディレクトリを削除せず、RDBバックアップと同じ保持期間・識別子で退避する。各URI配下に旧schemaが残っていないことを確認する。
6. Redisキャッシュを無効化し、新リリースのサーバーを起動する。起動時schema検証により空のLanceDBへ新schemaを作成する。schema mismatchなら停止し、旧LanceDBを混在させず原因を解消する。
7. Redispatchを実行する。memory/thread は対象実ユーザーまたは全件を指定して再投入する。memory の種別を限定する場合は `RedispatchEmbeddings.memory_kinds` に `MEMORY_KIND_*` を指定する。`RedispatchEmbeddings.kinds` は媒体の dispatch kind（TEXT / MEDIA）であり、`memory_kind` のフィルタではない。reflection intent は `kind=INTENT` を指定する。Redispatchは非同期jobの再投入であり、同期upsert完了を意味しない。
8. job完了後、RDBとLanceDBについて種別ごとの件数を照合する。`MemoryVectorService.GetIndexStats`、`ThreadVectorService.GetIndexStats`、reflection sidecarの embedding status を確認する。`GetIntentIndexStats` が未実装の場合は Redispatch応答と sidecar status を用いる。
9. 各実ユーザーについて、RAW、4階層summary、personality、reflectionを単独kindと複数kind ORの両方で検索する。検索probeが期待するIDだけを返すことを確認する。
10. 明示kindを送るクライアントでstaging smoke testを実施し、互換normalizer・`MEMORY_KIND_UNSPECIFIED_CREATE_COMPAT`・deprecated引数拒否の利用メトリクスが監視期間中0であることを確認する。満たすまで互換コードは削除しない。
11. 上記の証跡を保存してから外部トラフィックとワークフローを復帰する。

## ロールバック

問題があれば新サーバーとワークフローを停止したまま、RDBバックアップと退避済み旧LanceDBを対として復元してから旧サーバーを起動します。RDBだけ、またはLanceDBだけを戻してはいけません。contract migration後に旧コードを接続する場合も、旧コードが `memory_kind` 契約を満たすことを事前に確認します。

## 注意

- 新サーバーは旧LanceDB schemaを読まない。旧schemaとの二重読み取り・二重書き込みは行わない。
- embeddableでないcontent type、空content、空descriptionはRedispatchでskipされる。件数照合ではこの仕様を区別して記録する。
- ANN indexは最初のベクトル系クエリで遅延構築される。必要ならトラフィック復帰前に低負荷の検索probeを一度実行する。
