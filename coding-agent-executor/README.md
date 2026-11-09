# Coding Agent Executor

本書は現在のhelper実装を説明する。coding agentのすべての実行・公開にこのcrateやsealed artifactを必須とする仕様ではない。自動Issue処理の公開は設定に従ったworkflow工程で行える。現在のhelper呼出し・起動gateはコードに残っており、仕様修正だけで撤去・有効化されたとは扱わない。

このcrateは、信頼済みhostから呼び出すprotocol v2のworkspace準備・成果公開executorです。CLIは`--config FILE --stdio`でJSON Linesを受け取り、各行に対してJSON応答を返します。成果は`Prepare(attempt_id)`→`Capture`→`Seal`→`Resolve`の順にhost側で確定し、Push/PRにはresolved artifact digestとexecutorが生成したcommitを指定します。PR作成には、先行する同一runの成功済みpush receiptが必要です。

## CAPTURE・SEALとbase treeの制約

`InspectSource`はPrepare済みworkspaceを読み取り、承認baseからのcommit済み・未commitのGit diffとuntracked file bytesを返します。`.coding-agent`の報告はソース差分から除外します。新規symlinkは参照先を読まずlink targetを返し、HEAD・index・worktreeを変更しません。run/attempt/base/branchの不一致や上限超過では結果を返しません。この工程はseal・VM停止証明を要求しません。返却値の取得だけで意味判定・検証・公開が完了したことにはなりません。

`PushWorkspace`は通常workspaceのcommitを公開する追加操作です。Prepare要求、承認したcommit、実`SourceInspection`のJSON bytesから計算したSHA-256を指定します。sourceのHEAD/branch/clean状態を照合し、object dataのみをprivate Git領域へ容量制限付きで取り込み、remote照会後にもsourceを再照合します。Capture/Seal receiptは要求しません。既存branchが同じcommitなら照合結果を返し、別commitなら拒否します。新規branchのpushはremote refの期待不存在条件を使い、並行作成された既存branchも更新しません。receiptの既存`artifact_digest`欄にはsource digestを保存しますが、sealされたことを示す値ではありません。旧helperの`push`対応だけでは利用できず、固有capability `push_workspace`が必要です。

`LookupPush`と`LookupPullRequest`は公開結果をremoteから確認する操作です。固有capability `lookup_publication`が必要です。元の公開要求と同じrun/attempt/repository/branch/base/commitを渡し、pushやPR作成を再実行しません。`LookupPush`は既存のprivate publisherから確認し、workspaceの後続変更を公開しません。remote branchが存在しない/異なるcommitなら拒否します。PRはpush記録と既存PRのrun marker・head/base・本文を照合し、見つからなくてもPOSTしません。確認できた結果はローカルの復旧記録へ保存します。receiptの`artifact_digest`は元の要求値を引き継ぐ値であり、lookupがsource bytesを再測定した証拠ではありません。呼出し側は元の耐久公開intentとの束縛を維持します。

Prepareは承認済みbase commitをhost管理Gitで検査し、追跡対象のleaf entryが通常blobの`100644`または`100755`だけであることを確認してからcheckoutします。symlink（`120000`）、gitlink（`160000`）、その他のmodeを含むbaseは`unsupported_base_tree`で拒否し、flattenや暗黙の変換は行いません。

Captureは`.git`とrootの`.coding-agent`を除くworkspaceの通常ファイルをhost専用領域へ複製し、untracked・ignoredファイルも対象にします。symlink、hardlink、special file、上限を超えるsnapshotはfail closedです。Sealは複製済みsnapshotだけからhost管理Git objectを生成し、Git storageをfsyncしてから耐久receiptを保存します。以後のResolve/Push/PRはreceiptとそのartifact digest、host生成commitのみを使い、workspaceまたはguestの`.git` object storeをpublicationの入力にしません。

`host_isolated_after_seal`は、seal済みartifactがhost-private storageに固定され、その後workspace変更から独立していることを表します。これはguest writerの停止・quiescenceの証明ではなく、guest mountやcredentialの隔離状態も証明しません。成果と評価対象・公開対象の対応を確認する必要がありますが、通常CLI実行の完了にVM停止・writer停止proofを追加要求しません。

## 中断したattemptと再試行

CAPTURE中にプロセスが中断し、`capture-data`が存在する一方で`capture.json`がない場合、同じattemptの再実行はfail closedになります。現在、この状態から同じattemptを自動回復する機能はありません。同期的に検出されたcapture失敗では、一時的な`.capture-stage`がcleanupされることがありますが、これはfinalized artifactの回復・削除ではありません。呼出し側はCAPTURE/SEALおよびnative処理の失敗診断と残存するartifact/receipt状態を保持してください。再試行のためにartifactを自動削除したり、workspace/Git metadataからmutableな代替入力へfallbackしたりしません。この実装を完全なcrash-idempotent recoveryとして扱わないでください。

新しいattemptを使えるのは、人の明示承認があり、前のexecutionがterminalであると確認でき、かつnative coordinatorが新attemptを許可した場合に限ります。その場合もfresh workspaceを用い、これは自動agent rerunではありません。writerの状態がunknownのままなら、新attemptを作ってもその不確実性は解消されず、成功扱いにしてはいけません。

一方、耐久seal receiptが残っている後続Push/PRのlookup・再試行は、同じattemptとartifactを指定して継続できます。protocol v2でreceiptの有無や状態がunknownなら、host operatorは再試行を強制するためにartifactやreceiptを手動削除せず、診断用状態を保持してください。

## Push対象

push前に、host管理cloneで承認済みbase commitとresolved artifact commitのtree IDを比較します。treeが同じなら`no_changes_to_publish`を返し、run branchをremoteへ作成しません。empty commitや変更後に元treeへ戻したcommitも同じ扱いです。tree差分がある成果は通常のcommit検証・secret scanを経て公開できます。

## Pull Requestの照合と再試行

PR一覧filterは検索の最適化であり、forge応答を信頼する根拠ではありません。返された対象候補は、repository owner、runのcanonical branch、full head commit、base repository/branch、およびrun markerを個別に照合します。重複・不一致の候補は`pull_request_conflict`として拒否します。openまたはclosedの一致済みrun PRは既存結果として返し、再作成・再openしません。

- GitHubには`state=all`、`head`、`base`、`per_page`、`page`を指定します。
- Giteaには`state=all`、`base_branch`、`limit`、`page`を指定します。GitHub専用の`head`/`base` filterは送りません。
- `Link`のnextは同一origin/path、連続page、元filterの維持を検証します。nextがないfull pageはpage/limitで次を確認します。循環・不正・filter欠落・重複page結果、部分/切詰め応答、100 pageまたは16 MiBの総走査上限超過はfail closedとなり、PR POSTへ進みません。各応答の上限は1 MiBです。
- PR POST前にrun単位のpending receiptを耐久保存します。応答喪失時は同一payloadの再呼出しで全ページを再照合し、一意に一致するPRが見つかった場合だけreceiptを確定します。pendingのまま一致を確認できない場合はlookup-onlyを維持し、同じrunから二度目のPOSTを行いません。
- forgeが409またはGitHubの重複作成時に使う422を返した場合も、全ページを一度再照合します。一意な一致が見つかれば既存receiptを返し、見つからなければ`pull_request_conflict`で停止します。自動で二度目のPOSTは行いません。照合不能な通信・paginationエラーもPOSTせず、機密値を含まない固定診断を返します。

## PR本文のclose directive

外部Issueとの明示的な関連付けとclose承認を表すprotocol/config項目はありません。そのためPR titleまたはbodyで、`Close`/`Closes`/`Closed`、`Fix`/`Fixes`/`Fixed`、`Resolve`/`Resolves`/`Resolved`に続く`#number`、`owner/repo#number`、URL、Markdown link、autolinkなど外部Issue参照の可能性がある形式を`invalid_request`で拒否します。keywordは大小文字を区別せず、colon・空白・改行を挟む形式も対象です。参照先hostによる許可判定はせず、code fenceも例外にしません。閉じるkeywordのない通常のIssue参照や「we fixed parser」のような通常文は拒否しません。executorはPR mergeや外部・内部Issueのcloseを行いません。
