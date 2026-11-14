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

## 安全デプロイ（2026-11-09）

```bash
cd ~/ドキュメント/作業用/jarvis/jarvis
bash infra/scripts/deploy-mcp-gw-safe.sh --allow-web-fetch          # 乾燥実行（計画+スモーク）
sudo bash infra/scripts/deploy-mcp-gw-safe.sh --apply --allow-web-fetch
bash infra/scripts/check-deploy-drift.sh                            # 事後確認
```

## 2026-11-15 現状確認: systemd 配下での実行不能と修正

DSH からの接続自体は成立している（jarvis-mcp-gw のログに `dsh-mcp-client` の
initialize を確認、直近 2026-11-15 00:59）。ただし systemd 配下の実サービスでは
`tools/list` に出る全ツールが実行不能だった。

- 症状: `filesystem.*` / `git.*` は `canonicalize base /home/koppe: Permission denied`、
  `web.search` / `homeassistant.get_state` は firejail フォールバック失敗。
- 原因: unit の `ProtectHome=yes` と基準ディレクトリ既定値 `/home/koppe` の不整合。
- 修正: unit を `ProtectHome=read-only` + `JARVIS_MCP_GW_BASE_DIR=/home/koppe` に変更し、
  未実装ツールを承認必須化（`tools/list` 非表示）。repo 反映済み・デプロイ待ち。
- デプロイ: `sudo bash infra/scripts/deploy-mcp-gw-safe.sh --apply`
  （事後E2Eが OK になるまでを合格条件とし、失敗時は自動ロールバック）。

修正後の期待状態:

- `tools/list` = 4ツール（filesystem.list/read, git.diff/status）
- 上記4ツールの `tools/call` が成功する
- `web.fetch` 等の承認必須ツールは非表示のまま

DSH 側の設定箇所（2026-11-15 確認）:

- このセッションの DSH は `DSH_HOME=/home/koppe/.dsh-agentrouter` で起動しており、
  `~/.dsh-agentrouter/profiles/agentrouter/cordis.patch.yml` に
  `mcp-jarvis`（`serverName: jarvis`, `transport: streamable-http`,
  `url: http://127.0.0.1:8082/mcp`, `failOnStartupError: false`）が定義されている。
- 既定の `~/.dsh` 側には定義がないため、`DSH_HOME` の取り違えに注意。
- ゲートウェイ側の修正デプロイ後は、同じ設定のまま `filesystem.*` / `git.*` が
  実行可能になる（再起動や設定変更は不要）。
