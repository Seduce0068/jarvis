---
type: Reference
title: "jarvis MCP ツール有効性マトリクス"
description: "MCP ツールの allowlist・承認・実装状況"
tags:
  - jarvis
  - mcp
  - tools
generated:
  by: "DeepSeek Harness (lead agent)"
  at: "2026-11-17T23:33:40+09:00"
---

`infra/config/mcp_allowlist.toml`・`policy/casbin_policy.csv`・`jarvis-mcp-gw`実装の突き合わせ。
**デプロイ後に実際に動作するのは「ネイティブ実装済み」かつ「policy/allowlistで許可」かつ「systemd hardening 下で実行可能」なものだけ**。

## 実装状況（2026-11-15 更新）

> **2026-11-15 追記（MCP-1 承認フロー実装・repo側）**: `requires_approval = true` の
> ツールも `tools/list` に表示されるよう変更（旧「非表示」ワークアラウンド廃止）。
> 呼び出すと gateway が jarvis-policy の承認要求に転送し、オペレータが
> `POST /v1/approval/{id}/approve|deny` で決定するまで待機する（fail-closed）。
> 実機への有効化は次回デプロイ適用時。

| ツール | allowlist | 承認 | ネイティブ実装 | デプロイ後の挙動 |
|---|---|---|---|---|
| `filesystem.read` | あり | 不要 | **✓** | 動作（workspace配下に限定・`ProtectHome=read-only` で systemd 配下も可） |
| `filesystem.list` | あり | 不要 | **✓** | 動作（同上） |
| `git.status` | あり | 不要 | **✓** | 動作（同上） |
| `git.diff` | あり | 不要 | **✓** | 動作（同上） |
| `web.fetch` | あり | **必須** | **✓** | `tools/list` 表示・呼び出しは人間承認待ち（MCP-1 実装済み・未適用） |
| `web.search` | あり | **必須**（2026-11-15〜） | ✗ | `tools/list` 表示・呼び出しは人間承認待ち（ネイティブ実装待ち） |
| `homeassistant.get_state` | あり | **必須**（2026-11-15〜） | ✗ | `tools/list` 表示・呼び出しは人間承認待ち（上流MCP登録待ち） |
| `homeassistant.call_service` | あり | 要 | ✗ | `tools/list` 表示・呼び出しは人間承認待ち（MCP-1 実装済み・未適用） |
| `android.get_device_info` | あり | **必須**（2026-11-15〜） | ✗ | `tools/list` 表示・呼び出しは人間承認待ち（ネイティブ実装待ち） |
| `android.send_notification` | あり | 要 | ✗ | `tools/list` 表示・呼び出しは人間承認待ち（MCP-1 実装済み・未適用） |
| `filesystem.write` | あり | 要 | ✗ | `tools/list` 表示・呼び出しは人間承認待ち（MCP-1 実装済み・未適用） |
| `shell.execute` | あり | 要 | ✗ | `tools/list` 表示・呼び出しは人間承認待ち（MCP-1 実装済み・未適用） |
| `git.commit` | あり | 要 | ✗ | `tools/list` 表示・呼び出しは人間承認待ち（MCP-1 実装済み・未適用） |
| `github.create_pr` | あり | 要 | ✗ | `tools/list` 表示・呼び出しは人間承認待ち（MCP-1 実装済み・未適用） |

## 設定上のドリフト（要整理・継続）

1. **allowlistの`sandbox_type`は無視される** → **解消（2026-11-15）**
   - 旧状態: `AllowedTool`構造体に`sandbox_type`が無く、TOMLの値は読まれない死んだ設定だった。
   - 2026-11-15: ADR-0002 の `opencode` CLI sandbox fallback 撤去に伴い、repo の
     `mcp_allowlist.toml` から per-tool `sandbox_type` キーを削除（MCP-3、option A）。
     gateway 全体設定 `JARVIS_MCP_GW_SANDBOX_TYPE` は互換性のため解析のみに残存し、
     実行経路には影響しない。`/etc/jarvis/mcp_allowlist.toml` への同期は次回デプロイ時。
2. **未実装ツールのallowlistエントリ** → **挙動変更（2026-11-16）**
   - 旧状態: ネイティブ未実装ツールは `requires_approval = true` で `tools/list` から非表示だった。
   - MCP-1 承認フロー実装後は承認必須ツールも `tools/list` に表示され、
     呼び出し時に人間承認を待つ。未実装ツールは承認後に
     `no native handler ... register an upstream MCP server` で fail-closed となる
     （安全だが UX 上のデッドエンド）。
3. **`web.fetch`の承認**
   - 2026-11-10 に `requires_approval = true` を配備済み。承認フロー実装後に扱いを再検討。
   - 2026-11-15: 承認フロー実装済み（MCP-1）。`requires_approval = true` のままでも
     呼び出しごとに人間承認を待つため実行可能。宛先ドメイン制限（egress allowlist）は `jarvis-mcp-gw` に実装済み。

## デプロイ前チェックリスト

- [x] `web.fetch` の `requires_approval = true` 適用（2026-11-10 配備）
- [x] 未実装ツールの扱いを決定（2026-11-16: 承認フロー経由・fail-closed）
- [x] `sandbox_type`キーの整理方針を決定（2026-11-15: per-tool キー削除・ADR-0002 fallback 撤去）
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

## 2026-11-15 現状確認: systemd hardening との不整合を検出（検出時の記録）

> 注: 以下は修正前に検出した状態です。修正内容と適用後の実測は「修正（2026-11-15 デプロイ適用済み）」および「デプロイ結果（2026-11-15 11:36、実測）」を参照してください。

稼働中サービス（systemd 配下）に対する実測で、当時は **`tools/list` に出る全ツールが実行不能** であることが判明した。

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

> 注: 以下は 2026-11-15 時点の対応記録（履歴）。現行は承認必須ツールも `tools/list` に
> 表示され、呼び出し時に人間承認を待つ（上記「実装状況」の注記を参照）。

- `infra/systemd/jarvis-mcp-gw.service`
  - `ProtectHome=yes` → `ProtectHome=read-only`（`/home` を読み取り専用で参照可能に）
  - `JARVIS_MCP_GW_BASE_DIR` を明示（読み取り境界を unit 上で明確化）。
     unit のフォールバック値は
     `/home/koppe/ドキュメント/作業用/jarvis/jarvis`
     （`infra/systemd/jarvis-mcp-gw.service` の `Environment=` 行、実機は上書き値を優先）。
  - デプロイ実態に合わせて `User`/`Group` を `koppe` に同期
- `infra/config/mcp_allowlist.toml`
  - ネイティブ未実装の `web.search` / `homeassistant.get_state` / `android.get_device_info`
    を `requires_approval = true` に変更
    （**当時は `tools/list` から非表示**。現行は表示＋呼び出し時承認）
- `infra/scripts/smoke-mcp-gw.sh`
  - `tools/call`（filesystem.list / git.status）の成功を検証
  - 承認待ち・未実装ツールが `tools/list` に出ないことを検証
    （**当時のスモークテスト条件**。現行は表示＋呼び出し時承認を検証）
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

- **解消済み（2026-11-15/16）**: 承認フローは実装済み（MCP-1 + GPUI コンソール）。
  基準ディレクトリも `/home/koppe` から repo ルートへ絞り込み済み（MCP-4）。
  ただし両者とも repo 側の変更であり、**実機 `/etc` への反映は次回デプロイ適用時**。
- 未実装ツールは承認を通過しても `no native handler` で fail-closed となる
  （MCP-2）。外部サービス接続・資格情報・`ReadWritePaths` の判断待ち。
- **解消済み**: `web.fetch` の宛先ドメイン制限（egress allowlist）は実装済み。
  TOML の `egress_allowlist` または `JARVIS_MCP_GW_EGRESS_ALLOWLIST` で
  ホストパターンを設定し、`sandbox.rs` の `EgressAllowlist::from_config` が
  `parse_egress_allowlist` 経由で検証する。未設定・空・不正値は `DenyAll`
  となり全宛先を拒否する（fail-closed）。検証済みホストパターンのみ起動時に
  ログへ出力し、資格情報を含む設定値は出力しない。

## 全体タスク一覧

MCP 以外を含むプロジェクト全体の残タスク・判断待ち事項は [remaining-tasks.md](remaining-tasks.md) を参照。

## 現状（2026-11-18・repo 側 allowlist）

上記の 2026-11-15 実測ログは当時の記録として一切変更せず保持する。
以下は [repo 側 allowlist](../../infra/config/mcp_allowlist.toml) の定義を
読み取り専用で確認した現在の状態であり、**実機の実測値ではない**。
設定値・資格情報・秘密情報の値は記載しない。

### repo allowlist のツール名（14件・名前のみ）

- `filesystem.read` / `filesystem.write` / `filesystem.list`
- `shell.execute`
- `git.status` / `git.diff` / `git.commit`
- `github.create_pr`
- `homeassistant.get_state` / `homeassistant.call_service`
- `android.get_device_info` / `android.send_notification`
- `web.search` / `web.fetch`

### 差分判定の方針

- **実機の `/mcp` `tools/list` が正**。repo allowlist は管理上の定義であり、
  実機の状態を上書きする根拠にはしない。
- repo と実機の差分は
  [check-deploy-drift.sh](../../infra/scripts/check-deploy-drift.sh) で検出する。
  allowlist ファイルの `cmp` と、稼働サービスへの `tools/list`
  （読み取り系4ツールの部分集合）／`tools/call` 実測を実施。
- 2026-11-15 の「4ツールのみ」「6ツール」は実測時点の記録であり、
  現在の allowlist 定義（14件）と混同しない。実機の一覧は
  デプロイ適用と事後 E2E の結果で確認する。
