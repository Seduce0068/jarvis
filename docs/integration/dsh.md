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


## 更新（2026-11-09 01:07）: ネイティブハンドラ実装
- `sandbox.rs` の `execute_native` を **Rustネイティブ実装**へ置換（opencode CLI依存を排除）。
  - `filesystem.read` / `filesystem.list` / `git.status` / `git.diff`
  - パスは `JARVIS_MCP_GW_BASE_DIR`（既定 `/home/koppe`）配下に制限（canonicalizeで脱出拒否）
  - 読み取り上限 1MiB、gitはツール設定のtimeoutを適用
- 別ポートの実バイナリでE2E検証:
  - `tools/list` = 7ツール（filesystem.list/read, git.diff/status, homeassistant.get_state, web.fetch/search）
  - `filesystem.list` / `filesystem.read` / `git.status` の **tools/call 成功**
- 未実装（明確なエラーを返す）: `web.*` / `homeassistant.*` は**上流MCPサーバー登録**で対応予定。
  approval系（write/shell/commit）は従来通りfirejail等のレガシー経路（要見直し）。

### デプロイ手順（要root）
```bash
cd ~/ドキュメント/作業用/jarvis/jarvis/rust
cargo build --release -p jarvis-mcp-gw
sudo install -o root -g root -m 0755 target/release/jarvis-mcp-gw /opt/jarvis/bin/jarvis-mcp-gw
sudo systemctl restart jarvis-mcp-gw
curl -s http://127.0.0.1:8082/health
```

## web.fetch（ネイティブ実装・2026-11-09）

- `web.fetch` をネイティブハンドラとして実装（HTTP/HTTPS GET）。
  - 公開IPのみ許可（loopback/private/link-local/ULA/multicast等は拒否）
  - リダイレクトは手動検証（最大3回・各ホップで公開IP確認）
  - テキスト系content-typeのみ、サイズ上限1MiB（既定256KiB）
  - URL内の認証情報を拒否
- **注意**: allowlistには`web.fetch`が既に登録済み（`requires_approval = false`）。
  そのため**新バイナリのデプロイで有効化される**。安全のため、デプロイ前に
  `requires_approval = true` へ変更するか宛先制限を導入すること
  （`adr/0003-native-web-fetch.md`）。
- 有効化/安全化の手順:
  1. `infra/config/mcp_allowlist.toml` に `web.fetch` エントリを追加（`requires_approval = true` 推奨）
  2. `infra/config/policy/casbin_policy.csv` に `tool:web.fetch` のexecute権限を追加
  3. バイナリを再配置して `systemctl restart jarvis-mcp-gw`
- 有効化後は、取得先・サイズ・リダイレクトの制約をテストで確認すること。
