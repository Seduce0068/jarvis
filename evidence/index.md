# Test Evidence

JARVIS のテスト実行エビデンス（JSON）の索引です。
各ファイルは対応仕様のハッシュと実測結果を記録します。
内容の根拠は各本文を参照してください（索引は候補選択の材料であり保証ではありません）。
JSON はテキスト形式のため、リンク先として記載しています。

## エビデンス一覧

- [policy-approval の実行ログ][run-policy]
  — [test-policy-approval.md](../specs/test-policy-approval.md) の実測結果。
  49 passed / 0 failed。
- [mcp-gateway の実行ログ][run-mcp]
  — [test-mcp-gateway.md](../specs/test-mcp-gateway.md) の実測結果。
  40 passed / 0 failed / 1 skipped。
- [context の実行ログ][run-context]
  — [test-context.md](../specs/test-context.md) の実測結果。17 passed / 0 failed。
- [backup の実行ログ][run-backup]
  — [test-backup.md](../specs/test-backup.md) の実測結果。19 passed / 0 failed。
- [event の実行ログ][run-event]
  — [test-event.md](../specs/test-event.md) の実測結果。24 passed / 0 failed。
- [ui の実行ログ][run-ui]
  — [test-ui.md](../specs/test-ui.md) の実測結果。14 passed / 0 failed。

## 関連

- [specs/index.md](../specs/index.md): 対応するテスト仕様の索引。

[run-policy]: test-run-20261118-c0b30fe96b4d.json
[run-mcp]: test-run-20261118-da02ce4447c3.json
[run-context]: test-run-20261118-aece518bbdca.json
[run-backup]: test-run-20261118-a40f4bc81f5c.json
[run-event]: test-run-20261118-a4b7089f41f3.json
[run-ui]: test-run-20261118-2b7a98b3f085.json
