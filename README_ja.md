# Lookback

Lookback は、ローカルの Claude Code / Codex セッションログを検索可能な長期記憶として扱うための、クロスプラットフォーム Tauri デスクトップアプリです。

セッションログを [`memory-store`](https://github.com/jobworkerp-rs/memory-store) に取り込み、スレッド閲覧、要約、自省、パーソナリティ分析、RAG チャットをデスクトップ UI から利用できます。

英語ドキュメントは [README.md](README.md) を参照してください。開発者向けのセットアップ、テスト、環境変数の詳細は [docs/developer-guide_ja.md](docs/developer-guide_ja.md) に分離しています。

## できること

- ローカルの `claude-code` / `codex` セッションログをインポート。
- ディレクトリ内のプレーンテキスト (`.md` / `.txt`) を、ファイルごと・ディレクトリごと・全体で 1 スレッドのいずれかにまとめてスレッドとしてインポート。
- インポート済みスレッドを閲覧し、キーワード検索、セマンティック検索、ハイブリッド検索で検索。
- スレッド、日次、週次、月次の要約を生成。
- 過去セッションから自省とパーソナリティプロファイルを生成。
- チャットタブからインポート済みの記憶に対する RAG 質問を実行。
- RAG 検索ツールを外部 MCP クライアントへ公開。
- 同梱 conductor バックエンドで定期インポートと要約タスクをスケジュール。

## 対応プラットフォーム

Lookback は Tauri が対応するデスクトップ環境を対象にします。現在のリポジトリは macOS と Linux のビルドを想定しています。

ローカル LLM 実行は現在 Qwen 3.5 系と Gemma 4 系のモデルのみに対応しています。その他のモデルファミリーを使う場合は外部 LLM プロバイダーを利用してください。

## 必要なコンポーネント

このリポジトリにはアプリ UI が含まれますが、デスクトップアプリ全体を動かすには同梱バックエンドバイナリと jobworkerp プラグインが必要です。

- [`jobworkerp`](https://github.com/jobworkerp-rs/jobworkerp-rs): `all-in-one`
- [`memory-store`](https://github.com/jobworkerp-rs/memory-store): `front` / `memories-import`
- [`jobworkerp-conductor`](https://github.com/jobworkerp-rs/jobworkerp-conductor): `conductor-main`
- `protoc`: 自己完結した公式 protobuf コンパイラ。ビルド時に自動取得します（子プロセスが worker 登録時に
  runner スキーマをコンパイルするため実行時必須で、同梱バイナリとして配布します）
- [`llama-cpp-runner`](https://github.com/jobworkerp-rs/llama-cpp-runner): ローカル LLM ランナープラグイン
- [`mm-embedding-runner`](https://github.com/jobworkerp-rs/mm-embedding-runner): 埋め込み生成ランナープラグイン

フロントエンドの検査はこれらのバイナリなしでも実行できます。デスクトップアプリ全体の起動、ログインポート、モデルロード、配布パッケージのビルドには必要です。

## ソースからビルド

### 前提条件

ビルドホストに以下をインストールしてください。

- Rust edition 2024 に対応した Rust toolchain（rustc >= 1.85、`rustup` 経由）。
- Node.js と `pnpm`。
- 対象 OS 向け Tauri v2 の前提条件。
- バックエンドの各リポジトリとそのネイティブ依存が必要とするビルドツール:
  `git`、`cmake`、`pkg-config`、`curl` / `unzip` / `tar`。公式 `protoc` はビルド時に（`curl` + `unzip` で）
  自動取得するため、ホストへの protoc インストールは不要です。オフラインビルド時は自己完結した protoc を
  `PROTOC` に指定してください。
  - macOS: Xcode Command Line Tools（`xcode-select --install`）と、Homebrew で
    `brew install cmake pkgconf`。
  - Linux（Debian/Ubuntu）: `apt-get install -y cmake pkg-config build-essential`。
- Linux で CUDA ビルドを行う場合: CUDA toolkit（`nvcc`）。フルランタイムには `libcudnn` / `libnccl`。

### 自動ビルド（推奨）

`scripts/build-release.sh` は、5 つの公開バックエンドリポジトリを clone し、プラットフォームと
GPU バックエンドに応じた feature でビルドし、バイナリ / プラグイン / lindera 辞書を Tauri が
期待する場所に配置したうえで `pnpm tauri build` を実行します。

```bash
# macOS（Metal GPU、DMG + .app）
scripts/build-release.sh --profile mac

# Linux（CUDA GPU、deb + AppImage）
scripts/build-release.sh --profile linux-cuda
```

便利なフラグ: `--skip-clone`（既存 clone を再利用）、`--only <repos>`（一部のみ再ビルド）、
`--workdir <dir>`（clone/ビルド先、既定 `.build-deps/`）、`--skip-frontend`、`--lindera skip`。
全オプションは `scripts/build-release.sh --help` を参照してください。`DRY_RUN=1` を前置すると
実行せずにコマンドだけ表示できます。

生成されるパッケージ（macOS の `.app` / DMG、Linux パッケージターゲットなど）には、
`src-tauri/tauri.conf.json` に従って UI、workers、任意の検索用辞書ファイル、プラグイン、
同梱バックエンドバイナリが含まれます。

### GitHub Actions でのリリースビルド

タグ push で起動する公開リポジトリの `.github/workflows/release.yml` は、テスト後に署名付き
macOS DMG を作成し、その完了後に Linux CPU の deb / AppImage を作成します。macOS 署名と
notarization には、公開リポジトリの GitHub Secrets に次の値を登録してください。

1. Keychain から `Developer ID Application` 証明書を秘密鍵付きの `.p12` として書き出します。
2. `.p12` を base64 化します。

   ```bash
   openssl base64 -A -in certificate.p12 -out certificate-base64.txt
   ```

3. GitHub Secrets に `APPLE_CERTIFICATE`（`certificate-base64.txt` の中身）、
   `APPLE_CERTIFICATE_PASSWORD`、`APPLE_SIGNING_IDENTITY`、`APPLE_ID`、`APPLE_PASSWORD`、
   `APPLE_TEAM_ID` を登録します。`APPLE_PASSWORD` は Apple ID の通常パスワードではなく、
   app-specific password を使います。

### 手動ビルド

バックエンドを自分でビルドする場合は、成果物を次のように配置して
`pnpm install && pnpm tauri:build` を実行します。

1. `src-tauri/bin/` に、Tauri が期待するターゲットトリプルのサフィックス付き
   （`-aarch64-apple-darwin`、`-x86_64-apple-darwin`、または Linux のターゲットトリプル）で
   バックエンドバイナリを配置します。

   ```text
   src-tauri/bin/all-in-one-<triple>
   src-tauri/bin/front-<triple>
   src-tauri/bin/conductor-main-<triple>
   src-tauri/bin/memories-import-<triple>
   src-tauri/bin/protoc-<triple>
   ```

2. ランナープラグイン（`libjobworkerp_llama_cpp_plugin` と `libmm_embedding_runner`）を対象 OS
   向けにビルドし、共有ライブラリを `plugins/` に配置します。
3. `memory-store` の `front` を Lindera FTS 対応でビルドする場合は、検索用辞書（lindera 0.44.1
   形式）を `dict/lindera/ipadic` に配置します。Lookback はこのディレクトリをパッケージに含めて
   `memory-store` 用に配置しますが、辞書を直接読み込んで解析するわけではありません。

開発時の起動コマンドやパスの上書きは [docs/developer-guide_ja.md](docs/developer-guide_ja.md) を参照してください。

## 初回利用チュートリアル

1. Lookback を起動します。
2. セットアップウィザードを完了します。
   - データルートと Hugging Face キャッシュの場所を選びます。
   - ローカルまたは外部 LLM プロバイダーを選びます。
     - 外部 LLM の API キーは、Keychain など OS の認証情報ストアに保存されます。
   - 埋め込みモデルを選びます。
   - バックエンドとモデル準備状態の検査結果を確認します。
3. プロバイダー、モデルパス、キャッシュパス、言語、MCP 公開設定を変更したい場合は、後から **設定** を開きます。
4. **スレッド** を開き、**インポート** から Claude Code または Codex のセッションログを取り込みます。ディレクトリ内のプレーンテキストを取り込む場合は、**プレーンテキスト (ディレクトリ)** をチェックし、対象ディレクトリを選択して、スレッド分割方法を選びます。
   - **ファイルごと**: 1 ファイル = 1 スレッド。
   - **ディレクトリごと**: 同じディレクトリ内のファイルを 1 スレッドにまとめる。
   - **全体で1つ**: ディレクトリ全体を 1 スレッドにする。

   プレーンテキストインポートは `.md` / `.txt` を再帰的に読み込みます。任意の **ソース名** (`a-z0-9_-`、32 字以内) を指定すると、インポート済みスレッドの名前空間プレフィックスになります（空の場合はインポーターの既定値を使用）。プレーンテキストは Claude Code / Codex と同じ実行でまとめて取り込めます。
5. **スレッド** でインポート済みスレッドを閲覧します。まずキーワード検索を使い、埋め込み生成ランナーが利用可能な場合はセマンティック検索とハイブリッド検索も利用できます。
6. **要約** でスレッド単位・期間単位の要約を生成または確認します。
7. **自省** と **パーソナリティ** で、インポート済みセッションから高次の観察結果を生成します。
8. **チャット** で過去の履歴に質問します。回答内のソースリンクから関連スレッドや要約を辿れます。
9. 定期的にインポートや要約を実行したい場合は **定期実行** を設定します。

## MCP サーバ

Lookback は、Claude Desktop などの外部 MCP クライアント向けに RAG 検索機能を MCP サーバとして公開できます。MCP サーバは、通常の gRPC バックエンドと同じ `jobworkerp` sidecar 内で並行して動作するため、有効化してもアプリ内の閲覧、インポート、チャット機能は置き換わりません。

MCP で公開される範囲は `lookback-mcp-rag` function-set に限定されます。現在公開されるツールは次の 1 つです。

- `lookback_recall`: 生成済みの要約とインポート済みの元メッセージの両方から、過去の会話、判断、作業履歴を検索します。

有効化手順:

1. **設定** を開きます。
2. **MCP Server** でサーバを有効にします。
3. 設定を保存します。MCP は sidecar 起動時に設定されるため、Lookback は sidecar を再起動します。
4. 表示された接続先 URL をコピーします。通常の優先ローカル URL は次の形式です。

```text
http://127.0.0.1:39010/mcp
```

このポートが使用中の場合、Lookback は別の空きポートを選び、実際の URL を設定画面に表示します。

5. streamable HTTP transport として MCP クライアントに URL を登録します。クライアント設定例:

```json
{
  "mcpServers": {
    "lookback": {
      "url": "http://127.0.0.1:39010/mcp"
    }
  }
}
```

Lookback がリモートの `memory-store` を参照する設定の場合、MCP 検索も可能な範囲で同じ有効な記憶接続先を使います。リモート URL が不正または未設定の場合、RAG workflow はローカル sidecar の記憶エンドポイントにフォールバックします。

## アプリデータ

Lookback の永続データは OS のアプリケーションデータディレクトリに保存されます。代表的な既定値は次のとおりです。

```text
macOS: ~/Library/Application Support/lookback/
Linux: ~/.local/share/lookback/
```

このルートには SQLite / LanceDB データ、配置済みプラグイン、モデルキャッシュ設定、バックエンドプロセスログ、接続設定、孤立プロセスのクリーンアップ用 PID ファイルが含まれます。設定画面からデータルートを消去できます。

## 関連ドキュメント

- [README.md](README.md): 英語 README。
- [docs/developer-guide_ja.md](docs/developer-guide_ja.md): 開発者ガイド。
- [workers/README.md](workers/README.md): worker / workflow YAML bundle。
