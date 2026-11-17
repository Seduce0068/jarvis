---
type: Playbook
title: "JARVIS ローカル運用 Runbook"
description: "ローカル systemd サービスの起動・検証・停止、手動変更の
  ロールバック、バックアップ検証と clippy 表記の運用方針。"
tags:
  - jarvis
  - operations
  - systemd
  - runbook
status: draft
---

## 概要

OCI は使用しない（[ADR-0004](../../adr/0004-local-only-no-oci.md)）。
JARVIS のコアサービスはローカルの Kubuntu 上で systemd unit として稼働し、
health エンドポイントは `127.0.0.1` にのみバインドする（外部アクセスは
Tailscale 経由）。

このランブックは日次の起動・検証・停止と、手動変更を戻す手順をまとめる。
バックアップの復旧そのものは
[backup-restore-runbook.md](backup-restore-runbook.md) を正本とする。
秘密情報（トークン、鍵、プロジェクト ID 等）や実機固有の設定値は
この文書に記載しない。

## サービス構成と依存関係

| サービス | health ポート | 起動時の依存 |
| --- | --- | --- |
| `jarvis-policy` | 8081 | `postgresql`, `qdrant` |
| `jarvis-mcp-gw` | 8082 | `jarvis-policy` |
| `jarvis-event` | 8083 | `jarvis-policy`, `jarvis-mcp-gw`, `jarvis-context` |
| `jarvis-context` | 8084 | `postgresql`, `qdrant`, `jarvis-policy` |

`qdrant` / `postgresql` は共有の基盤サービスなので、コア4サービスの
停止・再起動時に毎回止める必要はない。

## 起動 → 検証 → 停止

### 起動

依存関係の順（基盤 → policy → gateway/context → event）で起動する。

```bash
echo "== 1. 基盤サービス =="
sudo systemctl start postgresql.service qdrant.service

echo "== 2. policy =="
sudo systemctl start jarvis-policy.service

echo "== 3. gateway / context =="
sudo systemctl start jarvis-mcp-gw.service jarvis-context.service

echo "== 4. event =="
sudo systemctl start jarvis-event.service
```

恒久的に自動起動させたい場合は `sudo systemctl enable --now <unit>` を使う。
既に `enabled` の unit に `enable` を再度実行する必要はない。

### 検証

`is-active` と health の両方を確認する。health が 200 でも dependency の
不整合はログに出るため、直近のジャーナルも確認する。

```bash
echo "== 状態 =="
systemctl is-active jarvis-policy jarvis-mcp-gw jarvis-event jarvis-context

echo "== health (期待値: すべて 200) =="
for port in 8081 8082 8083 8084; do
  code=$(curl -q -s -o /dev/null -w '%{http_code}' -m 2 \
    "http://127.0.0.1:${port}/health" || true)
  echo "port=${port} http=${code}"
done

echo "== 直近ログ =="
journalctl -u jarvis-policy -u jarvis-mcp-gw -u jarvis-event \
  -u jarvis-context -n 50 --no-pager
```

合格条件: 4サービスすべて `active`、4ポートすべて `http=200`、
ジャーナルに起動失敗や fail-closed による連続拒否がないこと。

### 停止

依存関係の逆順（event → gateway/context → policy）で停止する。
`jarvis-policy` を先に止めると承認フローが fail-closed になるため、
gateway を先に停止する順序を守る。

```bash
echo "== 1. event =="
sudo systemctl stop jarvis-event.service

echo "== 2. gateway / context =="
sudo systemctl stop jarvis-mcp-gw.service jarvis-context.service

echo "== 3. policy =="
sudo systemctl stop jarvis-policy.service
```

基盤サービスまで止める場合は、上記の後に明示的に実行する（他サービスへも
影響するため、通常は止めない）。

```bash
echo "== 4. 基盤（必要な場合のみ） =="
sudo systemctl stop qdrant.service postgresql.service
```

`stop` は一時停止であり、再起動時には自動起動する。再起動後も停止状態を
維持したい場合は `sudo systemctl disable --now <unit>` で無効化する
（恒久設定の変更なので対象 unit を限定する）。

バックアップ timer はコアサービスとは独立して運用する。

```bash
echo "== timer 状態（読み取り専用） =="
make timer-status

echo "== timer の一時停止 / 恒久停止（要判断） =="
sudo systemctl stop jarvis-backup.timer
sudo systemctl disable --now jarvis-backup.timer
```

## 手動変更のロールバック

### 原則

1. 変更前に退避コピーを作る（時刻付きの `.bak-<timestamp>`）。
2. unit を触る場合は変更前に `systemd-analyze verify` で構文を検証する。
3. 変更後は `is-active` と health で必ず事後検証する。
4. 失敗時は退避ファイルを戻し、`daemon-reload` → `restart` → 再検証する。

### unit / allowlist / バイナリの復元

```bash
TS="$(date +%Y%m%d-%H%M%S)"
UNIT="/etc/systemd/system/jarvis-mcp-gw.service"

# 変更前の退避
sudo cp -a "$UNIT" "$UNIT.bak-$TS"
systemd-analyze verify "$UNIT"

# 失敗時に戻す（権限・所有者を指定して install）
sudo install -o root -g root -m 0644 "$UNIT.bak-$TS" "$UNIT"
sudo systemctl daemon-reload
sudo systemctl restart jarvis-mcp-gw

# 再検証
systemctl is-active jarvis-mcp-gw
curl -q -s -o /dev/null -w '%{http_code}\n' -m 2 \
  http://127.0.0.1:8082/health
```

自動スクリプトは同じ形式の退避ファイルを作り、失敗時に自動ロールバックする。

- `infra/scripts/deploy-mcp-gw-safe.sh`
  - unit / allowlist: `.bak-<timestamp>`
  - バイナリ: `.prev-<timestamp>`
  - 再起動・事後 E2E 失敗時に自動で復元する。
- `infra/scripts/install-backup-timer.sh`
  - unit / timer / script: `.bak-<timestamp>`
  - ERR trap で自動で復元する。

自動スクリプトが途中失敗した場合、まず
`ls -1 /etc/systemd/system/*.bak-* /opt/jarvis/bin/*.prev-*` で退避ファイルを
探し、上記と同じ手順で手動復元する。スクリプト自身の `rollback()` が
走っている場合は、二重に戻さずログと最終状態を先に確認する。

### データのロールバック

PostgreSQL の DB、Qdrant のコレクション、`/etc/jarvis` の設定を上書きした
場合は、直接の再上書きを行わず
[backup-restore-runbook.md](backup-restore-runbook.md) の隔離先復元手順に
従って検証してから本番反映を判断する。

## make verify-backup の扱い方針

### 矛盾の現状

`Makefile` の `verify-backup` ターゲットのヘルプ文は「読み取り専用」だが、
実際は `infra/scripts/verify-backup.sh` を `/opt/jarvis/scripts/` へ
`install` してから実行する。つまり **スクリプト同期（書き込み）が
含まれており、読み取り専用ではない**。

スクリプト本体 [verify-backup.sh](../../infra/scripts/verify-backup.sh) は
バックアップデータを変更しない（SHA-256 と gzip/tar のフォーマットを検査
するだけ）。書き込みは Makefile 側の同期処理に起因する。

### 運用方針

1. `make verify-backup` は「最新スクリプトの同期 + 整合性検証」として扱う。
   `/opt/jarvis/scripts/` への書き込みが発生するため、権限と所有権を確認
   してから実行する。
2. 真に読み取り専用の検証は、作業コピーのスクリプトを直接実行する。

   ```bash
   sudo -u postgres bash infra/scripts/verify-backup.sh --latest
   ```

3. Makefile の改善案（このランブックでは実装しない。lead の判断事項）:
   - `verify-backup`（同期込み）と `verify-backup-readonly`（同期なし）を
     分離する。
   - 同期を残す場合はヘルプ文を「読み取り専用ではない」と訂正し、
     同期を `sudo install` と明示する。

## clippy 表記の統一

clippy ゲートは次の3経路に限定して表記する。

- 推奨（ゲート一式）: `make ci-local`
  - 対象: Rust / UI / secret scan / safety guards
- Rust 直接（Rust workspace）:

  ```bash
  cd rust && cargo clippy --all-targets --locked -- \
    -D warnings -D clippy::undocumented_unsafe_blocks
  ```

- UI 直接（GPUI 承認コンソール）:

  ```bash
  cd ui && cargo clippy --all-targets -- -D warnings
  ```

表記ルール:

- `cargo clippy --workspace -- -D warnings` 単独を `make ci-local` の
  代替として書かない。Makefile の `ci-local` とオプションが異なり、
  `-D clippy::undocumented_unsafe_blocks` が抜けるため。
- 文中では「clippy ゲートは `make ci-local` に含まれる」と書き、直接実行例
  は上記のコマンドをそのまま使う。
- UI は workspace 外の独立クレートなので、`make ui-test` または
  `cd ui && ...` で別途実行する（`make ci-local` には含まれる）。

## 関連

- [ADR-0004: ローカル運用（OCI 非使用）](../../adr/0004-local-only-no-oci.md)
- [backup-restore-runbook.md](backup-restore-runbook.md): 復旧手順の正本
- [backup-redesign-notes.md](backup-redesign-notes.md): バックアップ設計と安全ゲート
- [remaining-tasks.md](remaining-tasks.md): 残タスク・判断待ち事項
- [AGENTS.md](../../AGENTS.md): プロジェクト固有の開発・運用ルール
