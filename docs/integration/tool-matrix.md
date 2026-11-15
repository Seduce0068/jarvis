# jarvis MCP ツール有効性マトリクス

`infra/config/mcp_allowlist.toml`・`policy/casbin_policy.csv`・`jarvis-mcp-gw`実装の突き合わせ。
**デプロイ後に実際に動作するのは「ネイティブ実装済み」かつ「policy/allowlistで許可」かつ「systemd hardening 下で実行可能」なものだけ**。

## 実装状況（2026-11-15 更新）

| ツール | allowlist | 承認 | ネイティブ実装 | デプロイ後の挙動 |
|---|---|---|---|---|
| `filesystem.read` | あり | 不要 | **✓** | 動作（workspace配下に限定・`ProtectHome=read-only` で systemd 配下も可） |
| `filesystem.list` | あり | 不要 | **✓** | 動作（同上） |
| `git.status` | あり | 不要 | **✓** | 動作（同上） |
| `git.diff` | あり | 不要 | **✓** | 動作（同上） |
| `web.fetch` | あり | **必須** | **✓** | `tools/list` 非表示（承認フロー未配線） |
| `web.search` | あり | **必須**（2026-11-15〜） | ✗ | `tools/list` 非表示（ネイティブ実装待ち） |
| `homeassistant.get_state` | あり | **必須**（2026-11-15〜） | ✗ | `tools/list` 非表示（上流MCP登録待ち） |
| `homeassistant.call_service` | あり | 要 | ✗ | `tools/list` 非表示（承認待ち） |
| `android.get_device_info` | あり | **必須**（2026-11-15〜） | ✗ | `tools/list` 非表示（ネイティブ実装待ち） |
| `android.send_notification` | あり | 要 | ✗ | `tools/list` 非表示（承認待ち） |
| `filesystem.write` | あり | 要 | ✗ | `tools/list` 非表示（承認フロー未実装のため実質拒否） |
| `shell.execute` | あり | 要 | ✗ | `tools/list` 非表示（同上） |
| `git.commit` | あり | 要 | ✗ | `tools/list` 非表示（同上） |
| `github.create_pr` | あり | 要 | ✗ | `tools/list` 非表示（同上） |

## 設定上のドリフト（要整理・継続）

1. **allowlistの`sandbox_type`は無視される**
   - `AllowedTool`構造体に`sandbox_type`が無く、TOMLの値は読まれない（`sandbox_type = "firejail"`等は死んだ設定）。
   - 実際のサンドボックス種別は`McpGatewayConfig.sandbox_type`（配備版は env `JARVIS_MCP_GW_SANDBOX_TYPE=firejail`）。
   - 対応: キーを削除するか、`AllowedTool`へ追加してツール単位ルーティングを実装。
2. **未実装ツールのallowlistエントリ**
   - 2026-11-15 に対応済み: ネイティブ未実装ツールは `requires_approval = true` とし、`tools/list` から非表示にした。
   - 実装/upstream登録後に承認要否を再設計する。
3. **`web.fetch`の承認**
   - 2026-11-10 に `requires_approval = true` を配備済み。承認フロー実装後に扱いを再検討。
   - 承認フロー実装後は、宛先ドメイン制限（egress allowlist）も検討。

## デプロイ前チェックリスト

- [x] `web.fetch` の `requires_approval = true` 適用（2026-11-10 配備）
- [x] 未実装ツールの扱いを決定（2026-11-15: 承認必須化・非表示）
- [ ] `sandbox_type`キーの整理方針を決定
- [ ] `jarvis-mcp-gw`リリースバイナリのSHA256記録
- [ ] デプロイ後に `/mcp` の `tools/list` で意図したツールだけ出ることを確認（systemd 実行時E2Eは `check-deploy-drift.sh` に追加済み）

## スモークテストによる実証（2026-11-09 05:08）

`infra/scripts/smoke-mcp-gw.sh` で新バイナリを別ポート起動して確認:

- **approvalモード**（`requires_approval = true`）: `web.fetch` → **403**（安全）
- **activeモード**（web.fetchのみ承認不要へ戻す）: `web.fetch` → **200・example.com取得成功**

## デプロイ結果（2026-11-10 01:30）

- `deploy-mcp-gw-safe.sh --apply --allow-web-fetch` で配置・再起動。
- バイナリ: repo/installed のSHA256一致、`jarvis-mcp-gw` **active**。
- `web.fetch`: `requires_approval = true` で配置し、実サービスのMCP `tools/list` に**非表示**を確認。
- allowlist/policy: `/etc`とrepoが一致（policy 1行も同期）、`check-deploy-drift.sh` → **RESULT OK**。

## 2026-11-15 現状確認: systemd hardening との不整合を検出

稼働中サービス（systemd 配下）に対する実測で、**`tools/list` に出る全ツールが実行不能**であることが判明した。

| 確認項目 | 結果 |
|---|---|
| `tools/list` | 6ツール（filesystem.list/read, git.diff/status, homeassistant.get_state, web.search） |
| filesystem.list/read, git.status/diff の `tools/call` | `canonicalize base /home/koppe: Permission denied (os error 13)` |
| web.search, homeassistant.get_state の `tools/call` | firejail フォールバックが `create_empty_dir_as_root: Permission denied` で失敗 |
| DSH から呼び出した場合 | 同じエラー（ゲートウェイ経由で再現） |

原因は2点:

1. unit の `ProtectHome=yes` が `/home` をマスクする一方、ネイティブハンドラの基準ディレクトリ既定値が `/home/koppe`（`JARVIS_MCP_GW_BASE_DIR` 未設定）だったため、`canonicalize` が EACCES で失敗。
2. ネイティブ未実装ツールがレガシー firejail 経路へ落ちるが、`NoNewPrivileges` 等の hardening 下では firejail が起動できない。

従来のスモークテストは systemd 外で別ポート起動していたため、この乖離を検出できなかった。

### 修正（2026-11-15 デプロイ適用済み）

- `infra/systemd/jarvis-mcp-gw.service`
  - `ProtectHome=yes` → `ProtectHome=read-only`（`/home` を読み取り専用で参照可能に）
  - `JARVIS_MCP_GW_BASE_DIR=/home/koppe` を明示（読み取り境界を unit 上で明確化）
  - デプロイ実態に合わせて `User`/`Group` を `koppe` に同期
- `infra/config/mcp_allowlist.toml`
  - ネイティブ未実装の `web.search` / `homeassistant.get_state` / `android.get_device_info` を `requires_approval = true` に変更（`tools/list` から非表示）
- `infra/scripts/smoke-mcp-gw.sh`
  - `tools/call`（filesystem.list / git.status）の成功を検証
  - 承認待ち・未実装ツールが `tools/list` に出ないことを検証
- `infra/scripts/check-deploy-drift.sh`
  - systemd unit の配備差分を検出
  - 稼働中サービスへの実 `tools/call` を実行し、hardening 起因の実行時失敗を検出
- `infra/scripts/deploy-mcp-gw-safe.sh`
  - unit/allowlist の同期、`daemon-reload`、事後E2E必須、失敗時の全変更ロールバックを追加

### デプロイ手順（要root）

```bash
cd ~/ドキュメント/作業用/jarvis/jarvis
bash infra/scripts/deploy-mcp-gw-safe.sh              # 乾燥実行（差分・スモーク確認）
sudo bash infra/scripts/deploy-mcp-gw-safe.sh --apply # 同期・再起動・事後E2Eまで
bash infra/scripts/check-deploy-drift.sh              # RESULT: OK を確認
```

### デプロイ結果（2026-11-15 11:36、実測）

`sudo bash infra/scripts/deploy-mcp-gw-safe.sh --apply` を実行。事後E2Eまで含めて合格。

| 確認項目 | 結果 |
|---|---|
| `deploy-mcp-gw-safe.sh --apply` | `DONE: APPLIED（事後確認 OK）` / `DEPLOY_EXIT=0` |
| `check-deploy-drift.sh`（独立再実行） | binary/allowlist/policy/unit **全て OK**、`RESULT: OK`（exit 0） |
| 稼働 unit | `User=koppe`, `ProtectHome=read-only`, `JARVIS_MCP_GW_BASE_DIR=/home/koppe` |
| サービス | `jarvis-mcp-gw` active（2026-11-15 11:36:49 起動） |
| `/health` | OK |
| `tools/list` | **4ツールのみ**（filesystem.list/read, git.diff/status） |
| `filesystem.list` / `filesystem.read` | 成功（`entries` / 本文を返す） |
| `git.status` / `git.diff` | 成功（`stdout` を返す） |
| `web.fetch` / `homeassistant.get_state` | 承認必須で拒否（`requires human approval`） |
| スモーク（approval モード） | PASS (ok=11 fail=0) |

注: ゲートウェイ再起動により MCP セッションは失効する。クライアント（DSH 等）は
再接続（再 initialize）が必要。

## 運用メモ: 再起動後の再接続（OPS-1）

- `jarvis-mcp-gw` の再起動・デプロイ適用後は既存 MCP セッションが無効になる。
- 旧セッションでの呼び出しは `Unauthorized: Session not found` になり得る。
- 対処: クライアント側で接続を再確立し `initialize` から再実行する（endpoint/設定変更は通常不要）。

## 残リスク（要検討）

- 基準ディレクトリが `/home/koppe` 全体のため、読み取りツールは SSH 鍵や `.env` 等の機微ファイルにも到達し得る（localhost 限定・policy/allowlist 前提）。基準ディレクトリの絞り込み（例: 作業ディレクトリのみ）や機微パスの拒否リストを次段で検討する。
- 承認フローは未実装。`web.fetch` 等は「承認必須・非表示」のまま実用不可。

## 全体タスク一覧

MCP 以外を含むプロジェクト全体の残タスク・判断待ち事項は [remaining-tasks.md](remaining-tasks.md) を参照。
