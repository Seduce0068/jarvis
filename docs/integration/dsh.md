---
type: Reference
title: "DSH 連携メモ"
description: "DSH 側の MCP 接続・承認フロー運用メモ"
tags:
  - jarvis
  - dsh
  - mcp
generated:
  by: "DeepSeek Harness (lead agent)"
  at: "2026-11-17T23:33:40+09:00"
---

> **2026-11-15 追記（最新確認）**: 本ドキュメント冒頭の 2026-11-09 実測・結論は当時の状態です。2026-11-15 11:36 の安全デプロイ適用後は、systemd 配下の `jarvis-mcp-gw` で read-only native ツール 4 件（`filesystem.list/read`, `git.diff/status`）の `tools/list` 公開と `tools/call` 成功を確認済みです（詳細は末尾「2026-11-15 現状確認」「運用メモ: 再起動後の再接続（OPS-1）」参照）。再起動後は MCP セッション失効のためクライアント再接続/再 `initialize` が必要です。公開ツール・挙動は実機の `tools/list` / allowlist / unit 設定を正としてください。

## 接続面
- jarvis-mcp-gw: MCP Streamable HTTP `http://127.0.0.1:8082/mcp`（rmcp）
  - `initialize` は成功（serverInfo: jarvis-mcp-gw 0.1.0）
  - `tools/list` は `Accept: application/json, text/event-stream` の両方が必須（片方だと406）
  - 上流MCPサーバー未登録のため公開ツールは現状なし（2026-11-09 当時）
  - `/v1/execute` は500（sandbox fallback が `opencode` CLI 依存で現行CLIと不整合、2026-11-09 当時）
- DSH側: `dsh --profile acp` でACPサーバ、`dsh-mcp-client` でMCPクライアント。

## 現時点の結論
- （2026-11-09 当時）MCPゲートウェイの**ツール公開と実行バックエンドが未完成**のため、DSHからの接続は「土台はあるが実用前」だった。
- 接続順序（推奨、当時メモ）:
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
  未実装ツールを承認必須化（`tools/list` 非表示）。**2026-11-15 11:36 にデプロイ適用済み。**
- デプロイ: `sudo bash infra/scripts/deploy-mcp-gw-safe.sh --apply`
  （事後E2Eが OK になるまでを合格条件とし、失敗時は自動ロールバック）。

デプロイ後の実測（2026-11-15 11:36）:

- `tools/list` = 4ツール（filesystem.list/read, git.diff/status）
- 上記4ツールの `tools/call` が成功する（`entries` / `stdout` を確認）
- `web.fetch` 等の承認必須ツールは非表示、呼ぶと承認必須エラー
- `check-deploy-drift.sh` → `RESULT: OK`
- 注意: ゲートウェイ再起動で MCP セッションは失効するため、クライアントは
  再接続（再 initialize）が必要。

### 運用メモ: 再起動後の再接続（OPS-1）

- `jarvis-mcp-gw` の再起動・デプロイ適用後、既存の MCP セッションは無効になる。
- 症状例: `tools/call` 等で `Unauthorized: Session not found` が返る。
- 対処: クライアント側で MCP 接続を再確立し、`initialize` からやり直す（同じ endpoint 設定のままで可）。
- DSH ではセッション失効後に jarvis MCP を使う操作を再試行する前に再接続状態を確認する。

DSH 側の設定箇所（2026-11-15 確認）:

- このセッションの DSH は `DSH_HOME=/home/koppe/.dsh-agentrouter` で起動しており、
  `~/.dsh-agentrouter/profiles/agentrouter/cordis.patch.yml` に
  `mcp-jarvis`（`serverName: jarvis`, `transport: streamable-http`,
  `url: http://127.0.0.1:8082/mcp`, `failOnStartupError: false`）が定義されている。
- 既定の `~/.dsh` 側には定義がないため、`DSH_HOME` の取り違えに注意。
- ゲートウェイ側の修正デプロイ後は、同じ設定のまま `filesystem.*` / `git.*` が
  実行可能になる（再起動や設定変更は不要）。

## 承認フロー（MCP-1、2026-11-15 実装・未適用）

- `requires_approval = true` のツールも `tools/list` に表示されるようになる
  （旧「非表示」ワークアラウンド廃止。次回デプロイ適用後から有効）。
- 呼び出すと gateway が jarvis-policy の承認要求へ転送し、オペレータが
  承認/却下するまで応答を待つ（policy 側の既定タイムアウトは300秒、
  期限切れは拒否）。クライアントは HTTP/MCP タイムアウトを長めに設定すること。
- オペレータの承認方法（ローカルホストから）:
  1. `curl -s http://127.0.0.1:8081/v1/approvals` で保留中の要求を確認
  2. 承認: `curl -s -X POST http://127.0.0.1:8081/v1/approval/<id>/approve -H 'content-type: application/json' -d '{"approved_by":"koppe"}'`
     却下: `curl -s -X POST http://127.0.0.1:8081/v1/approval/<id>/deny -H 'content-type: application/json' -d '{"reason":"not now"}'`
- jarvis-policy の HTTP（8081）に到達できない場合の呼び出しは fail-closed で拒否される。
- 承認UI・通知・監査永続化（PostgreSQL）は今後の拡張（[remaining-tasks.md](remaining-tasks.md) MCP-1）。

## 関連

- プロジェクト全体の残タスク/判断待ち事項: [remaining-tasks.md](remaining-tasks.md)

### 自動再接続の実機確認（OPS-1、2026-11-17）

DSH に同梱の `@deepseek-ai/dsh-mcp-client` は `reconnect` 設定をサポートしており、
未指定でも既定で自動再接続する（`src/connection.ts` の `RECONNECT_DEFAULTS`）:

- `enabled: true`
- `initialDelayMs: 500`, `maxDelayMs: 30000`（指数バックオフ）
- `maxAttempts: 10`（連続失敗10回で放弃）

したがって gateway 再起動後の `Unauthorized: Session not found` は、
再接続＋再 `initialize` で自動復旧する想定であり、**設定変更は不要**。
`cordis.patch.yml` の `failOnStartupError: false` は起動時の一時障害を
許容するためのもので、そのまま維持する。
