# Lookback の memory_kind 移行

この手順は、起動画面で「移行を実行」がエラーになった場合にのみ「手動移行手順を開く」から使う、配布アプリ向けのフォールバック手順です。まずは必ず「移行を実行」を使用してください。アプリは手動移行中は停止したままにしてください。

1. `<data-root>/memories/default.sqlite3` と `<data-root>/lancedb` を同じ時点のバックアップとして退避します。
2. アプリ bundle 内の `migration-toolkit/` にある `sqlite/011_add_memory_kind.sql` を SQLite に一度だけ適用します。SQL と runbook はこのフォルダ内の同梱物だけを使います。
3. macOS では `Lookback.app/Contents/MacOS/migrate-memory-kind`、Linux では Lookback 実行ファイルと同じディレクトリの `migrate-memory-kind` を実行します。`<data-root>` は Lookback の設定で選択したデータ root に置き換えます。

   ```sh
   export SQLITE_URL="sqlite://<data-root>/memories/default.sqlite3"
   printf '{}' > /absolute/path/memory-kind-mapping.json
   /absolute/path/to/migrate-memory-kind client-apply \
     --mapping /absolute/path/memory-kind-mapping.json \
     --output /absolute/path/client-memory-kind-audit.json
   ```

4. audit の `status` が `completed` で、`warnings` と `failures` が空であることを確認します。空でない場合は contract SQL を適用せず、バックアップから復元するか原因を修復します。
5. `sqlite/012_contract_memory_kind.sql` を適用し、LanceDB を退避して再構築します。詳細な監査規則、mapping JSON、Redispatch は同じフォルダの `memory-kind-client-migration_ja.md` と `vectordb-rebuild-runbook_ja.md` に従います。
6. Lookback を再起動します。
7. 起動後に「移行後のベクトル再生成が完了していません」と表示された場合、Settings の Embedding index で「移行後の再生成を再試行」を選びます。SQLite の移行は完了しているため、SQLite や退避済み LanceDB を復元せず、memory・thread・reflection の Redispatch だけを再実行します。成功すると通知は消えます。
