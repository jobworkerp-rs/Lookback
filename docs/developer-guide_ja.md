# 開発者ガイド

このガイドには Lookback の開発者向けセットアップ、起動、テスト、環境変数の詳細をまとめています。利用者向けの概要と初回利用チュートリアルは [../README_ja.md](../README_ja.md) を参照してください。

## ディレクトリ構成

```text
src/                 React UI、hooks、API ラッパー、Vitest テスト
src-tauri/           Tauri アプリ、Rust コマンド、バックエンドプロセスのライフサイクル、gRPC クライアント
workers/             Lookback 用 worker / workflow YAML バンドル
proto/               Rust gRPC client が使う vendored protobuf（jobworkerp、llm_memory、conductor、issue_tracker）
dict/                memory-store 用に配置する任意の検索用辞書
docs/                公開用の開発者ドキュメント
```

worker / workflow バンドルの設計は [../workers/README.md](../workers/README.md) を参照してください。

## 依存関係のインストール

```bash
pnpm install
```

## フロントエンド開発

```bash
pnpm dev
pnpm test
pnpm build
pnpm lint
```

## Rust の検査

```bash
cargo build -p lookback-tauri
cargo clippy -p lookback-tauri --all-targets -- -D warnings
cargo test -p lookback-tauri -- --test-threads=1
```

Rust テストにはバックエンド相当のポートやデータディレクトリを共有するものがあるため、テストスレッド数を 1 にしています。

## デスクトップアプリ全体の起動

1. 次のリリースバイナリをビルドまたは入手します。
   - [`jobworkerp`](https://github.com/jobworkerp-rs/jobworkerp-rs) の `all-in-one`
   - [`memory-store`](https://github.com/jobworkerp-rs/memory-store) の `front`
   - [`jobworkerp-conductor`](https://github.com/jobworkerp-rs/jobworkerp-conductor) の `conductor-main`
   - [`memory-store`](https://github.com/jobworkerp-rs/memory-store) の `memories-import`
   - [リリースビルドの詳細](#リリースビルドの詳細)に記載する公開 source pin の `issue-tracker-server`
   - runtime `protoc`: staging スクリプトが固定版の自己完結バイナリを app 用に自動取得します。
     `PROTOC` で development staging の入力を上書きできます。この runtime binary は release build の
     preflight が `PATH` 上に要求する host compiler とは別物です。`PROTOC` を指定しても host `protoc`
     の check は満たされず、host `jq` も必要です。
2. 必要な jobworkerp ランナープラグインを対象 OS 向け共有ライブラリとしてビルドし、プラグインディレクトリに配置します。
   - [`llama-cpp-runner`](https://github.com/jobworkerp-rs/llama-cpp-runner): ローカル LLM 実行用
   - [`mm-embedding-runner`](https://github.com/jobworkerp-rs/mm-embedding-runner): 埋め込み生成用

   ローカル LLM 実行は Qwen 3.5/3.6 系と Gemma 4 系のモデルのみに対応しています。Lookback は
   チャット前に要約を行うため、既定の Local preset は非MTPの Gemma 4 E2B IT QAT です。MTP preset は
   チャット重視の用途向けに利用できます。Gemma 4 MTP preset では QAT 本体 GGUF を `model` に残し、
   `LlamaRunnerSettings.mtp.draft_model` に draft GGUF を指定します。

   現行の macOS バンドルパスは `.dylib` ファイルを使います。Linux ビルドでは対応する共有ライブラリ拡張子とリソースマッピングを使ってください。
3. `memory-store` の `front` バイナリを Lindera FTS 対応でビルドする場合は、互換 lindera 3.x
   形式の IPADIC 検索用辞書を `dict/lindera/ipadic` に配置します。別のディレクトリを使う場合は
   `LOOKBACK_LINDERA_SRC` を設定します。Lookback はこのファイル群をデータディレクトリへ配置し、
   起動ごとに `lance_language_models/lindera/ipadic/config.yml` を生成して、`memory-store` 向けに
   `LANCE_LANGUAGE_MODEL_HOME` を設定します。辞書本体は直接読み込みません。
4. リポジトリ内の予備パスからバイナリを解決できない場合は、明示的にパスを指定して起動します。

```bash
LOOKBACK_JOBWORKERP_BIN=/path/to/all-in-one \
LOOKBACK_MEMORIES_BIN=/path/to/front \
LOOKBACK_CONDUCTOR_BIN=/path/to/conductor-main \
LOOKBACK_MEMORIES_IMPORT_BIN=/path/to/memories-import \
LOOKBACK_ISSUE_TRACKER_SERVER_BIN=/path/to/issue-tracker-server \
PROTOC=/path/to/protoc \
LOOKBACK_PLUGINS_SRC=/path/to/plugins \
pnpm tauri:dev
```

`pnpm tauri:dev` と `pnpm tauri dev` は、起動前に `scripts/stage-dev-external-bins.sh` を実行し、Tauri が検証する `src-tauri/bin/<name>-<target-triple>` を実バイナリで配置します。配置元の解決順序はアプリ実行時と同じく、環境変数、`PATH`、workspace 相対の予備パスです。たとえば Linux x86_64 では `src-tauri/bin/all-in-one-x86_64-unknown-linux-gnu` が必要です。MCP を有効にした場合、sidecar は `MCP_ENABLED=true` で起動します。必要な jobworkerp revision で `cargo build --release -p jobworkerp-main --bin all-in-one` を実行するか、互換バイナリを `LOOKBACK_JOBWORKERP_BIN` に指定してください。

Linux x86_64 では同じ staging が `coding-agent-executor` を debug build し、開発用 coding-agent パッケージ（executor、配置済みの `all-in-one`、作業ツリーの dirty 状態をそのまま記録した manifest）を dev 時の `resource_dir()` である `target/debug/coding-agent/` に配置します。debug build のアプリだけがこのパッケージを受け付け、設定画面の診断には開発用パッケージと表示されます。release bundle には含まれません。要件と上書き変数は [docs/coding-agent-executor-install.md](coding-agent-executor-install.md) の "Development staging" を参照してください。

### Experimental Issues ビルド

Issue 候補の抽出は debug ビルドで有効です。通常の release ビルドでは Issues UI を非表示にし、利用者が以前に有効なポリシーを保存していても抽出を強制的に無効化します。`issue-tracker-server` も同梱しません。experimental として明示的に有効化した release を作成するには、release スクリプトに `--experimental-issues` を指定します。

```bash
scripts/build-release.sh --profile linux-cuda --experimental-issues
```

このフラグは次の 3 つを必ず同時に有効にします。`issue-tracker-server` のビルドと同梱、アプリの Cargo feature `experimental-issues`、Issue Tracker の E2E ゲート（`scripts/test-issue-tracker-grpc-e2e.sh`）。feature を有効にしたのに server を同梱しないビルドは起動時に失敗するので、`pnpm tauri build` に `--features experimental-issues` だけを付けないでください。手動の `pnpm tauri build` は、開発用に `issue-tracker-server` を含めている `src-tauri/tauri.conf.json` の `externalBin` をそのまま使うので、常に server を同梱します。通常の release は `build-release.sh` で作ってください。

`pnpm tauri build`（`pnpm tauri:build` を含む）は開発用 staging や migration bundle の生成・再利用を行いません。package 前に、同じ pinned Memories build から得た target-specific `front`、`memories-import`、migration bundle が配置され、各 artifact digest が provenance manifest と一致する必要があります。wrapper と Tauri の build/bundle hook は provenance 欠損や不一致を拒否します。pinned source からの build/staging には `scripts/build-release.sh` を使ってください。手動 package の場合は clean な pinned source と release pin を `scripts/lib/write-migration-source-provenance.mjs` に渡し、`pnpm verify:packaging-migration` を通してから build します。`pnpm tauri:dev` は起動のたびに隣接する `../memories` の `scripts/build-memories-db-migrate-sqlite.sh` で migration bundle を生成し、`bundle-manifest.json` の content digest がステージ済みの bundle と異なる場合だけ差し替えます。memories の作業ツリーを変更すれば、次の `pnpm tauri:dev` で反映されます。

開発用 staging の解決予定を、書き込みやビルドをせず確認するには次を実行します。

```bash
scripts/stage-dev-external-bins.sh --dry-run
```

同じ staging で、Tauri が検証する `plugins/*.so*` / `*.dylib` も満たします。この glob は `src-tauri` 基準なので、bundle 用の staging 先は `agent-app/src-tauri/plugins/` です。Linux は `plugins/*.so*` で通常の plugin `.so` と、CUDA runtime の `libcudart.so.12` や SONAME symlink のような versioned 共有ライブラリをまとめて含めます。`LOOKBACK_PLUGINS_SRC` が指定されていればその配下を再帰的に探し、未指定の場合は従来の workspace 配置である `agent-app/../../plugins/cuda_runner/` と `agent-app/../../plugins/` から共有ライブラリを探して、`agent-app/src-tauri/plugins/` へコピーします。standalone の `issue-tracker-server` は target-triple 付き external binary として別に配置します。`LOOKBACK_ISSUE_TRACKER_SERVER_BIN` を指定するとそのバイナリを使い、未指定時は sibling の `issue-tracker-server` checkout で `issue-tracker-front --bin issue-tracker-server` をビルドします。`LOOKBACK_ISSUE_TRACKER_SERVER_DIR` で checkout を上書きでき、旧 `LOOKBACK_ISSUE_TRACKER_RUNNER_DIR` も互換用に受け付けます。loopback listener、runtime 設定、health check、private data root は Lookback が管理し、この server は jobworkerp worker、plugin、FunctionSet、MCP tool として公開しません。dev/release staging は `agent-app/../plugins/` には書き込みません。

実行時は Lookback が配置した共有ライブラリ名と digest を runtime plugin directory の manifest に記録します。次回 staging で source から消えたライブラリは、この manifest に記録されたものだけを削除します。manifest が欠損または破損している場合は削除せず再作成するため、利用者が配置した共有ライブラリは保持されます。

macOS で動いて Linux で `resource path bin/all-in-one-x86_64-unknown-linux-gnu doesn't exist` になる場合、macOS 側には `src-tauri/bin/*-aarch64-apple-darwin` または `*-x86_64-apple-darwin` が既に残っていて、Linux 用の `*-x86_64-unknown-linux-gnu` が未配置であることが原因です。`scripts/build-release.sh` は release build の中でこの配置を行いますが、通常の dev 起動では release script を通らないため、dev 用 staging が必要です。

Linux の `pnpm tauri:dev` は、Tauri が Rust crate をビルドしている間も Vite 開発サーバーを同時に起動します。Vite は `src-tauri/` と `target/` を監視対象から除外しているため、Cargo が `target/debug/build/*/rustc*` のような一時ディレクトリを作成・削除しても、Vite の file watcher はそのディレクトリを走査しません。

Linux の開発時・パッケージ版の起動では、Lookback は既定で `GDK_BACKEND=wayland,x11` と `WEBKIT_DISABLE_DMABUF_RENDERER=1` を設定します。GTK はネイティブ Wayland を先に試し、利用できない場合は X11 へフォールバックします。AppImage の linuxdeploy の GTK hook は、固定している Tauri CLI（2.12.4）ではバックエンドを設定しないので同じ動作になります。X11 を強制する古い CLI の hook は、`scripts/build-release.sh` が同じ規則に書き換えます。deb / rpm では GTK 初期化前にアプリケーションが設定します。明示的な値が優先されるため、X11 を強制するには `GDK_BACKEND=x11`、DMA-BUF renderer 有効のネイティブ Wayland を試すには `GDK_BACKEND=wayland WEBKIT_DISABLE_DMABUF_RENDERER=0 pnpm tauri dev` のように指定します。

`Unknown system error -116` の `scandir .../target/debug/build/.../rustc*` で停止する場合の確認手順:

1. `vite.config.ts` の `server.watch.ignored` に `**/target/**` が含まれていることを確認します。
2. 古い Vite 開発サーバーが残っている場合は停止し、`pnpm tauri:dev` を起動し直します。
3. それでも再発する場合は、`target/` がリポジトリ外の別パスに変更されていないか、`CARGO_TARGET_DIR` の値を確認します。

### Linux AppImage の初期設定画面

AppImage 版で初期設定ウィザードの保存先選択が進まない場合は、次の順で確認します。

1. AppImage をターミナルから起動し、`dialog`、`portal`、`permission`、`validate_data_root` を含むエラーが出ていないか確認します。
2. `選択…` でディレクトリ選択ダイアログが開かない場合は、パスを入力欄へ直接入力します。UI はダイアログ起動失敗を表示し、手入力の validation は継続します。
3. デスクトップ環境に `xdg-desktop-portal` と GTK portal backend（Debian/Ubuntu なら `xdg-desktop-portal-gtk`）が入っているか確認します。Tauri の Linux ネイティブダイアログは環境によって portal に依存します。
4. `次へ` が無効のままの場合は、入力したパスが絶対パスで、存在する書き込み可能ディレクトリか、親ディレクトリが書き込み可能な新規作成可能パスかを確認します。

これらの変数を省略した場合、Lookback は次の順でバイナリを解決します。

1. 環境変数による上書き。
2. パッケージ済み実行ファイルの隣にある Tauri `externalBin`。
3. `PATH`。
4. ローカル開発用のワークスペース相対の予備パス。

## リリースビルドの詳細

`scripts/build-release.sh --profile mac`（または `--profile linux-cuda`）は、依存 repository の clone・
GPU feature 付きビルド・バイナリ/プラグイン/lindera の配置・Tauri パッケージングまでを自動化
します。前提条件とフラグはルート README「ソースからビルド」を参照してください。以下の手動手順は
スクリプトが行う内容を記述したもので、部分ビルドやカスタムビルドの参考用です。

pipeline の互換用論理名 `memory-store` は、非互換の `memory-store/main` ではなく、次の公開 GitHub tag と解決済み commit を指します。

| Build input | 公開 repository | Tag | peeled commit |
| --- | --- | --- | --- |
| `memory-store`（Memories） | [`jobworkerp-rs/memory-store`](https://github.com/jobworkerp-rs/memory-store) | `v0.0.24` | `8d365fc792489a98035bb11aaa3fe7e040a1177f` |
| `issue-tracker-server` | [`sutr-app/issue-tracker-server`](https://github.com/sutr-app/issue-tracker-server) | `v0.0.1` | `5974330a31ae19fb04db99841be73f305958d0d8` |

release は同じ pinned Memories checkout から `front`、`memories-import`、`memories-db-migrate` をビルドします。ビルド前に選択した ref が固定 full revision を含むことと、release に必要な入力が揃っていることを検査します。移行 task や schema の内容は Lookback では検査せず、Memories の build script が bundle 生成時に `memories-db-migrate bundle verify` で検査します。

全 platform の release-build preflight は host `protoc` と `jq` が `PATH` 上にあることを要求します。app に同梱する runtime `protoc` は別途 download されるため、`PROTOC=/path/to/protoc` だけでは host compiler の check を満たしません。古い Gitea source の clone が `.build-deps/` に残っている場合も、build は origin を自動変更しません。公開 source 用の新しい clean build root を使ってください（macOS の例: `scripts/build-release.sh --profile mac --workdir "$HOME/.cache/lookback-public-deps"`）。

SQLite migration bundle の provenance（format version 3）には、credential を除いた Git origin/ref/full revision、生成時の `bundle-manifest.json` の content digest、各成果物と task catalog/history・最新 migration の digest を記録します。provenance の追加や macOS の署名で bundle の内容が変わるため、package 時の検証は `bundle verify` ではなく、この provenance と Atlas lock との一致で行います。`memory-store` という `--only` / build-cache 名は互換性のため維持しています。

カスタム source の環境変数は次のとおりです。

| Variable | 用途 |
| --- | --- |
| `LOOKBACK_MEMORY_STORE_REPO_URL` | clone する Memories remote URL。上書き時は ref と revision も明示します。URL に token/password を埋め込まず、Git credential helper を使います。 |
| `LOOKBACK_MEMORY_STORE_REF` | release source ref（branch）。remote override と local source の両方で指定します。 |
| `LOOKBACK_MEMORY_STORE_REVISION` | 期待する full 40 桁 Git revision。選択 ref から到達可能で、checkout の HEAD と一致する必要があります。 |
| `LOOKBACK_MEMORY_STORE_SOURCE_DIR` | clone せずに使う既存 local Git checkout の root。`REF` と `REVISION` も必須です。source の origin と commit provenance を検査します。 |
| `LOOKBACK_ISSUE_TRACKER_SERVER_REPO_URL` | Issue Tracker server の remote URL 上書き。`REF` と `REVISION` も明示します。URL に token/password を埋め込まず、Git credential helper を使います。 |
| `LOOKBACK_ISSUE_TRACKER_SERVER_REF` | Issue Tracker server の reviewed source ref。 |
| `LOOKBACK_ISSUE_TRACKER_SERVER_REVISION` | Issue Tracker server の期待する full 40 桁 Git revision。selected ref と checkout の HEAD に一致する必要があります。 |

remote URL override の例:

```bash
LOOKBACK_MEMORY_STORE_REPO_URL=https://git.example.invalid/team/LLM-Memories.git \
LOOKBACK_MEMORY_STORE_REF=reviewed-release \
LOOKBACK_MEMORY_STORE_REVISION=0123456789abcdef0123456789abcdef01234567 \
scripts/build-release.sh --profile mac
```

local checkout の例:

```bash
LOOKBACK_MEMORY_STORE_SOURCE_DIR=/work/LLM-Memories \
LOOKBACK_MEMORY_STORE_REF=reviewed-release \
LOOKBACK_MEMORY_STORE_REVISION=0123456789abcdef0123456789abcdef01234567 \
scripts/build-release.sh --profile mac
```

custom source は `memories-db-migrate` の `local apply` / `local restore` / `bundle manifest` / `bundle verify` を提供する Memories revision である必要があります。`--skip-clone` は pin と一致する既存 checkout のみ再利用します。

起動時の移行、移行前バックアップの設定、失敗時の操作については [データベース移行の運用](database-migration_ja.md) を参照してください。

Tauri のパッケージングを実行する前に、バックエンドバイナリとリソースを配置する必要があります。

1. `all-in-one`、`front`、`conductor-main`、`memories-import`、`issue-tracker-server`、`protoc` のリリースバイナリをビルドします。
2. [../src-tauri/tauri.conf.json](../src-tauri/tauri.conf.json) の `externalBin` ベース名と一致する名前で `src-tauri/bin/` に配置します。
3. Tauri のパッケージング用に、対象プラットフォームに対応したプラットフォームトリプルのサフィックス付きバイナリも用意します。レビュー済みの jobworkerp revision を使う場合は `LOOKBACK_JOBWORKERP_REF` を指定します。
4. [`llama-cpp-runner`](https://github.com/jobworkerp-rs/llama-cpp-runner) と [`mm-embedding-runner`](https://github.com/jobworkerp-rs/mm-embedding-runner) のランナープラグイン共有ライブラリを対象 OS 向けにビルドし、`src-tauri/plugins/` に配置します。`issue-tracker-front` crate から `issue-tracker-server` をビルドし、target-triple サフィックス付きで `src-tauri/bin/` に配置します。`--experimental-issues` を指定して `--only` で Tauri packaging を行う場合は、`issue-tracker-server` を含める必要があります。部分的な backend build だけを行う場合は `--skip-frontend` を指定します。
5. パッケージに含める `memory-store` の `front` ビルドが Lindera FTS を必要とする場合は、
   `dict/lindera/ipadic` に IPADIC 検索用辞書を配置します。`dict/` 配下は **git 管理外** です。
   `scripts/build-release.sh --lindera-only` で lindera 3.0.7 release 辞書を取得すると、IPADIC
   の `COPYING` ライセンスも辞書と一緒に配置されます。`config.yml` は起動時にデータディレクトリ側へ
   生成されます。`pnpm tauri:dev` で形態素解析 FTS を使いたい場合にも事前生成が有効です
   （未生成時は sidecar が ngram トークナイザにフォールバックします）。
6. `pnpm tauri:build` を実行します。

`LOOKBACK_RELEASE_VERSION` が設定されている場合、`scripts/build-release.sh` は Tauri packaging の前に
そのタグを `src-tauri/tauri.conf.json` へ反映します。通常の `v` prefix は取り除くため、
`v0.0.3` のタグでは、checkout 済み Tauri config の開発用既定値ではなく `0.0.3` の bundle asset が
生成されます。

### Linux AppImage の後処理

`scripts/build-release.sh` は Linux AppImage 生成後に AppImage を一度展開し、linuxdeploy が生成した
GTK runtime hook を補正してから再梱包します。この hook を作る Tauri CLI の GTK plugin は、ビルドするホストに
`gtk-query-immodules-3.0` と `gdk-pixbuf-query-loaders` を必要とします。無い場合は GTK module の cache が
作られず、AppImage の作成が失敗します。linuxdeploy の標準 hook は
`GTK_IM_MODULE_FILE` を AppImage 内の `immodules.cache` に固定しますが、この cache にはホスト側の
fcitx/ibus module が含まれないため、日本語入力が XIM 経由に落ちて WebKitGTK のテキスト入力欄で
フリーズする環境があります。

この後処理では、ホスト側に fcitx/ibus を含む GTK input method cache がある場合に
`GTK_IM_MODULE_FILE` をその cache へ向け、ホストの GTK module path も探索対象へ戻します。
`GTK_IM_MODULE` が未設定で `XMODIFIERS=@im=fcitx` / `@im=ibus` がある場合は、対応する
GTK IM module も補完します。`XMODIFIERS` が desktop launcher 経由で渡らない場合でも、
`fcitx5-remote` または実行中の `fcitx5` / `ibus-daemon` プロセスから module を推定します。
ホストの fcitx/ibus module が AppImage 同梱 GLib より新しい GLib でビルドされている環境では
module 初期化が拒否されるため、ホスト cache を使う場合だけホスト GLib 系ライブラリとその依存を
`LD_PRELOAD` で先に読ませます。該当 cache がない場合は AppImage 内の cache にフォールバックします。
GitHub Actions の Linux job は self-hosted runner `lookback-linux-cuda` で動きます。この runner では job に
root 権限が無く、FUSE もマウントできません。runner image には NCCL が入っていないため、依存 crate の
build script が NCCL を自動検出することはありません。Linux job が使うシステムツールは、すべて runner image に
入っている必要があります。必要なツールの一覧は `test` job の `Verify runner image tools` step にあり、
この step がどの job も upload する前に確認するので、runner image の依存不足は run の早い段階で失敗します。
workflow からはパッケージを install できないため、足りないパッケージは runner 管理者に image への追加を
依頼します。Linux の build job は `APPIMAGE_EXTRACT_AND_RUN=1` を設定し、linuxdeploy と `appimagetool` を
FUSE でマウントせず展開して実行させます。
この runner は共有されているため、タグ push の場合も含めて、runner 上の job には読み取り専用の
`GITHUB_TOKEN` だけを渡し、checkout でもトークンを残さず、secrets は参照しません。build した bundle は
workflow の artifact として渡し、GitHub-hosted runner で動く `publish-linux` job が GitHub Release に添付します。
CUDA plugin はホスト提供の NVIDIA driver である `libcuda.so.1` に link しますが、これは bundle
してはいけません。build container の `ldconfig` に実 driver library がない場合、
`scripts/build-release.sh` は一時的な `LD_LIBRARY_PATH` で CUDA toolkit stub を linuxdeploy にだけ
見せます。既存の AppImage 後処理は、公開前に `libcuda.so*` の混入を引き続き削除します。
CUDA AppImage job では `LOOKBACK_TAURI_VERBOSE=1` を設定し、Tauri が呼び出す linuxdeploy の stderr を
GitHub Actions log に出します。bundling に失敗した場合、`scripts/build-release.sh` は AppDir と
staged plugin の `ldd` 結果も出力します。
CUDA ビルドでは同じ展開 root で NVIDIA driver library の除去も行います。AppImage hook の変更後は
次を実行して、hook の置換と冪等性を確認します。

```bash
bash scripts/test-appimage-hooks.sh
```

### 公開せずにリリースビルドを確認する

release workflow を手動実行すると、何も公開せずに、リリースと同じようにすべての bundle を build し、macOS の
DMG を署名・公証できます。リポジトリの外の変更（`lookback-linux-cuda` の image の更新、`lookback-macos` の
host の変更、Apple の証明書や secrets の更新）の後に、次の手順で確認します。実行されるのは、公開側の
`main` にある workflow とスクリプトです。

1. 公開側の GitHub リポジトリで Actions > Release を開き、default branch で Run workflow を実行します。
   workflow に `workflow_dispatch` が入っている必要があります。workflow の変更は、リリースの同期でしか
   GitHub に反映されません。
2. この run は `test`、`build-macos`、`build-cuda`、`build` を実行し、`publish-macos` と `publish-linux` は
   スキップします。リリースのバージョンは埋め込まず、artifact も残さず、GitHub Release への upload も
   行いません。
3. `Verify runner image tools` が失敗した場合は、表示されたツールを runner 管理者に連絡します。

### GitHub Actions の macOS 署名付きリリース

公開リポジトリの `.github/workflows/release.yml` はタグ push で起動します。`test` の成功後、署名付き
macOS build と Linux CUDA build は並列実行でき、Linux CPU build は CUDA 完了後に開始します。build job は
読み取り専用の token のままで、成果物は GitHub-hosted runner で動く `publish-macos` と `publish-linux` が
リリースに添付します。
macOS job は `self-hosted`、`macOS`、`lookback-macos` label の runner を使い、host `protoc` と `jq` を
Homebrew で install します。Linux の job は、これらを runner image から使います。
lint job では、`tauri-build` の path validation を通すためだけに空の external-binary/plugin placeholder と
空の migration resource directory も作成します。これらは release artifact ではありません。

macOS では `scripts/build-release.sh --profile mac` が Tauri に unsigned `.app` の組み立てを依頼し、
検証前に Tauri が external-binary bytes を書き換えないようにします。build 後に
`scripts/finalize-macos-bundle.sh` が source/package の migration provenance、Atlas lock、実際に package された
resource hash を検証してから signing します。検証済みの nested Mach-O file と plugin を先に署名し、upstream
source hash を保持したまま署名後の migration/Atlas hash を記録した後、外側の `.app` を seal します。
DMG の作成・署名はこれらの検査後に行います。これは Tauri packaging 前の plugin/migration signing に代わる処理です。
Linux `.so` plugin と CUDA runtime staging は変更しません。workflow は従来どおり `.p12` を run ごとの keychain に
import し、keychain の signing identity と長時間 build 前の `notarytool history` 認証確認を行います。
build の step には `APPLE_SIGNING_IDENTITY` だけを渡し、最後の step で、job が失敗した場合も含めてその run の
keychain を削除します。
`release.yml` の action はすべてコミット SHA で固定し、バージョンを末尾のコメントに書いています
(`uses: owner/action@<sha> # vX.Y.Z`)。タグが書き換えられても、Apple の secrets と同じ job で動くコードは変わりません。
更新するときは `git ls-remote https://github.com/<owner>/<action> 'refs/tags/<tag>^{}'` で新しいタグの SHA を調べ、
SHA とコメントの両方を書き換えます。固定していない参照は `scripts/test-public-release-workflow.mjs` が拒否します。

workflow は finalizer が作成した DMG を `notarytool submit --wait` で送信して ticket を staple します。その後、
実際に package された resource tree に対する migration provenance、code signature、stapled ticket、Gatekeeper 判定を
確認してから artifact を upload します。finalizer の optional `--notarize` は既存の
`APPLE_NOTARY_KEYCHAIN_PROFILE` を使いますが、release workflow は従来の Apple ID / Team ID / app-specific password
Secrets を使う別の notarization step を維持します。

`.github/` (release workflow と actionlint の runner label 設定) は上流の source リポジトリで管理され、
リリースごとの同期で公開リポジトリに反映されます。公開側で直接編集しても次の同期で上書きされます。
すべての job は `if: github.server_url == 'https://github.com'` で GitHub 上でのみ実行されます。
`node scripts/test-public-release-workflow.mjs` は、この制限と release job dependencies、signing Secrets、
provenance check、artifact flow を検証します。

macOS release build では、jobworkerp は plugin の検出と依存 dylib の解決の両方で、署名済みの
`.app/Contents/Resources/plugins` を直接使います。data directory へコピーされた plugin は参照しないため、
別 Team ID で署名された旧 plugin や CI placeholder が hardened macOS sidecar に走査されません。Linux の
ユーザー管理 `.so` plugin は保持します。

公開リポジトリで署名と notarization を行う手順:

1. Apple Developer で `Developer ID Application` 証明書を作成します。
2. CSR を作成した Mac の Keychain Access で、秘密鍵付きの証明書を `.p12` として書き出します。
3. `.p12` を base64 化します。

   ```bash
   openssl base64 -A -in certificate.p12 -out certificate-base64.txt
   ```

4. GitHub Secrets に以下を登録します。

   | Secret | 用途 |
   | --- | --- |
   | `APPLE_CERTIFICATE` | base64 化した `.p12` の内容 |
   | `APPLE_CERTIFICATE_PASSWORD` | `.p12` 書き出し時のパスワード |
   | `APPLE_SIGNING_IDENTITY` | `Developer ID Application: ...` の署名 identity |
   | `APPLE_ID` | notarization に使う Apple ID |
   | `APPLE_PASSWORD` | Apple ID の app-specific password |
   | `APPLE_TEAM_ID` | Apple Developer Team ID |

5. リリースタグを push します。`test` 後に macOS と CUDA job が開始し、CPU job は CUDA 完了後に開始します。
6. macOS job が失敗した場合は `Build signed macOS bundles` と後続 verification step のログから、証明書 import、
   provenance validation、codesign、notarization のどこで止まったかを確認します。

release workflow を変更した場合は、`node scripts/test-public-release-workflow.mjs` と
`bash scripts/test-release-workflow.sh` を実行して、GitHub 限定の制限、macOS job、Linux dependency、署名用 Secrets、
事前認証チェック、DMG の Release 添付対象を検証してください。
macOS plugin / migration 署名ロジックを変更した場合は
`bash scripts/test-build-release-macos-signing.sh` で、macOS の `.dylib` と
`memories-db-migrate` と Atlas が署名対象となり、Linux では no-op になることを検証してください。
`bash scripts/test-verify-atlas-lock.sh` は upstream と署名済み Atlas の checksum guard を検証します。

### 接続先の設定とリモート接続の診断

設定画面の Connection では、jobworkerp・memories・Issue tracker・conductor のそれぞれについて
Local sidecar か Remote server かを選びます。Local にしたサービスだけをアプリが起動します。
組み合わせには次の制約があり、満たさない選択肢は理由とともに選べなくなります。

- jobworkerp を Remote にするには、memories と Issue tracker（Issue tracking が有効な場合）も
  Remote にします。リモートの jobworkerp からこのマシンの sidecar には接続できないためです。
- conductor を Remote にするには、jobworkerp も Remote にします。conductor が Local のまま
  jobworkerp だけを Remote にすることはできます（定期実行はリモートの jobworkerp に投入されます）。

旧形式の `connection.json`（`mode` が 1 つだけの形式）は、jobworkerp と memories に同じ mode を
当てはめて読み込みます。ファイルは自動では書き換えず、次に保存したときに新しい形式になります。
Issue tracker のデータと conductor の scheduler は Local と Remote で別々であり、移行しません。

保存時は URL 形式と workflow 用の host/port/tls 分解を検証します。実際の gRPC 到達確認は
Connection の `接続をテスト` を押して行います。プロキシ越しなどで jobworkerp・conductor から見た
アドレスが異なる場合は、「jobworkerp・conductor から見たアドレス」で上書きします。

#### リモート jobworkerp を使う手順

1. memories（と Issue tracker）を Remote にし、URL を入力します。Issue tracker には gateway の
   control URL と worker URL も入力します。
2. jobworkerp を Remote にして URL を入力します。定期 import を使う場合は「jobworkerp ホスト上の
   パス」に memories-import・opencode DB・作業ディレクトリのパスを入力します。
3. `接続をテスト` で到達を確認してから、画面下の「保存して適用」で保存します。
4. 「リモート jobworkerp の準備状況」で不足を確認し、`worker を登録` を押します。
5. 不足する channel・plugin runner・環境変数は、表示された設定例をリモート jobworkerp に設定し、
   再起動後に `再確認` を押します。何が必要かは `workers/README.md` の「リモート jobworkerp で
   使う」を参照してください。
6. 起動時にも準備状況を確認し、不足があればサイドバーに「リモート jobworkerp の準備が必要」と
   表示されます。アプリの更新後に「別のバージョン」と表示された場合は、手順 4 で登録し直します。
   リモートの jobworkerp に接続できない場合も起動は止めず、「リモート jobworkerp に接続できない」と
   表示します。接続設定から接続先を直してください。

jobworkerp が Remote の間は、coding agent と chat の Issue ツールは使えません。Timezone の変更も
無効になるため、リモート側の `TZ` を設定してください。

#### リモート memories の一覧や検索が空に見える場合

1. Connection の `接続をテスト` を押し、jobworkerp と memories の両方に到達できるか確認します。
2. Semantic / Hybrid 検索を使う場合は、Settings の Embedding model をリモートサーバ側の
   embedding モデル・ベクトル次元と同じ設定にします。memories が Remote の間はローカルで各記事の
   embedding は生成されず、検索クエリの embedding だけがこの設定に依存します。この変更では
   ローカル embedding インデックスのリセットや再生成は行いません。
3. 失敗した場合は画面のエラー、または `<data-root>/log/lookback.log` を確認します。接続失敗は
   `jobworkerp connection failed (<url>)` または `memories connection failed (<url>)` の形で記録されます。
4. より詳細なログが必要な場合は `LOOKBACK_RUST_LOG=debug` を付けて起動し、同じ操作を再実行します。
5. 接続テストが成功しても一覧が空の場合、接続自体は成功しています。リモート memories 側に対象データが
   存在するか、また Lookback が現在参照している `user_id` のデータかを確認してください。

### jobworkerp メンテナンスの確認

Lookback は local sidecar の jobworkerp に対して、古い `JobResult` と
`JobProcessingStatus` を約 1 週間ごとに削除します。cleanup 系の実行履歴を残さないため、
conductor scheduler や jobworkerp periodic worker は使わず、Tauri 側から gRPC API を直接呼びます。
保持期間は 7 日です。

実行内容:

1. `JobResultService.DeleteBulk` を `end_time_before = 現在時刻 - 7日` で実行します。
2. `JobProcessingStatusService.PurgeStaleJobs` を `stale_threshold_hours = 168`、
   `orphaned_only = true` で実行します。
3. `JobProcessingStatusService.Cleanup` を `retention_hours_override = 168` で実行します。

確認手順:

1. アプリを起動し、sidecar が ready になるまで待ちます。
2. `<data-root>/jobworkerp-maintenance.json` の `last_success_at_ms` が更新されているか確認します。
3. 詳細を確認する場合は `<data-root>/log/lookback.log` で
   `jobworkerp maintenance completed` または `jobworkerp maintenance failed` を確認します。
4. 再実行したい場合はアプリを終了し、`jobworkerp-maintenance.json` を削除してから起動します。

## 環境変数

### スレッドグループ要約の timeout と失敗ログ

要約タブの手動生成は、アプリがスレッド要約・ThreadGroup・日次・週次・月次を別ジョブとして
順に実行します。各段の進捗と結果を行別に表示し、画面を開き直した場合も実行中の状態を復元します。
手動の段階生成同士は直列実行し、import の進捗状態とは独立して管理します。
スレッドグループの一括要約は、手動生成では対応する batch の job timeout を使い、定期実行では
子バッチ呼び出しを最大121時間に制限します。メンバー要約の更新はスレッドごとに thread-summary single を
1件ずつ呼び、各件の前に件数を出します。グループ内の LLM 呼び出しや 1 件ごとの single 呼び出しには、jobworkerp の
run / call の既定（1時間）が適用されます。timeout の規約と値は `workers/README.md` の
「タイムアウト」を参照してください。
要約タブ・定期実行・Import からの一括要約は、指定期間（定期実行は lookback 日数、Import は
取り込んだ会話の期間）に会話のあるメンバーを持つグループだけを対象にします。グループ要約は
対象グループの現在の全メンバーから生成します。定期実行のスレッド要約も同じ期間に会話の
あるスレッドだけを対象にします（期間外の未要約スレッドの扱いは `workers/README.md` の
「定期実行 workflow」を参照）。
Import とグループ単体の再生成では、グループごとの失敗を `thread_group_summary`
ターゲットの短い WARN（グループ ID と理由コード）に記録します。失敗の詳細は
DEBUG（最大400文字）でのみ記録します。Import のグループ要約ステージは生の進捗・
終端チャンクを INFO/DEBUG に出力しません。実行が全体 timeout に達した場合も、
既に記録されたグループ別 WARN は残ります。定期実行ではグループ要約を子 workflow として
呼ぶため、その子の途中結果が同じログに記録されるとは限りません。完了前に止まった子 workflow
の診断には子ジョブの結果やトレースが必要です。

### サイドカーのログ既定値

常駐する `jobworkerp`、`memories`、`conductor`、`issue-tracker` サイドカーの `RUST_LOG` は
ビルドプロファイルで決まります。

- `pnpm tauri dev`（debug ビルド）では、`info` 基準です。
- macOS の DMG などの release バンドルでは、通常の進捗ログをパッケージ済みアプリの
  ログへ出しすぎないよう `warn` 基準です。

どちらのプロファイルでも、ログ量の多い対象を抑制する
`app=warn`、`worker_app=warn`、`lance=warn` は維持します。
`LOOKBACK_RUST_LOG` を設定すると、プロファイルにかかわらずフィルター全体を上書きできます。
たとえば `LOOKBACK_RUST_LOG=debug pnpm tauri dev` のように起動します。この設定の対象は
3 つの常駐サイドカーで、短命な `memories-import` プロセスは対象外です。

### ログのローテーションと保持

Lookback はアプリ本体とサイドカーの標準出力・標準エラーを
`<data-root>/log/` に保存します。アクティブログの名前
（`lookback.log` と `<sidecar>.<stdout|stderr>.log`）は固定のままなので、ローテーション後も
アプリ内のログビューアは現在のファイルを表示できます。UTC の日付が変わったとき、または
次の書込みで 10 MiB を超えるときに writer がローテーションします。閉じた旧ファイルには
UTC のタイムスタンプ付きサフィックスが付き、閉じた後は内容を変更しません。

容量上限に関する動作だけが debug/release で異なります。

- debug（`pnpm tauri dev`）と release のどちらも、閉じた管理対象アーカイブを 14 日後に削除します。
- release では、管理対象ログの合計が 256 MiB を超えた場合にも、古い閉じたアーカイブから削除します。
  アクティブログはこの掃除では削除しません。
- debug では合計容量を理由に削除しません。開発時の調査記録は通常のローテーションで保持されます。

サイドカーや `memories-import` が生成する短命プロセスの native tracing ファイルは
`log/native/<起動識別子>/` に隔離されます。親が捕捉する stdout/stderr ファイルを正本とし、
native ファイルは子プロセス終了後だけ同じ 14 日保持の対象にします。実行中のファイルを
rename/delete することはありません。

マイグレーション試行ログ（`database-migration-*.log`）は閉じた監査ログとして 14 日保持します。
release では 256 MiB の容量掃除にも含めます。未知のファイル、ディレクトリ、シンボリックリンク、
アクティブログはログ保守で削除しません。

ローテーション済みログの確認手順:

1. Settings または `LOOKBACK_*` の起動設定から data root を確認し、`<data-root>/log/` と
   `native/` 配下を一覧します。Issue Tracker が利用不能な場合は、まず
   `issue-tracker.stdout.log` と `issue-tracker.stderr.log` を確認します。
2. まず固定名のアクティブログを確認します。ローテーション前のイベントを探す場合は、対象 stream の
   UTC タイムスタンプ付きアーカイブを開きます。アーカイブは通常の text/JSONL なので、アプリ実行中でも
   コピーできます。
3. release の掃除結果を調べる場合は `lookback.log` の retention 実行結果を確認します。掃除に失敗しても
   sidecar の起動や通常処理は停止せず、次の保守間隔で再試行されます。

開発時によく使う上書き設定は次のとおりです。

| Variable | 用途 |
| --- | --- |
| `LOOKBACK_JOBWORKERP_BIN` | `all-in-one` のパス |
| `LOOKBACK_MEMORIES_BIN` | `front` のパス |
| `LOOKBACK_CONDUCTOR_BIN` | `conductor-main` のパス |
| `LOOKBACK_MEMORIES_IMPORT_BIN` | `memories-import` のパス |
| `LOOKBACK_ISSUE_TRACKER_SERVER_BIN` | dev staging 用の事前ビルド済み `issue-tracker-server` のパス |
| `LOOKBACK_ISSUE_TRACKER_SERVER_DIR` | dev staging でビルドする sibling `issue-tracker-server` checkout の上書き |
| `LOOKBACK_ISSUE_TRACKER_MAINTENANCE_INTERVAL_SECS` | Issue Tracker のメンテナンス間隔（秒）。既定値は `5` 秒で、`1` 未満は `1` 秒にクランプし、無効な値は `5` 秒にフォールバック |
| `PROTOC` | `protoc` のパス |
| `LOOKBACK_PLUGINS_SRC` | プラグイン共有ライブラリのソースディレクトリ |
| `LOOKBACK_ISSUE_TRACKER_RUNNER_DIR` | 旧 checkout 上書き環境変数（互換用） |
| `LOOKBACK_LINDERA_SRC` | `memory-store` 用に配置する IPADIC 検索用辞書ファイルのソースディレクトリ |
| `LOOKBACK_WORKERS_DIR` | worker / workflow YAML バンドルの上書き |
| `LOOKBACK_ENV_FILE` | バックエンドプロセスに渡す `.env` テンプレート |
| `LOOKBACK_RUST_LOG` | バックエンドプロセスのログフィルター上書き |
| `LOOKBACK_FORCE_SETUP_WIZARD` | 開発時に初回セットアップウィザードを強制表示 |

LLM と埋め込み生成の設定は通常、設定画面から管理します。外部 LLM の API キーは Keychain など OS の認証情報ストアに保存され、対応する `LOOKBACK_LLM_*` と `LOOKBACK_EMBEDDING_*` は主に開発用の上書き設定です。

## テストとリント

コミット前の標準チェックは次のとおりです。

```bash
pnpm test
pnpm lint
pnpm build
cargo test -p lookback-tauri -- --test-threads=1
cargo clippy -p lookback-tauri --all-targets -- -D warnings
```

一部の統合テストは実際のバックエンドバイナリとプラグインパスを必要とします。テストファイルに必要な `LOOKBACK_*` 変数が記載されている場合は、そのテストを実行する前に明示的に設定してください。
## Ruri ONNX の実機確認

Ruri 対応 runner と `memories` を配置した後は、既存データを変更しない一時 data root で
次の順に確認します。

1. `LOOKBACK_FORCE_SETUP_WIZARD=1` と一時 data root を指定して Lookback を起動する。
2. 日本語 UI の embedding 手順で `Ruri v3 310M` が選択されていることを確認する。
3. setup を適用し、初回ダウンロードと worker load の完了を待つ。
4. Sidecar 状態でモデル名、768 次元、`INT8`、text-only を確認する。
5. テキストを import し、Semantic / Hybrid / チャット RAG の検索結果が返ることを確認する。
6. ログに `UnsupportedDevice`、次元不一致、ONNX operator error がないことを確認する。
