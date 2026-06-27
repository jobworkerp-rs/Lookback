# 開発者ガイド

このガイドには Lookback の開発者向けセットアップ、起動、テスト、環境変数の詳細をまとめています。利用者向けの概要と初回利用チュートリアルは [../README_ja.md](../README_ja.md) を参照してください。

## ディレクトリ構成

```text
src/                 React UI、hooks、API ラッパー、Vitest テスト
src-tauri/           Tauri アプリ、Rust コマンド、バックエンドプロセスのライフサイクル、gRPC クライアント
workers/             Lookback 用 worker / workflow YAML バンドル
proto/               Rust gRPC client が使う vendored protobuf 定義
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
   - `protoc`: staging スクリプトが公式 protobuf リリース（自己完結バイナリ）から自動取得します。
     自前の自己完結 protoc を使う場合は `PROTOC` で上書きできます
2. 必要な jobworkerp ランナープラグインを対象 OS 向け共有ライブラリとしてビルドし、プラグインディレクトリに配置します。
   - [`llama-cpp-runner`](https://github.com/jobworkerp-rs/llama-cpp-runner): ローカル LLM 実行用
   - [`mm-embedding-runner`](https://github.com/jobworkerp-rs/mm-embedding-runner): 埋め込み生成用

   ローカル LLM 実行は Qwen 3.5 系と Gemma 4 系のモデルのみに対応しています。

   現行の macOS バンドルパスは `.dylib` ファイルを使います。Linux ビルドでは対応する共有ライブラリ拡張子とリソースマッピングを使ってください。
3. `memory-store` の `front` バイナリを Lindera FTS 対応でビルドする場合は、互換 IPADIC 検索用辞書を `dict/lindera/ipadic` に配置します。別のディレクトリを使う場合は `LOOKBACK_LINDERA_SRC` を設定します。Lookback はこのファイル群を配置し、`memory-store` 向けに `LANCE_LANGUAGE_MODEL_HOME` を設定するだけで、辞書を直接読み込みません。
4. リポジトリ内の予備パスからバイナリを解決できない場合は、明示的にパスを指定して起動します。

```bash
LOOKBACK_JOBWORKERP_BIN=/path/to/all-in-one \
LOOKBACK_MEMORIES_BIN=/path/to/front \
LOOKBACK_CONDUCTOR_BIN=/path/to/conductor-main \
LOOKBACK_MEMORIES_IMPORT_BIN=/path/to/memories-import \
PROTOC=/path/to/protoc \
LOOKBACK_PLUGINS_SRC=/path/to/plugins \
pnpm tauri:dev
```

`pnpm tauri:dev` と `pnpm tauri dev` は、起動前に `scripts/stage-dev-external-bins.sh` を実行し、Tauri が検証する `src-tauri/bin/<name>-<target-triple>` を実バイナリで配置します。配置元の解決順序はアプリ実行時と同じく、環境変数、`PATH`、workspace 相対の予備パスです。たとえば Linux x86_64 では `src-tauri/bin/all-in-one-x86_64-unknown-linux-gnu` が必要です。

同じ staging で、Tauri が検証する `plugins/*.so` / `plugins/*.so.*` / `*.dylib` も満たします。この glob は `src-tauri` 基準なので、bundle 用の staging 先は `agent-app/src-tauri/plugins/` です。`plugins/*.so.*` は CUDA runtime の `libcudart.so.12` や SONAME symlink のような versioned Linux 共有ライブラリを含めるために必要です。`LOOKBACK_PLUGINS_SRC` が指定されていればその配下を再帰的に探し、未指定の場合は従来の workspace 配置である `agent-app/../../plugins/cuda_runner/` と `agent-app/../../plugins/` から共有ライブラリを探して、`agent-app/src-tauri/plugins/` へコピーします。dev/release staging は `agent-app/../plugins/` には書き込みません。

macOS で動いて Linux で `resource path bin/all-in-one-x86_64-unknown-linux-gnu doesn't exist` になる場合、macOS 側には `src-tauri/bin/*-aarch64-apple-darwin` または `*-x86_64-apple-darwin` が既に残っていて、Linux 用の `*-x86_64-unknown-linux-gnu` が未配置であることが原因です。`scripts/build-release.sh` は release build の中でこの配置を行いますが、通常の dev 起動では release script を通らないため、dev 用 staging が必要です。

Linux の `pnpm tauri:dev` は、Tauri が Rust crate をビルドしている間も Vite 開発サーバーを同時に起動します。Vite は `src-tauri/` と `target/` を監視対象から除外しているため、Cargo が `target/debug/build/*/rustc*` のような一時ディレクトリを作成・削除しても、Vite の file watcher はそのディレクトリを走査しません。

Linux の dev 起動では、WebKitGTK / GDK が Wayland で `Gdk-Message: Error 71 ... dispatching to Wayland display` を出して終了する環境を避けるため、`pnpm tauri dev` は既定で `GDK_BACKEND=x11` と `WEBKIT_DISABLE_DMABUF_RENDERER=1` を設定します。明示的に Wayland を試す場合は、起動時に `GDK_BACKEND=wayland WEBKIT_DISABLE_DMABUF_RENDERER=0 pnpm tauri dev` のように指定すると、その値が優先されます。

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

`scripts/build-release.sh --profile mac`（または `--profile linux-cuda`）は、以下の clone・
GPU feature 付きビルド・バイナリ/プラグイン/lindera の配置・Tauri パッケージングまでを自動化
します。前提条件とフラグはルート README「ソースからビルド」を参照してください。以下の手動手順は
スクリプトが行う内容を記述したもので、部分ビルドやカスタムビルドの参考用です。

Tauri のパッケージングを実行する前に、バックエンドバイナリとリソースを配置する必要があります。

1. `all-in-one`、`front`、`conductor-main`、`memories-import`、`protoc` のリリースバイナリをビルドします。
2. [../src-tauri/tauri.conf.json](../src-tauri/tauri.conf.json) の `externalBin` ベース名と一致する名前で `src-tauri/bin/` に配置します。
3. Tauri のパッケージング用に、対象プラットフォームに対応したプラットフォームトリプルのサフィックス付きバイナリも用意します。
4. [`llama-cpp-runner`](https://github.com/jobworkerp-rs/llama-cpp-runner) と [`mm-embedding-runner`](https://github.com/jobworkerp-rs/mm-embedding-runner) からランナープラグインの共有ライブラリを対象 OS 向けにビルドし、リポジトリルートの `plugins/` に配置します。
5. パッケージに含める `memory-store` の `front` ビルドが Lindera FTS を必要とする場合は、
   `dict/lindera/ipadic` に IPADIC 検索用辞書を配置します。`dict/` 配下は **git 管理外** です。
   `scripts/build-release.sh --lindera-only`（lindera 0.44.1 CLI）で生成すると、IPADIC ソースに
   含まれる `COPYING` ライセンスも辞書と一緒に配置されるため、ライセンスは常にビルドした辞書に
   対応します。`pnpm tauri:dev` で形態素解析 FTS を使いたい場合にも事前生成が有効です（未生成時は
   sidecar が ngram トークナイザにフォールバックします）。
6. `pnpm tauri:build` を実行します。

### Linux AppImage の後処理

`scripts/build-release.sh` は Linux AppImage 生成後に AppImage を一度展開し、linuxdeploy が生成した
GTK runtime hook を補正してから再梱包します。linuxdeploy の標準 hook は
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
CUDA ビルドでは同じ展開 root で NVIDIA driver library の除去も行います。AppImage hook の変更後は
次を実行して、hook の置換と冪等性を確認します。

```bash
bash scripts/test-appimage-hooks.sh
```

### GitHub Actions のディスク容量確保

GitHub hosted runner の Linux リリースジョブは、Tauri の deb/AppImage bundling 前に
`scripts/ci-free-disk-space.sh` を実行します。これは Android SDK、.NET、GHC、CodeQL、言語別
toolcache など、このリリースビルドで使わない大きなプリインストール済みディレクトリを削除し、
削除前後の `df -h` をログに残します。

容量不足を調査する手順:

1. GitHub Actions の `Build Linux bundles (cpu)` ジョブで `Free runner disk space` ステップを開きます。
2. `Disk usage (before)` と `Disk usage (after)` の空き容量を確認します。
3. bundling がまだ `No space left on device` で失敗する場合は、同じジョブの後続ログで `target/`、
   `.build-deps/`、`dict/` の増加量を確認します。
4. 削除対象を増やす場合は `scripts/ci-free-disk-space.sh` の `cleanup_paths` に、このビルドで
   使わない GitHub runner 標準ディレクトリだけを追加します。
5. 変更後は `bash scripts/test-ci-free-disk-space.sh` で root prefix と dry-run の挙動を確認します。

## 環境変数

開発時によく使う上書き設定は次のとおりです。

| Variable | 用途 |
| --- | --- |
| `LOOKBACK_JOBWORKERP_BIN` | `all-in-one` のパス |
| `LOOKBACK_MEMORIES_BIN` | `front` のパス |
| `LOOKBACK_CONDUCTOR_BIN` | `conductor-main` のパス |
| `LOOKBACK_MEMORIES_IMPORT_BIN` | `memories-import` のパス |
| `PROTOC` | `protoc` のパス |
| `LOOKBACK_PLUGINS_SRC` | プラグイン共有ライブラリのソースディレクトリ |
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
