# クライアント用 memory_kind 移行手順

## 用途

この手順は Lookback のように一人分のデータをローカルアプリで利用する
クライアントDB向けです。複数利用者が共有するサーバーDBには使用せず、従来の
`migrate-memory-kind plan`、`apply`、`verify` 手順を使用してください。

`client-apply` は server と同じ mapping と evidence を使用して変換対象を決定します。
事前検証で未解決となった行、または行単位の更新に失敗した行だけを監査JSONへ
記録して継続します。共有サーバーDBには使用せず、厳密な `plan`、`apply`、`verify`
手順を使用してください。

## 実行手順

1. 対象DBをバックアップし、`memory_kind` の expand DDL を適用する。SQLite は
   `infra/sql/sqlite/manual/011_add_memory_kind.sql`、PostgreSQL は
   `infra/sql/postgres/manual/010_add_memory_kind.sql` を使用する。
2. 対象DBに接続する環境変数を設定し、監査ファイルがまだ存在しない場所を選ぶ。
3. 次を実行する。

   ```sh
   cargo run -p grpc-admin --bin migrate-memory-kind -- client-apply \
     --mapping /absolute/path/memory-kind-mapping.json \
     --output /absolute/path/client-memory-kind-audit.json
   ```

4. 監査JSONの `failures` と `warnings` を確認する。行単位の失敗や参照欠損が
   あっても、処理済みの他行は保持したままコマンドは完了する。DB接続不能や必要な
   table/column がない場合は開始不能として失敗する。
   実行開始前に `status: "in_progress"` の監査 journal を同期する。完了時は
   `status: "completed"` の監査へ原子的に置換するため、出力エラー時は残った
   journal または `.completed` ファイルを確認してから再実行する。
5. contract DDL を適用する前に、監査結果とアプリの読み取り結果を確認する。
   警告または失敗が残るDBには、共有サーバー用の contract DDL を適用しない。

## 補完規則

- `--mapping` は server 側の `plan`/`apply` に渡すものと同一のJSONを使用する。
- thread の owner (`user_id`) と `memory_kind`、所属 memory、standalone memory、
  default system memory の kind は server と同じ規則で更新する。既存の有効値も
  server の判定結果と異なれば更新対象である。
- 所属 memory の `user_id` が `100000` 以上である場合は生成系 owner とみなし、
  解決済みの所属 thread の `user_id` に更新する。`100000` 未満の memory author は
  保持する。
- legacy reflection aggregate は server と同じく origin owner ごとの thread に置換する。
  新 thread・label・aggregate key・membership・reflection sidecar を作成してから旧
  aggregate thread を削除する。置換は aggregate 単位で原子的に実行し、失敗時はその
  aggregate だけをロールバックして `failures` に記録する。
- summary 系は label だけで推測せず、server と同じ metadata と owner 根拠を要求する。
  根拠不足・矛盾・参照欠損は更新せず warning に記録する。

## mapping JSON

通常は空の JSON (`{}`) を指定する。これは `summary`、`daily_summary`、
`weekly_summary`、`monthly_summary` の標準 label mapping を使用し、owner は metadata の
`source_user_id` または `user:<id>` label から解決する。

非標準の summary label を使用している場合、または正しい owner を metadata / label から
一意に解決できない場合だけ、次の JSON を指定する。未知の field、空または重複する label、
無効な kind、0 以下の ID はエラーになる。

```json
{
  "summary_labels": [
    { "label": "work_daily", "memory_kind": "DAILY_SUMMARY" }
  ],
  "explicit_owners": [
    { "thread_id": 123, "owner_user_id": 1 }
  ]
}
```

`summary_labels[].memory_kind` は `THREAD_SUMMARY`、`DAILY_SUMMARY`、
`WEEKLY_SUMMARY`、`MONTHLY_SUMMARY` のいずれかである必要がある。`explicit_owners` は
根拠不足の generated thread にだけ追加し、正常な metadata / label 根拠を上書きする用途に
は使用しない。

## external_id 変換規則

- server 側の移行と同じ `namespace_for_external_id` と `owner_scoped` を使い、
  対象IDを `<namespace>:<thread.user_id>:<suffix>` に変換する。512 byte を超える
  場合も server と同じ決定的ハッシュ形式になる。
- `metadata.source` を持ち、その source namespace と一致するID、および
  DAILY / WEEKLY / MONTHLY の kind と一致する `daily:` / `weekly:` / `monthly:` IDを
  対象にする。すでに同じ owner scope を持つIDは変更しない。
- owner は所属 thread がちょうど1件の場合の `thread.user_id` とする。所属なし・
  複数所属・欠損参照・namespace 不一致・変換後IDの重複は変更せず、監査JSONの
  `warnings` に記録する。
- `external_id` 更新が行単位で失敗した場合も、失敗は `failures` に記録して残りの
  移行を継続する。
