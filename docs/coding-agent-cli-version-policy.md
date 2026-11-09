# Coding agent CLI version 運用

Coding agent の CLI version は、実行前の診断で取得します。特定の固定 version や semver 範囲を満たさないことだけを理由に実行を拒否しません。利用者が CLI を更新することも許容しますが、更新後の互換性確認と、互換性を損なった場合の復旧は利用者の責任です。

## image を更新するとき

更新前後の image で、次の項目を確認してください。

1. CLI の `--version` と `--help` が成功すること。
2. 対象 CLI の起動引数、認証、作業ディレクトリ、初期 prompt が意図どおり動くこと。
3. 実際の provider を使った通常 run が成功し、正常終了・取消・結果保存が成立すること。

現在の起動引数は次のとおりです。prompt は shell 文字列へ埋め込まず、独立した argv 要素として渡します。

- OpenCode interactive: `opencode --prompt=<prompt>`
- OpenCode one-shot: `opencode run -- <prompt>`
- Codex interactive: `codex --no-daemon -- <prompt>`
- Codex one-shot: `codex --no-daemon exec -- <prompt>`
- Claude Code interactive: `claude -- <prompt>`
- Claude Code one-shot: `claude -p -- <prompt>`

image の更新時は、CLI version の値そのものではなく、更新後の実引数と CLI の挙動を確認してください。専用 HOME の外にある CLI binary は、更新しても sandbox 再作成後の永続化を保証しません。

## 互換性に問題がある場合

通常 run、認証、prompt、または終了処理に問題がある場合は、まず動作確認済みの元 image へ戻して実行を復旧します。そのうえで更新後 CLI の引数変更や認証要件を確認し、必要な修正版 image と設定を準備してから再度更新します。
