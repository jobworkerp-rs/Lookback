# Lookback Worker / Workflow YAML バンドル

`agent-app` がローカル起動する jobworkerp sidecar に対して、起動時に
`jobworkerp-client` 経由で適用するワーカー定義群と、Lookback の import 処理や
定期実行から参照するワークフロー定義群を集約する。

memories リポジトリのオリジナル YAML は変更せず、ここに **編集済みの
コピー** を置く運用とする（Phase B 時点で確認済みの方針。memories
バイナリ自体は無改修で sidecar 起動するため、ワークフロー側だけを
ここで差し替える）。

Issue tracking の正本は standalone の `issue-tracker-server` です（release に同梱する条件は [開発者ガイド](../docs/developer-guide_ja.md#experimental-issues-ビルド) を参照）。Lookback は loopback gRPC で接続し、生の manager method は MCP / FunctionSet / UI / Tauri command に出しません。jobworkerp 側に載せるのは次の系統です。

- `lookback-issue-extract`（`issue_extract` チャネルの WORKFLOW）。tracker 未 READY でも登録する。DISABLED またはシグナル偽なら LLM / RPC せず skip する。host 空（未 READY）は失敗ログのみで run もキューも作らない。
- `lookback-thread-issue-resolution`（`issue_extract`）。スレッド要約保存後の後続メッセージを照合し、既存 Manager API により確定する。再起動後のジョブ再送は行わない。`ListReviewCandidates` は server が page_size を 100 に丸め `totalSize` を返さないため、全ページを走査し、token 消尽・composite snapshot の一致・candidate ID の一意性で完走を保証する（件数照合はしない）。走査中に保持するのはこの thread を source とする候補行と全候補の ID だけで、ページ応答は走査後に破棄する。engine は jq 式の評価ごとに workflow context 全体を複製するため、全件を保持すると候補 1 件あたり数分かかる。
- `lookback-thread-issue-resolution-manual`（`issue_resolution_manual`）。保存済み要約を再生成せずに前項の worker を起動する。同じ単一 slot のチャネルで親子を実行するとデッドロックするため、親は独立チャネルを使う。判定範囲の checkpoint は summary metadata の `resolution_high_water_message_*`（自動照合が一度でも投入された summary）を優先し、無ければ summary が処理した `last_processed_message_*` を使う。どちらも無い旧 summary は `saved_summary_missing_or_incomplete` で終わるので、先に thread 要約を更新する。候補を持つ全 source thread にまとめて投入する開発者向けスクリプトが `scripts/backfill-thread-issue-resolution.sh`（`--dry-run` で対象一覧のみ、`--newest-first` で候補の作成日時が新しい thread から、`--candidates-since YYYY-MM-DD` でその日以降に作成された候補を持つ thread だけを対象にし、終了時に status 別件数と要約更新が必要な thread を出力。結果は `<data dir>/backfill-thread-issue-resolution.progress` に追記され、COMPLETE / UNCONFIRMED の thread は次回以降スキップされるため `--limit N` の繰り返しで未処理分を順に消化できる。FAILED は再試行対象、`--include-done` で全件やり直し）。checkpoint の無い旧 summary は `--refresh-summaries FILE` で `memories-summarize-batch` を thread ID 指定・force・抽出無効で 1 回投入して checkpoint を付けてから照合する。jobworkerp / Issue Tracker のポートは sidecar プロセスの listen port を reflection で探索する。
- `lookback-thread-group-issue-resolution`（`issue_resolution_group`）。ThreadGroup の他メンバーの証拠で候補を照合する。グループ要約 single は結果を待たずに投入する（下流タスク）。候補数に比例して長くなるため、スレッド要約が完了を待つ `issue_extract` を塞がないよう独立チャネルを使う。再起動後のジョブ再送は行わない。Review candidate の読取りは同一 thread 照合と同じ全ページ走査で、保持するのは active member を source に含む候補行だけである（group 外の候補は skipped にも数えない）。operation history が 1 ページ（100 件）を超えた候補は保護判定できないため failed として記録する。候補と member thread は、抽出時の source label snapshot と member の labels で同じ repository / path かを判定し（規則は workflow のコメント参照）、判定できない候補は根拠に `[[issue:<id>]]` 参照を要求する。入力 `dry_run: true` は判定結果を `dry_run_candidates` に出し、書き込みを行わない（検証用）。
- `lookback-issue-resolution-{review-candidates,prepare,origin,judge,apply}`（`issue_resolution_shared`）。Thread Group 照合と cross-thread 照合が共有する候補ごとの処理（候補一覧、保護判定、origin 時刻、根拠 thread の生メッセージによる LLM 判定、`ResolveCandidate` の書込みと確定確認）。呼出し元は結果を待つため、呼出し元とは別の 2 slot チャネルで動かす。各 worker は skip / 保護 / 失敗を呼出し元と同じ形のリストで返し、呼出し元がそれを結合する。prepare は `RESOLUTION_PENDING` と active claim を保護し、関連 Issue を読めない候補は failed にする。judge は根拠 thread ごとに読むメッセージの上限時刻（要約 checkpoint）を受け取れ、取得・LLM 呼出しの失敗だけを `transient_failure` とする。
- `lookback-cross-thread-issue-resolution`（`issue_resolution_cross`、`queue_type: WITH_BACKUP`）。source thread 以外の後続 thread の会話で Candidate を終端化する（仕様は #202）。thread 要約 single が抽出の後に結果を待たずに予約する。policy の `resolution_mode` と `cross_thread_resolution_mode`（UI 非露出、既定 DISABLED）が両方 `APPLY` のときだけ予約される。処理範囲は、最後に一時失敗なく終わった自動照合の開始時刻（時計ずれ分を引く）から照合開始までに要約された thread と、作成・更新された Candidate である。その開始時刻は、照合が一時失敗なく終わったときだけ投入する目印ジョブ `lookback-cross-thread-issue-resolution-watermark`（COMMAND `true`、`store_success`）の `uniq_key` に入れ、次の照合が jobworkerp のジョブ結果から読む（WORKFLOW ジョブは workflow が失敗しても SUCCESS で終わるため、ジョブ自体の状態は使えない）。Candidate ごとの処理は `lookback-cross-thread-issue-resolution-candidate`（`issue_resolution_cross_candidate`）が行い、thread hybrid 検索で同じ project の thread を探して根拠 thread ごとに共有 judge で判定し、全判定が正常で終端判定が一種類のときだけ共有 apply で書き込む。埋め込み worker 名と jobworkerp の接続先は Memories 起動後の登録時に置換する。`lookback-cross-thread-issue-resolution-manual`（`issue_resolution_manual`）は運用者の明示実行用で、`since_ms` と `dry_run` を受け取り、自動照合の処理範囲を動かさない。
- cross-thread 照合の導入手順（運用者向け、`scripts/lib/lookback-sidecar.sh` を共有するスクリプトで行う）:
  1. 要約生成（定期実行・import）を止めた状態で `scripts/migrate-repo-labels.sh` を実行し、旧形式の `repo:` label と正規形の対応・件数を確認する。問題なければ `--apply` で thread（要約を保持する memory thread を含む）の label を書き換える。旧形式は exact match の thread 検索で見つからないため、移行しないとその thread は根拠にならない。
  2. `scripts/backfill-cross-thread-issue-resolution.sh`（既定は dry run）でレビュー待ち全件を判定し、`would write` と `contradicting judgements` の内容、`<data dir>/backfill-cross-thread-issue-resolution-<time>.json` の全出力を確認する。必要なら `--search-limit` / `--score-min` を変えて再実行し、決めた値を workflow の入力既定値に反映する。
  3. 判定に問題がなければ `--apply` で既存の滞留分を書き込む（`resolution_mode=APPLY` が必要）。
  4. `<data dir>/issue-auto-policy/current.json` の `policy.cross_thread_resolution_mode` を `"APPLY"` にする（UI には出さない）。以後は thread 要約ごとに自動照合が予約される。
- `lookback_issue_*`（GRPC Gateway）。tracker が READY になってから登録し、`lookback-mcp` 経由でのみ公開する。

## 構成

```
workers/
├── llm-workers.yaml                     # memories-llm worker + batch workers
├── lookback-issue-extract-workers.yaml  # extract / resolution WORKFLOW workers
├── lookback-issue-gateway-workers.yaml  # READY 後の IssueGateway GRPC workers
├── function-sets.yaml                   # lookback-rag（チャット）と lookback-mcp（MCP）
├── lang-workers/workers/<feature>/      # 言語別 single 登録用 (多言語生成、下記参照)
│   ├── <feature>-single.yaml            # workerName: memories-llm 維持 + prompt は context 参照
│   └── prompts/<role>.<lang>.txt        # memories から同期 (sync-memories-prompts.sh)
└── workflows/
    ├── issue-extract/issue-extract.yaml # extract 本体（窓切り・Apply・失敗記録）
    ├── thread-issue-resolution/         # 同一 thread 解決照合と手動再評価
    ├── issue-resolution-shared/         # group / cross-thread 照合が共有する候補ごとの処理
    ├── cross-thread-issue-resolution/   # source thread 以外の後続 thread による終端化（親と Candidate ごと）
    ├── auto-embedding-workers.yaml      # Metal embedding worker
    ├── thread-summary/
    │   └── thread-summary-batch.yaml    # output_language で single-<lang> を呼び分け
    ├── thread-group-summary/
    │   └── thread-group-summary-batch.yaml # member summary を owner 単位で更新し、group ごとに single-<lang> を呼ぶ
    ├── thread-reflection/
    │   ├── thread-reflection-batch.yaml
    │   └── auto-reflection-*-embedding*.yaml # embedding (据え置き)
    ├── personality/
    │   └── thread-personality-batch.yaml    # merge_enabled で lang-workers の 2層 merge を起動
    ├── {daily,weekly,monthly}-work-summary/<feature>-batch.yaml
    ├── lookback-periodic-run.yaml       # conductor CronScheduler entry
    └── rag/
        └── lookback-recall.yaml         # RAG retrieval tool

# The RAG chat itself does NOT have a parent workflow YAML — it
# dispatches `memories-llm` directly with
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
- **Issue 連携** (`issue_tracker_*` / `policy_json` 入力、Issue の抽出と解決照合、
  schema 必須項目 `has_unfinished_issue_candidates`) は agent-app 独自。memories の prompt には
  `has_unfinished_issue_candidates` の指示が無いので、thread-summary と thread-group-summary の
  prompts も手マージ対象とし、`sync-memories-prompts.sh` は上書きしない。
- **batch の progress 報告** (`reportProgress` / `progress_processed` / `progress_total` / ループの `at`)
  は import・要約生成・単独生成の `(m/N)` 表示に使う。`m` は処理中の項目番号（添字 + 1）、`N` は
  対象件数であり、成功件数ではない。対象0件ではカウンターを表示しない。memories 側にも同じ step が
  あるが、取り込み時に消さないこと。

memories 側の更新を取り込むときは `sync-memories-prompts.sh` で prompts を同期し、
`diff-memories-singles.sh` で single/batch と手マージ対象の prompts の差分を確認して、LLM 呼び出し方式・
Issue 連携・progress を壊さない範囲で手マージする。

## 要約バッチの結果契約

thread / daily / weekly / monthly の各要約 batch は、子 single workflow の結果を
`generated`、`skipped`、`failed` に分類して返す。`skipped` は既存かつ最新の要約、または
入力不足による正常な非生成結果であり、`succeeded_count` に含まれる。したがって
要約タブの手動生成はアプリ側で `thread → ThreadGroup → daily → weekly → monthly` の順に
選択された batch を別ジョブとして実行し、全件が正常スキップされた stage の後続も実行する。
既定の `stop_on_error: false` では、失敗した stage を表示し、後続も続行する。
最初の失敗時に停止したい場合だけ `stop_on_error: true` を渡す。部分失敗の警告でも失敗件数が
あれば後続を停止する。path だけ失敗した期間 batch は `completed_with_errors` として警告表示する。
終端出力がない実行は成功扱いにせず、期間 batch の失敗は対象期間ごとに復旧記録へ残す。
旧 `summaries_pipeline` の復旧記録も表示でき、同じ対象期間を再生成して成功すると解消される。
段ごとの進捗は実行IDごとに保存・復元し、import と要約生成が並行しても上書きしない。

LLM で要約を生成する各 workflow（thread / ThreadGroup / daily / weekly / monthly）は、
生成結果を保存前に検証する。JSON として解釈できない出力、型が契約と異なる出力、または
title・summary・overall_purpose・purpose が空の出力は `…/invalid-output` エラーとし、
そのエラーに限って再生成する（回数は各 workflow の `catch.retry`）。constrained JSON 生成で thinking モデルが
スキーマの骨格だけを空欄で返すことがあるためである。再生成しても満たさない場合は
その単位を失敗として batch に返し、空の要約は保存しない。

thread summary と personality signal の memory は、元スレッドの会話時刻（`created_at` は
`first_message_at`、`updated_at` は `last_message_at`）を新規作成時も更新時も持つ。スレッドの
`created_at` / `updated_at` はサーバーの監査時刻なので使わない。日次要約・user personality は
この `updated_at` で対象日や最新性を決め、再処理の要否もこの値と `last_message_at` の比較で判定する。

最新判定の正本は `thread-summary-single` で、メッセージを読む前（スレッドと既存の summary だけを取得した段階）に
判定し、最新なら `skipped: already up to date` で終える。`thread-summary-batch` は対象を絞り込んだうえで
全件の single を呼ぶ。最新のスレッドは single の前段でスキップされるので、メッセージの取得や LLM の呼び出しは起こらない。
この thread summary は期間要約とスレッドグループ要約の内部入力であり、RAG/MCP の公開検索結果や
Lookback の要約一覧・検索・選択 UI には含めない。

`thread-group-summary-batch` は3段階で動く。まず対象 group の active member をすべて読み、
次に member のスレッドごとに `memories-thread-summary-single-<lang>` を1件ずつ呼んで member summary を更新し
（各件の前に `reportProgress` で「n / メンバー数」を出す）、最後に group ごとに言語別 single
(`memories-thread-group-summary-single-<lang>`) を呼んで集約・保存する。複数 group に属する thread も1回しか再要約しない。refresh に失敗した thread を
含む group だけを `thread-refresh-failed`（`incomplete_input`）として失敗させ、それ以外の group は集約を続ける。
メッセージ数が `min_message_count` に満たず thread-summary で要約がスキップされ、
要約 memory も持たない member は集約入力から除外する（single の `skip_reason` がメッセージ不足の
スレッドを `insufficient_message_thread_ids` に集める）。全 member が除外された group は `skipped_groups` になる。
それ以外の理由で member summary が欠けている場合、single は `incomplete_input` を返し、batch は
`thread-summary-missing`（`incomplete_input`）として失敗させる。
policy の `resolution_mode` が `APPLY` のとき、single は要約を新しく公開した group についてだけ
`lookback-thread-group-issue-resolution` を結果を待たずに投入する（`await: false`）。投入した group は
batch の `resolution_dispatched_groups`、投入の失敗は `resolution_failures` に group 単位で集め、
要約の成否には影響させない。照合自体の結果はこの batch には返らない。投入した照合 job の期限は、
呼び出しの `timeout`（照合文書と同じ 24 時間）が job timeout として渡される。
`target_group_id` を指定した実行では一つの group だけを対象にし、
`expected_membership_snapshot_digest` と現在の digest が異なる場合は書き込みを中止する。
終端結果には `generated_groups`、`skipped_groups`、`failed_groups`、`expected_group_count` を含め、
部分的または不完全な対象実行も復旧記録から再試行できる形で返す。

## function-set (`function-sets.yaml`)

`function-sets.yaml` には 2 つの function-set を定義している。どちらも RAG の
`lookback_recall`、`lookback_get_context`、`lookback_get_reflections`（WORKFLOW worker,
using: run）を target とするが、用途別に分離している:

- **`lookback-rag`** — アプリ内 RAG チャットが LLM に見せるツール集合
  (`function_options.function_set_name`)。
- **`lookback-mcp`** — 設定画面で MCP サーバを有効化したときに外部 MCP
  クライアント (Claude Desktop 等) へ公開するツール集合。sidecar は
  `MCP_SET_NAME=lookback-mcp` で起動され、MCP の公開ツールがこの set に
  限定される。RAG target に加え、Issue Tracker が READY のときだけ
  curated `lookback_issue_*` GRPC worker を載せる。

**分離の理由**: MCP で公開したいツール範囲と、チャット LLM に呼ばせたいツール
範囲は別物になりうる。同一 set を共有すると一方の拡張が他方に漏れる。
`lookback-mcp` の Issue target は unary method だけを載せる。Tracker が READY
でないときは sidecar が Issue worker を登録せず、RAG だけの MCP を維持する。
MCP の有効/無効・接続先ポートの扱いは `commands/mcp_settings.rs` を参照。

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
- memories がリモート設定 (`connection.json` の `memories.mode` = remote) のときは UI が
  disable される。リモート memories は独自の vectordb を保持しており、ローカル
  側で sidecar を切り替えても反映されないため。
- 起動失敗時は自動 rollback: 旧設定 / 旧 lancedb を復元して再起動する。

## チャネル設計

`agent-app` の sidecar が jobworkerp に渡すチャネル設定のうち、生成・検索系は以下のとおり
（Issue Tracker・coding agent 用のチャネルも別途登録する。全体は `jobworkerp_channel_env_vars` を参照）:

- `WORKER_CHANNELS=llm,llm_external,llm_workflow,llm_batch,llm_periodic,thread_group_summary,embedding,embedding_workflow,rag`
- `WORKER_CHANNEL_CONCURRENCIES=1,2,1,2,1,1,1,1,2`

LLM 系は parent → child の階層構造として運用する。手動の段階生成には親 workflow の
チャネルを使わず、アプリが各 batch を順に投入する:

| 階層 | Channel | concurrency | 担当 |
|-----|---------|-------------|------|
| 0 | `llm` | 1 | `memories-llm` (LLMPromptRunner) の LLM job 本体。GPU/モデルメモリ競合を回避するため逐次化 |
| 0 | `llm_external` | 2 | External API LLM (`memories-llm-external`)。API-bound なので local GPU 用の `llm` とは分離 |
| 1 | `llm_workflow` | 1 | LLM step を **直接** 含む single workflow (thread-summary-single / thread-group-summary-single / thread-personality-single / thread-reflection-single / user-personality-merge)。lang-worker の single は、sidecar が `upsert-generation-workers` に渡す `--channel llm_workflow` でこの channel に登録される |
| 2 | `llm_batch` | 2 | LLM workflow を **fan-out** する batch workflow (thread-summary-batch / thread-personality-batch / thread-reflection-batch、および手動生成 / periodic-run の daily/weekly/monthly-work-summary-batch) |
| ─ | `llm_periodic` | 1 | conductor の `CronSchedulerService` から実行される `memories-lookback-periodic-run`。手動生成と競合しすぎないよう定期実行 parent を逐次化 |
| ─ | `thread_group_summary` | 1 | 手動・import・定期実行の ThreadGroup 集約。内部で `llm_workflow` の single（スレッド要約・グループ要約）を待つため、それらと分離 |
| 0 | `embedding` | 1 | `memories-mm-embedding`。Metal/GPU bound の embedding job 本体 |
| 1 | `embedding_workflow` | 1 | auto-embedding / reflection-intent embedding workflow |
| ─ | `rag` | 2 | RAG チャットの retrieval ツール (`lookback_recall`)。memories-llm の chat ジョブが function-calling で発火する非 GPU の gRPC 検索。`llm` (concurrency=1) とは別 channel に分離して、チャット LLM が自身のツール待ちで自分の slot を解放できずデッドロックする事態を避ける (DECIDE-CHAT-3) |

LLM 階層 (`llm` → `llm_workflow` → `llm_batch`) の concurrency は
**child < parent** (`1 < 1 < 2`) を満たさなければならない。これは
「parent が唯一の slot を占有しながら child の完了を待ち、child は slot を
取れず開始できない」というデッドロックを避ける基本要件。memories の
`embedding_workflow` (auto-embedding-workers.yaml) が `embedding`
(concurrency=1) から別 channel に分離されているのと同じ設計パターン。

手動の段階生成同士はアプリ側で直列実行する。待機中の生成は batch を投入しないため、
前の生成の終了を待つ時間は batch の job timeout に含まれない。待機中も中断できる。

memories-import の `--summarize-channel` / `--personality-channel` には
**`llm_batch`** を渡す。手動の段階生成も同チャネルの batch worker を直接呼ぶ。
`user-personality-merge` は内部に直接 LLM step を持つ単一 workflow なので
`llm_workflow` 側に残す。
merge 呼び出しは run/call の既定 1 時間に収まる（実測最大約 8 分、下記「タイムアウト」）。
`user-personality-merge` の LLM 入力は最大 100 signal だが、signal JSON の
`max_context_chars` は 150,000 に抑える。262k context のモデルでも system prompt /
schema / response budget の余白を残し、長文 signal で性能が落ちるのを避けるため。
`thread-reflection-batch` は 110k token までの thread context を single workflow に渡すため、
`memories-thread-reflection-single-<lang>` の呼び出しも run/call の既定 1 時間に収まる（実測最大約 10 分）。

## リモート jobworkerp で使う

Connection で jobworkerp を Remote server にした場合、このバンドルの worker は起動時には
登録されない。Settings の Connection にある「リモート jobworkerp の準備状況」から確認・登録する
（操作手順は `docs/developer-guide_ja.md` の「接続先の設定とリモート接続の診断」）。

- **登録するもの**: このバンドルの worker・function set、生成系の lang-worker、query embedding
  worker。memories / Issue tracker / jobworkerp 自身のアドレスは「jobworkerp から見たアドレス」
  （未設定なら Connection の URL）で展開する。
- **登録しないもの**: coding agent の worker（チャネル `native-interactive` / `coding-agent-one-shot` も不要）と、chat の Issue
  ツール。chat の Issue ツールはローカルの facade を呼ぶため、リモートからは届かない。Issue
  の抽出・解決 workflow は Issue tracker の URL を使うので登録する。
- **リモート側で用意するもの**:
  - 上記「チャネル設計」のチャネル（coding agent 用を除く）。必要な `WORKER_CHANNELS` /
    `WORKER_CHANNEL_CONCURRENCIES` と `TZ` などの環境変数は準備状況パネルに表示される。
  - plugin runner `LLMPromptRunner` と `MultimodalEmbeddingRunner`（`PLUGINS_RUNNER_DIR`）。
  - 外部 LLM を使う場合は、その API キーの環境変数。
- **定期 import**: `memories-lookback-periodic-run` はリモート jobworkerp 上で動くため、
  memories-import・opencode DB・作業ディレクトリのパスは Connection の「jobworkerp ホスト上の
  パス」で指定する。
- 登録内容はアプリのバージョンと展開後の YAML から fingerprint を取り、リモートごとに記録する。
  アプリの更新や設定変更で YAML が変わると、準備状況に「別のバージョン」と表示されるので
  登録し直す。

## タイムアウト

jobworkerp のタスクは既定ではタイムアウトしない。`WORKFLOW_TASK_DEFAULT_TIMEOUT_SEC` を
設定すると、`timeout` を持たない run / call タスクにだけその値が適用され、do / try / for /
fork などのコンテナには適用されない。タイムアウトしたタスクの内側の子ジョブは取り消される。

1. アプリは jobworkerp を `WORKFLOW_TASK_DEFAULT_TIMEOUT_SEC=3600` で起動する
   （`src-tauri/src/sidecar/lifecycle.rs`）。LLM 呼び出しや 1 件ごとの single 呼び出しの
   ハングを 1 時間で検出する。実測の最大は約 14 分（スレッド要約）で、十分に収まる。
2. 要約系ワークフローに `timeout` を書くのは、1 時間を超えるのが正常な子バッチ呼び出し
   （run タスク）と、`await: false` で投入する呼び出し（timeout が投入した job の期限になる）だけ。
   コンテナ・文書直下・1 件ごとの呼び出しには書かない。呼び出しが timeout すると
   その段の `catch` に入る。手動の段階生成は、各段の失敗と `stop_on_error` により後続を判断する。
3. Tauri から投入する job timeout は、待つ子バッチ呼び出しの timeout より長くする
   （`src-tauri/src/jobworkerp/job_timeouts.rs`）。ストリーミングジョブの job timeout は
   「出力が途切れてよい最大時間」として働き、期限に達すると子ジョブも取り消される。
   定期実行（`memories-lookback-periodic-run`）は conductor が投入するため、同じ規則の値
   （`PERIODIC_RUN`）を CronScheduler の `job_timeout_sec` に設定する。この値は画面に表示されない
   ため、不足していればアプリが直す（`src-tauri/src/commands/periodic_tasks.rs`）。
4. memories が正本の埋め込みワークフロー（`auto-*embedding*.yaml`、`auto-image-caption.yaml`）は
   memories 側と同一でなければならない（`scripts/verify-embedding-contract.mjs`）。変更は memories 側で行う。

### 子バッチ呼び出しの上限の算出

実測値（2026-10-14 時点、LLM: LAN 内の OpenAI 互換サーバー上の Qwen）から次の式で決める。

- バッチ: 件数（実測最大に余裕を上乗せ、一括インポートの外れ値を除く）× 1 件の p90 × 混雑係数 1.5
- 手動の段階生成: 各段に対応する batch の job 期限を適用し、全段の合計期限は設けない
  （期間バッチは下位の要約を読むだけで、下位の生成は含まない）。

| 前提 | 値 |
|---|---|
| 会話スレッド数 | 200 / 日、650 / 週（1 週分の未要約を一度に処理できること） |
| パス数 | 8 / 日、15 / 週、25 / 月 |
| 活動グループ数 | 350 / 週（うち LLM 集約対象 50） |
| 1 件の p90（秒） | スレッド要約 421、日次 79、週次 254、月次 600（未計測。出力上限で頭打ちとして仮置き）、グループ LLM 120（LLM なし 21）、reflection 318、personality 224 |

| 子バッチ呼び出し | 上限 | 算出 |
|---|---|---|
| thread-summary batch | 115 時間 | 650 × 421 秒 × 1.5 |
| thread-group batch | 121 時間 | メンバー要約更新（件数の前提は thread-summary batch と同じで 115 時間）+ 集約 (50 × 120 秒 + 300 × 21 秒) × 1.5 ≈ 6 時間 |
| daily batch | 11 時間 | 37 日（月次段）× (8 + 1) × 79 秒 × 1.5 |
| weekly batch | 11 時間 | 6 週（月次段）× (15 + 1) × 254 秒 × 1.5 |
| monthly batch | 7 時間 | 1 か月 × (25 + 1) × 600 秒 × 1.5 |
| personality / reflection batch（定期実行） | 19 / 27 時間 | 200 × 224・318 秒 × 1.5 |
| periodic の import | 各 2 時間 | 既存値 |
| thread-issue-resolution / group 照合（文書直下） | 24 時間 | 既存値。job 期限は 25 時間 |
| グループ要約 single からの group 照合の投入（`invokeCrossThreadResolution`、`await: false`） | 24 時間 | 待たないため、この値は投入した job の期限になる。照合文書と同じ値 |

ThreadGroup のメンバー要約の更新は、メンバーのスレッドごとに thread-summary single を1件ずつ直列に呼ぶ。
子 batch を1回で呼ぶと、その進捗は親のストリームに流れない。そのため1件ずつ呼び、各件の前に
`reportProgress` で件数を出す（import のヘッダに `(n/m) メンバー要約を更新中` として表示される）。
1件ずつの呼び出しなので、run/call の既定の 1 時間が適用される。直前段のスレッド要約はインポート範囲だけを
対象とするため、範囲外のメンバーが未要約または古い場合はここで生成され、段全体では 1 時間を超えうる。
前提を超える範囲（例: 複数か月分の月次）は失敗表示になるため、範囲を分けて実行する。
月次は実行後に実測で見直す。値は `src-tauri/src/data/paths.rs` の構造テストで固定している。

## タイムゾーン境界

daily / weekly / monthly work-summary と periodic import は、worker プロセスの
`TZ` env が設定されている場合は jq の `localtime` / `strflocaltime` を使って
DST-aware な日付境界を計算する。`TZ` が未設定の場合だけ、
`timezone_offset_hours` の固定オフセット fallback を使う。

Lookback は Settings の Timezone 選択、`TZ` env、OS timezone、`Asia/Tokyo`
fallback の順で sidecar 起動時に `TZ` を解決して jobworkerp に渡す。Timezone
の変更は worker 起動時 env の変更なので、保存時に sidecar restart が必要。
Connection で jobworkerp が Remote server の場合、このローカル `TZ` はリモート jobworkerp へ
注入されないため、UI では Timezone 変更を無効化する（リモート側で `TZ` を設定する）。
YAML 側では「昨日」「週初」「月初」などの calendar date を先に求め、各日の
00:00 を改めて UTC epoch に変換する。DST 日は 23h / 25h の日があるため、
`day_start + 86400` のような固定秒加算で終了境界を作らない。
一部の実在タイムゾーンでは DST により local 00:00 が存在しない日がある。
その場合、補正後の候補 epoch が対象 calendar date 上にあるなら、その日の
最初の実在時刻として採用し、再補正で前日に戻さない。

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
`memories-<feature>-single-<lang>`（`memories-thread-group-summary-single-<lang>` を含む）/
`memories-user-personality-merge-<lang>` を worker 名で呼び分ける。single / merge YAML の絶対パスは runtime に保存しない。
言語別 worker は起動時に `workers/lang-workers` から登録されるため、bundle 位置が
変わっても scheduler の保存済み runtime が陳腐化しない。
`runtime.memories_import_bin` は `regular` タスクの import 前段で使う
`memories-import` の解決済みパス。旧 scheduler は未指定でも読み込めるが、起動時の
runtime refresh で現在のパスに再保存される。

`regular` タスクの `task.source` / `task.sources` は import 元を表す。
`codex+claude-code` は `codex` と `claude-code` の両方に展開され、wrapper 内の
`runImportCodex` / `runImportClaudeCode` が必要な方だけ実行される。その後、同じ
source filter を使って thread summary / daily summary / personality / reflection を
実行する。ユーザー操作による import は Tauri の `start_import` が同じ batch 群を直接
dispatch し、定期実行は `lookback-periodic-run.yaml` が担当する。

`regular` タスクの `lookback_days` は `0` 以上を指定できる。`0` は import の開始日と
日次要約の対象日を実行時点の当日にする。`1` 以上では import を指定日数前の0時から
開始し、日次要約は従来どおり前日を終点とする直近日数を対象にする。

thread summary と ThreadGroup summary は、最終メッセージが `lookback_days + 1` 日前
以降のスレッドだけを対象にする(`buildPeriodicActivityWindow` で一度だけ求める)。
1日余分に取るのは、どのタイムゾーンでも import のローカル0時開始を含めるため。
期間外の未要約スレッドは定期実行では拾わないので、要約タブで per-thread を期間指定して
生成する。

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
   `regular` は選択済みの Codex / Claude Code ログルートを source ごとに確認してから、
   存在する source だけを import し、thread summary と optional な daily /
   personality / reflection を呼ぶ。欠損したログルートは workflow 出力の
   `skipped_import_sources` に記録し、後続ステージは継続する。`weekly` / `monthly` は既存の daily / weekly
   summary を集約する。各 batch は言語別 single worker を worker 名で解決する。

## LLM モデルの集中管理

memories リポジトリ版の workflow YAML は `runnerName: LLM` + inline
`settings.ollama` で Ollama を直接呼び出す構成だが、agent-app では
**`workerName: memories-llm` 参照** に統一している。モデル切替は
`workers/llm-workers.yaml` の `LOOKBACK_LLM_MODEL` / `LOOKBACK_LLM_HF_REPO`
/ `LOOKBACK_LLM_CTX_SIZE` / `LOOKBACK_LLM_KV_CACHE_TYPE` /
`LOOKBACK_LLM_MTP_ENABLED` / `LOOKBACK_LLM_MTP_N_MAX` /
`LOOKBACK_LLM_MTP_DRAFT_MODEL` env で行い、
各 workflow YAML を触る必要がない。

既定モデル: 非MTPの Gemma 4 E2B IT QAT (Unsloth GGUF, Q4_K_XL)。
- `LOOKBACK_LLM_MODEL=gemma-4-E2B-it-qat-UD-Q4_K_XL.gguf`
- `LOOKBACK_LLM_HF_REPO=unsloth/gemma-4-E2B-it-qat-GGUF`
- `LOOKBACK_LLM_CTX_SIZE=131072`
- `LOOKBACK_LLM_KV_CACHE_TYPE=KV_CACHE_TYPE_Q4_0`
- `LOOKBACK_LLM_MTP_ENABLED=false`
- `LOOKBACK_LLM_MTP_N_MAX=2`
- `LOOKBACK_LLM_MTP_DRAFT_MODEL=null`

thread-reflection の既定入力上限は `context_limit_tokens=110000`。reflection の
LLM 呼び出しは `max_tokens=20000` を確保するため、既定の 131072 context 内に
収まるよう余白を持たせている。

MTP 対応 preset では `mtp.enabled=true` を `LlamaRunnerSettings` に渡す。
`n_max` は plugin 既定値の 3 ではなく、長文の要約・自省処理で安定しやすい
2 をアプリ既定にする。
Gemma 4 QAT MTP preset は QAT の target GGUF を `model` に指定したまま、同じ HF repo の
draft GGUF を `draft_model` に指定する。たとえば 12B では
`gemma-4-12B-it-qat-UD-Q4_K_XL.gguf` が本体で、`mtp-gemma-4-12b-it.gguf` は
投機推論用の draft model である。Qwen MTP preset は same-file MTP のため
`LOOKBACK_LLM_MTP_DRAFT_MODEL=null` を注入し、`draft_model` を未指定にする。

注意: MTP はチャットのような短い対話では高速化しやすいが、Lookback の要約
ワークロードでは長い prefill / 長文生成の比率が高く、draft 側の追加計算が
効いて多くの場合は遅くなる。要約性能を重視する検証では、同じ preset を
`LOOKBACK_LLM_MTP_ENABLED=false` でも比較する。

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

### 要約生成での thinking 無効化

thread / ThreadGroup / daily / weekly / monthly の要約生成は JSON Schema で出力を
制約するため、LLM 呼び出しに `extract_reasoning_content: false` を指定して thinking を
要求しない。thinking モデルは制約付き生成の中で推論を書けず、空欄のスキーマ骨格を
返すことがあるためである。この指定をモデル・サーバーごとの無効化方法に変換するのは
jobworkerp の LLM runner の責務であり、genai の adapter ごとに送り方が異なる。

外部 LLM では、実際のバックエンドに合う genai adapter を設定画面のモデル名の
名前空間で指定する。たとえば oMLX サーバーは `reasoning_effort` を無視するので、
`openai::<model>` ではなく `omlx::<model>` を指定する（omlx adapter は
`chat_template_kwargs.enable_thinking: false` を送る）。base URL を設定していても、
名前空間を明示したモデルの API キーはその adapter が読む環境変数
（`omlx::` なら `OMLX_API_KEY`）に渡す。

## reflection ステージ

ユーザー操作による import では Tauri の `start_import` が、importer の構造化 event から
確定した対象 thread ID を `thread-reflection-batch` へ直接渡す。Reflection を選択した
場合にだけ dispatch し、sidecar には `MEMORY_REFLECTION_DISPATCH_ENABLED=true` を渡す。
prompt version はアプリ側の既定値 `v1` を batch 入力へ供給する。定期実行では
`lookback-periodic-run.yaml` が同じ batch を呼び出す。
