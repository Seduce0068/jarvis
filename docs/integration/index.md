# Integration Documents

JARVIS の連携・残タスク・バックアップに関する文書の索引です。
内容の根拠は各本文を参照してください（索引は候補選択の材料であり保証ではありません）。

## 文書一覧

- [remaining-tasks.md](remaining-tasks.md)
  — 全プロジェクトの残タスク・判断待ち事項（MCP / BK / SEC / CI / DOC / OPS）。
- [unknown-tasks-survey.md](unknown-tasks-survey.md)
  — 5観点のエージェント並列調査と6回目の吸収で洗い出した未掲載の残タスク・機能追加案（2026-11-17）。
- [tool-matrix.md](tool-matrix.md)
  — MCP ツールの allowlist・承認・ネイティブ実装の突き合わせ。
- [dsh.md](dsh.md)
  — DSH 側の MCP 接続・承認フロー・再起動時の再接続運用メモ。
- [backup-redesign-notes.md](backup-redesign-notes.md)
  — OCI 非使用前提のバックアップ再設計メモ（前提棚卸し・安全ゲート・実装状況）。
- [backup-restore-runbook.md](backup-restore-runbook.md)
  — ローカルバックアップの復旧手順（隔離先での非破壊検証を前提）。
- [local-operations-runbook.md](local-operations-runbook.md)
  — ローカル systemd サービスの起動・検証・停止、手動変更のロールバック、
    `make verify-backup` の扱い方針、clippy 表記の統一。

## 関連

- [ADR-0004](../../adr/0004-local-only-no-oci.md): OCI 非使用・ローカル運用
- [ADR-0005](../../adr/0005-gpui-desktop-adoption.md): GPUI 承認コンソール
