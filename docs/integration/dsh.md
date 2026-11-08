# DSH 連携メモ（2026-11-09 実測）

## 接続面
- jarvis-mcp-gw: MCP Streamable HTTP `http://127.0.0.1:8082/mcp`（rmcp）
  - `initialize` は成功（serverInfo: jarvis-mcp-gw 0.1.0）
  - `tools/list` は `Accept: application/json, text/event-stream` の両方が必須（片方だと406）
  - 上流MCPサーバー未登録のため公開ツールは現状なし
  - `/v1/execute` は500（sandbox fallback が `opencode` CLI 依存で現行CLIと不整合）
- DSH側: `dsh --profile acp` でACPサーバ、`dsh-mcp-client` でMCPクライアント。

## 現時点の結論
- MCPゲートウェイの**ツール公開と実行バックエンドが未完成**のため、
  DSHからの接続は「土台はあるが実用前」。
- 接続順序（推奨）:
  1. jarvis-mcp-gw の `tools/list` 応答をSSE/JSONどちらでも成立するよう確認
  2. allowlist由来ツールを `tools/list` に出す（現在は上流サーバー登録のみが対象）
  3. sandbox fallback を runtime-agnostic に置換（opencode CLI 依存を解消）
  4. DSH の MCP 設定に `/mcp` を追加（serverName: jarvis）
  5. 読み取りツールから段階許可 → 書き込みは approval 必須

## Zed / ACP
- Zed は ACP クライアント。`dsh --profile acp` をエージェントとして登録可能。
- OpenCode も ACP 対応なら同一UIで切替可能（ADR-0002の交換可能性）。
