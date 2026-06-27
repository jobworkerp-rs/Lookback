# Lookback Worker / Workflow YAML バンドル

`agent-app` がローカル起動する jobworkerp sidecar に対して、起動時に
`jobworkerp-client` 経由で適用するワーカー定義群と、`memories-import`
や `agent-chat-pipeline` から参照するワークフロー定義群を集約する。

memories リポジトリのオリジナル YAML は変更せず、ここに **編集済みの
コピー** を置く運用とする（Phase B 時点で確認済みの方針。memories
バイナリ自体は無改修で sidecar 起動するため、ワークフロー側だけを
ここで差し替える）。

## 構成

```
workers/
├── llm-workers.yaml                # memories-llm worker + batch workers (IMPL-1)
├── lang-workers/workers/<feature>/ # 言語別 single 登録用 (多言語生成、下記参照)
│   ├── <feature>-single.yaml       # workerName: memories-llm 維持 + prompt は context 参照
│   └── prompts/<role>.<lang>.txt   # memories から同期 (sync-memories-prompts.sh)
└── workflows/
    ├── auto-embedding-workers.yaml          # IMPL-7 (Metal)
    ├── thread-summary/
    │   └── thread-summary-batch.yaml        # output_language で single-<lang> を呼び分け
    ├── thread-reflection/
    │   ├── thread-reflection-batch.yaml
    │   └── auto-reflection-*-embedding*.yaml # embedding (据え置き)
    ├── personality/
    │   └── thread-personality-batch.yaml    # merge_enabled で lang-workers の 2層 merge を起動
    ├── agent-chat-pipeline/
    │   └── agent-chat-pipeline.yaml         # IMPL-4b (reflection branch)
    ├── summaries-pipeline/summaries-pipeline.yaml
    ├── {daily,weekly,monthly}-work-summary/<feature>-batch.yaml
    ├── lookback-periodic-run.yaml           # conductor CronScheduler entry
    └── rag/
        └── lookback-recall.yaml             # RAG retrieval tool (IMPL-CHAT-1)

# The RAG chat itself does NOT have a parent workflow YAML — it
# dispatches `memories-llm` directly (ARCH-CHAT-1) with
# `function_options.function_set_name: lookback-rag` (see
# `workers/function-sets.yaml`).
```

## 多言語生成 (ja/en) と lang-workers

生成系 (summary / personality / reflection / work-summary) は出力言語を ja/en で切り替える。
batch は `output_language` を受け取り `workerName: "memories-<feature>-single-${output_language}"` で
**言語別 single worker** を呼び分ける。言語別 single worker は sidecar 起動時に
`memories-import upsert-generation-workers --feature all --language all --channel llm_workflow
--repo-root workers/lang-workers` で登録され、prompt 本文 (`prompts/<role>.<lang>.txt`) を worker の
`settings.workflow_context` に焼き込む (`src-tauri/src/sidecar/generation_workers.rs`)。

### memories からの vendoring 方針 (重要)

`lang-workers/` の single YAML と `workflows/<feature>/<feature>-batch.yaml` は memories の
対応ファイル (`agent-chat-import/{workers,workflows}/<feature>/`) と**フローは同一だが、agent-app 固有
の2点を保持するため手マージ**する。**機械コピーは prompts (`*.txt`) のみ** (`sync-memories-prompts.sh`)。

- **single の LLM 呼び出しは `workerName: "${$workflow.input.llm_worker_name}"` (= `memories-llm`)**。
  memories の `runnerName: LLM` + `settings.ollama` 直指定は**取り込まない** (ローカル llama-cpp 構成を壊す)。
  prompt のみ memories 方式の context 参照 (`$<feature>_system_prompt` / `$<feature>_user_tail` +
  英語見出し + `validate*PromptContext`) に揃える。
- **batch の progress 報告** (`reportProgress` / `progress_processed` / `progress_total` / `at: thread_index`)
  は agent-app 独自 (import toast の `(N/M)` 表示)。memories の reorg では失われているので**保持必須**。

memories 側の更新を取り込むときは `sync-memories-prompts.sh` で prompts を同期し、
`diff-memories-singles.sh` で single/batch の差分を確認して、LLM 呼び出し方式と progress を壊さない
範囲で手マージする。

## function-set (`function-sets.yaml`)

`function-sets.yaml` には 2 つの function-set を定義している。どちらも target は
`lookback_recall`（WORKFLOW worker, using: run）だが、用途別に分離している:

- **`lookback-rag`** — アプリ内 RAG チャットが LLM に見せるツール集合
  (`function_options.function_set_name`)。
- **`lookback-mcp-rag`** — 設定画面で MCP サーバを有効化したときに外部 MCP
  クライアント (Claude Desktop 等) へ公開するツール集合 (FR-MCP-1)。sidecar は
  `MCP_SET_NAME=lookback-mcp-rag` で起動され、MCP の公開ツールがこの set に
  限定される。

**分離の理由**: MCP で公開したいツール範囲と、チャット LLM に呼ばせたいツール
範囲は将来的に別物になりうる。同一 set を共有すると一方の拡張が他方に漏れる。
今は両者とも `lookback_recall` のみだが、`lookback-mcp-rag` の targets を増やせば
MCP 公開範囲だけを独立に広げられる。MCP の有効/無効・接続先ポートの扱いは
design IMPL-10 / `commands/mcp_settings.rs` を参照。

## Embedding model 設定

`auto-embedding-workers.yaml` の `memories-mm-embedding` ワーカー (model_id /
tokenizer_model_id / dtype / device / max_sequence_length) は **Settings 画面
の Embedding model カード** から変更可能。設定は
`<App data dir>/embedding-settings.json` に永続化され、sidecar 再起動時に
`commands/embedding_workers_yaml.rs` が `<App data dir>/workers/staged/auto-embedding-workers.yaml`
へレンダリングして、memories の `MEMORY_WORKERS_YAML` をそちらに切替える。
ここにある committed YAML は **fallback / 開発用** として残してある。

### 重要な制約

- vector_size を変えると既存 LanceDB レコードと次元が合わず、memories の
  起動時プローブが失敗する。Settings UI から保存すると `<App data dir>/lancedb`
  は自動的に `<App data dir>/lancedb-backup/lancedb-<sec>-<nanos>/` に rename
  退避され、空 lancedb が再作成される (チェックボックスを外すと退避せずに削除)。
- 退避後は **Memory** と **Reflection (intent)** の embedding を Settings カードの
  「再生成」ボタンから手動で再生成する必要がある (新しい次元に合わせて全件再投入)。
- 退避ディレクトリは無期限に保持される。手動削除する場合はパスを Settings
  画面の保存後メッセージから確認して `rm -rf <backup_path>`。
- memories がリモート設定 (`connection.json` の mode = remote) のときは UI が
  disable される。リモート memories は独自の vectordb を保持しており、ローカル
  側で sidecar を切り替えても反映されないため。
- 起動失敗時は自動 rollback: 旧設定 / 旧 lancedb を復元して再起動する。

## チャネル設計

`agent-app` の sidecar は jobworkerp に対して以下の env を渡す:

- `WORKER_CHANNELS=llm,llm_external,llm_workflow,llm_batch,llm_pipeline,llm_periodic,embedding,embedding_workflow,rag`
- `WORKER_CHANNEL_CONCURRENCIES=1,2,1,2,1,1,1,1,2`

LLM 系は strict parent → child の階層構造、`llm_pipeline` はそれと別系統
の最上位 parent として運用する:

| 階層 | Channel | concurrency | 担当 |
|-----|---------|-------------|------|
| 0 | `llm` | 1 | `memories-llm` (LLMPromptRunner) の LLM job 本体。GPU/モデルメモリ競合を回避するため逐次化 |
| 0 | `llm_external` | 2 | External API LLM (`memories-llm-external`)。API-bound なので local GPU 用の `llm` とは分離 |
| 1 | `llm_workflow` | 1 | LLM step を **直接** 含む single workflow (thread-summary-single / thread-personality-single / thread-reflection-single / user-personality-merge) |
| 2 | `llm_batch` | 2 | LLM workflow を **fan-out** する batch workflow (thread-summary-batch / thread-personality-batch / thread-reflection-batch、および agent-chat-pipeline / summaries-pipeline から呼ぶ daily/weekly/monthly-work-summary-batch) |
| ─ | `llm_pipeline` | 1 | `memories-summaries-pipeline` (生成ダイアログの段階生成親)。`llm_batch` の子を順次呼ぶため **別 channel** に分離。concurrency 1 で同時 1 pipeline に制限 |
| ─ | `llm_periodic` | 1 | conductor の `CronSchedulerService` から実行される `memories-lookback-periodic-run`。手動生成と競合しすぎないよう定期実行 parent を逐次化 |
| 0 | `embedding` | 1 | `memories-mm-embedding`。Metal/GPU bound の embedding job 本体 |
| 1 | `embedding_workflow` | 1 | auto-embedding / reflection-intent embedding workflow |
| ─ | `rag` | 2 | RAG チャットの retrieval ツール (`lookback_recall`)。memories-llm の chat ジョブが function-calling で発火する非 GPU の gRPC 検索。`llm` (concurrency=1) とは別 channel に分離して、チャット LLM が自身のツール待ちで自分の slot を解放できずデッドロックする事態を避ける (DECIDE-CHAT-3) |

LLM 階層 (`llm` → `llm_workflow` → `llm_batch`) の concurrency は
**child < parent** (`1 < 1 < 2`) を満たさなければならない。これは
「parent が唯一の slot を占有しながら child の完了を待ち、child は slot を
取れず開始できない」というデッドロックを避ける基本要件。memories の
`embedding_workflow` (auto-embedding-workers.yaml) が `embedding`
(concurrency=1) から別 channel に分離されているのと同じ設計パターン。

`llm_pipeline` は `llm_batch` の **更に上位** だが、同 channel で slot を
奪い合うとデッドロックするため別 channel に置く (この場合 strict-increase
ルールは適用されない)。concurrency を 1 にするのは、複数 pipeline を同時に
実行状態にすると下位 slot 不足で待たされた pipeline が timeout するため
(超過分はキュー待ち = 実行状態でない = timeout 対象外)。

memories-import の `--summarize-channel` / `--personality-channel` には
**`llm_batch`** を渡す。pipeline 経由の段から呼ぶ `*-batch.yaml`
invocation も `options.channel: llm_batch` を指定する。
`user-personality-merge` は内部に直接 LLM step を持つ単一 workflow なので
`llm_workflow` 側に残す。
大量の layer-1 signal を統合する merge はローカル LLM で長時間化しやすい。
`user-personality-merge` 単体 workflow は 6 時間まで許容し、ユーザー操作から
到達する `thread-personality-batch` / `agent-chat-pipeline` 内の merge 呼び出しと
Tauri の jobworkerp dispatch timeout は 3 時間に揃える。`マージのみ` は
`user-personality-merge` 内の `mergeProfile` から `memories-llm` を直接呼ぶため、
この inner LLM step にも 3 時間 timeout を明示する。
`user-personality-merge` の LLM 入力は最大 100 signal だが、signal JSON の
`max_context_chars` は 150,000 に抑える。262k context のモデルでも system prompt /
schema / response budget の余白を残し、長文 signal で性能が落ちるのを避けるため。

## 定期実行 workflow

`memories-lookback-periodic-run` は conductor の `CronSchedulerService` から呼ばれる
Lookback 専用 wrapper worker。登録 channel は `llm_periodic`、concurrency は `1`。

入力は conductor args の `input` JSON 文字列として渡され、次の形を取る。

```json
{
  "schema_version": 1,
  "task": {
    "name": "朝の要約",
    "source": "codex",
    "task_kind": "regular",
    "hour": 9,
    "minute": 0,
    "interval_hours": 24,
    "interval_days": null,
    "weekly_day": null,
    "monthly_day": null,
    "lookback_days": 7,
    "force_thread_summary": true
  },
  "runtime": {
    "memories_grpc_host": "127.0.0.1",
    "memories_grpc_port": 9010,
    "memories_grpc_tls": false,
    "llm_worker_name": "memories-llm",
    "output_language": "ja",
    "memories_import_bin": "/Applications/Lookback.app/Contents/MacOS/memories-import"
  }
}
```

`runtime.output_language` は各 batch worker に転送され、batch は
`memories-<feature>-single-<lang>` / `memories-user-personality-merge-<lang>`
を worker 名で呼び分ける。single / merge YAML の絶対パスは runtime に保存しない。
言語別 worker は起動時に `workers/lang-workers` から登録されるため、bundle 位置が
変わっても scheduler の保存済み runtime が陳腐化しない。
`runtime.memories_import_bin` は `regular` タスクの import 前段で使う
`memories-import` の解決済みパス。旧 scheduler は未指定でも読み込めるが、起動時の
runtime refresh で現在のパスに再保存される。

`regular` タスクの `task.source` / `task.sources` は import 元を表す。
`codex+claude-code` は `codex` と `claude-code` の両方に展開され、wrapper 内の
`runImportCodex` / `runImportClaudeCode` が必要な方だけ実行される。その後、同じ
source filter を使って thread summary / daily summary / personality / reflection を
実行する。`agent-chat-pipeline.yaml` はこの conductor 定期実行経路では使わない。

初回起動時、Lookback は無効状態の既定 scheduler を 3 件だけ投入する
(`Daily import and summaries`, `Weekly summary`, `Monthly summary`)。投入済み marker は
data root の `periodic-defaults-seeded.json` に保存されるため、ユーザがテンプレートを
削除しても次回起動で復活しない。

運用手順:

1. Lookback 起動時に `src-tauri/src/sidecar/lifecycle.rs` が
   `llm_periodic` を含む `WORKER_CHANNELS` / `WORKER_CHANNEL_CONCURRENCIES`
   を jobworkerp に渡す。
2. `workers/llm-workers.yaml` が `memories-lookback-periodic-run` を登録する。
3. `conductor-main` 起動後、Lookback が既存 scheduler の runtime endpoint
   (gRPC port + `output_language`) を現在の値へ再保存する。
4. conductor の cron 発火時、wrapper workflow が `task.task_kind` に応じて処理する。
   `regular` は source import の後に thread summary と optional な daily /
   personality / reflection を呼ぶ。`weekly` / `monthly` は既存の daily / weekly
   summary を集約する。各 batch は言語別 single worker を worker 名で解決する。

## LLM モデルの集中管理

memories リポジトリ版の workflow YAML は `runnerName: LLM` + inline
`settings.ollama` で Ollama を直接呼び出す構成だが、agent-app では
**`workerName: memories-llm` 参照** に統一している。モデル切替は
`workers/llm-workers.yaml` の `LOOKBACK_LLM_MODEL` / `LOOKBACK_LLM_HF_REPO`
/ `LOOKBACK_LLM_CTX_SIZE` / `LOOKBACK_LLM_KV_CACHE_TYPE` env で行い、
各 workflow YAML を触る必要がない。

既定モデル: Qwen3.6-27B (Unsloth GGUF, Q4_K_XL)。
- `LOOKBACK_LLM_MODEL=Qwen3.6-27B-UD-Q4_K_XL.gguf`
- `LOOKBACK_LLM_HF_REPO=unsloth/Qwen3.6-27B-GGUF`
- `LOOKBACK_LLM_CTX_SIZE=8192`
- `LOOKBACK_LLM_KV_CACHE_TYPE=KV_CACHE_TYPE_Q4_0`

KV cache type は `type_k` / `type_v` の両方へ同じ値を渡す。既定は
`Q4_0`。長い `ctx_size` ではKV cacheがRAM消費を大きく左右するため、
設定画面のRAM目安は「プリセットのモデル本体目安 + `ctx_size` と
KV cache typeから算出したKV cache目安」を表示する。

設定画面で選べるKV cache type:

- `Q4_0`
- `Q4_1`
- `IQ4_NL`
- `Q5_0`
- `Q5_1`
- `Q8_0`

## reflection ステージ

`agent-chat-pipeline.yaml` に IMPL-4b で reflection branch を追加した。
有効化条件:

1. pipeline 入力で `thread_reflection_batch_yaml` /
   `thread_reflection_single_yaml` を指定 (空文字なら branch skip)
2. memories sidecar 起動時に `MEMORY_REFLECTION_DISPATCH_ENABLED=true`
   が注入されていること (Phase C で `lib.rs` から true を渡す)
3. `reflection_prompt_version` (既定 `"v1"`) を pipeline 入力で供給
