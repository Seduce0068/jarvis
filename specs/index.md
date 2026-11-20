# Test Specifications

JARVIS のテスト仕様の索引です。
内容の根拠は各本文を参照してください（索引は候補選択の材料であり保証ではありません）。

## 仕様一覧

- [test-policy-approval.md](test-policy-approval.md)
  — jarvis-policy 承認フロー テスト仕様。kill switch / トークン認証 / キュー上限 / 重複 ID / 期限切れ 410 の動的試験設計。
- [test-mcp-gateway.md](test-mcp-gateway.md)
  — jarvis-mcp-gw 実行経路 テスト仕様。
  policy-first / allowlist / 引数上限 / 承認トークン / kill switch 再検査。
- [test-context.md](test-context.md)
  — jarvis-context PII フィルタ・検索順序 テスト仕様。
  再帰的 redact / enabled=false / 不正パターン / reorder_by_ids。
  スコア保持（REQ-12/13: attach_scores / スコア0 とランク外の区別）。
- [test-backup.md](test-backup.md)
  — ローカルバックアップ テスト仕様。
  backup-local.sh / verify-backup.sh / install-backup-timer.sh。
- [test-event.md](test-event.md)
  — jarvis-event Webhook・Backpressure テスト仕様。webhook ペイロード変換と bounded channel の動的試験設計。
- [test-ui.md](test-ui.md)
  — 承認コンソール UI テスト仕様。エラー保持 / 期限切れ強調 /
  トークンリダイレクト防止 / キーボード a11y / full args / タイムアウト。
  scope 別トークン（REQ-19/20: 未設定時はヘッダなし / admin→API フォールバック）。

## 関連

- [evidence/index.md](../evidence/index.md): 各仕様に対応するテスト実行エビデンスの索引。
- [todo.md](../todo.md): 仕様の根拠とした残タスク・判断事項。
