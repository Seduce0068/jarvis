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
generated:
  by: "DeepSeek Harness (lead agent)"
  at: "2026-11-18T06:34:00+09:00"
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

## P0-10: kill switch 既定トークンの strict ガード

`jarvis-policy` は kill switch の管理トークンが既定値 `"changeme"` のとき、
既定では `tracing::warn!` を出すだけで起動を継続する（
[config.rs:66](../../rust/jarvis-policy/src/config.rs) の既定値、
[lib.rs:35-48](../../rust/jarvis-policy/src/lib.rs) の分岐）。
[config.rs:38](../../rust/jarvis-policy/src/config.rs) の
`require_kill_switch_admin_token`（既定 `false`）を `true` にすると、
同じ状態で起動を拒否する opt-in ガードが有効になる。
本番で `true` にする前に、以下の順でトークン注入を確認する。
秘密情報の値は表示も記録もしないこと。

### 値を見ずにトークン注入を確認する

実行環境: ローカル Kubuntu（3 は root 権限の読み取りを含む）。

```bash
echo "== 1. 注入経路 (ExecStart / EnvironmentFile) =="
systemctl cat jarvis-policy | grep -E '^ExecStart=|^EnvironmentFile='

echo "== 2. bws.env の存在と所有者 (値は読まない) =="
stat -c '%n owner=%U mode=%a' /etc/jarvis/bws.env

echo "== 3. 稼働プロセスの env に変数名があるか (値は出さない) =="
PID="$(systemctl show jarvis-policy -p MainPID --value)"
if [ "${PID}" -gt 0 ] 2>/dev/null && \
    sudo cat "/proc/${PID}/environ" | tr '\0' '\n' | \
      grep -q '^JARVIS_POLICY_KILL_SWITCH_ADMIN_TOKEN='; then
  echo "injected"
else
  echo "not injected"
fi
```

3 が `injected` なら変数名の注入を確認できている。
`not injected` の場合は strict ガードを有効化しない。
`PID=0`（未起動）なら先に通常の起動手順で起動してから再確認する。
値そのものは読まないため、この確認だけでは `"changeme"` のままか判別できない。
次節で `true` にして起動が通れば、注入済みかつ既定値でないことも確認できる。

### strict ガードを有効化する（注入確認後）

トークンの値は書かず、フラグだけを drop-in に設定する。

```bash
sudo systemctl edit jarvis-policy
```

開いたエディタへ次を入力して保存する。

```ini
[Service]
Environment=JARVIS_POLICY_REQUIRE_KILL_SWITCH_ADMIN_TOKEN=true
```

反映して再起動し、検証する。

```bash
echo "== 反映と再起動 =="
sudo systemctl daemon-reload
sudo systemctl restart jarvis-policy

echo "== 事後検証 =="
systemctl is-active jarvis-policy
curl -q -s -o /dev/null -w '%{http_code}\n' -m 2 \
  http://127.0.0.1:8081/health
journalctl -u jarvis-policy -n 20 --no-pager
```

合格条件: `active`、`http=200`、ジャーナルに `changeme` の拒否が無いこと。
変更の退避と失敗時の復元は「手動変更のロールバック」節に従う。

### 起動拒否時に false へ戻す

`journalctl` に `kill switch admin token is still 'changeme' but` と
`require_kill_switch_admin_token = true` が出て起動しない場合は、
注入が未完了か既定値のままなので、ガードを `false` へ戻して復旧する。
トークンの値の確認・変更は行わない。

```bash
echo "== 1. drop-in の退避 =="
DROPIN="/etc/systemd/system/jarvis-policy.service.d/override.conf"
TS="$(date +%Y%m%d-%H%M%S)"
sudo cp -a "$DROPIN" "$DROPIN.bak-$TS"

echo "== 2. エディタで true を false へ書き換える =="
sudo systemctl edit jarvis-policy

echo "== 3. 反映・再起動・再検証 =="
sudo systemctl daemon-reload
sudo systemctl restart jarvis-policy
systemctl is-active jarvis-policy
curl -q -s -o /dev/null -w '%{http_code}\n' -m 2 \
  http://127.0.0.1:8081/health
```

エディタでは次の行だけを `false` に変える（他は触らない）。

```ini
[Service]
Environment=JARVIS_POLICY_REQUIRE_KILL_SWITCH_ADMIN_TOKEN=false
```

`false` へ戻せば既定の warn-only 動作に戻りサービスは起動する。
その後、値を見ずに注入を確認する手順をやり直し、原因を特定してから
再度 `true` 化を判断する。drop-in 自体を削除しても既定 `false` に戻るが、
設定意図を残すため明示的に `false` を書く。

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

### 経緯（旧矛盾、2026-11-18 に解消済み）

従来の `verify-backup` ターゲットはヘルプ文で「読み取り専用」としながら、
`infra/scripts/verify-backup.sh` を `/opt/jarvis/scripts/` へ `install`
してから実行していた（スクリプト同期 = 書き込み）。スクリプト本体
[verify-backup.sh](../../infra/scripts/verify-backup.sh) はバックアップ
データを変更しない読み取り専用実装だったため、書き込みは Makefile 側の
同期処理に起因していた。

### 現行手順（2026-11-18 更新）

- `make verify-backup` は**真に読み取り専用**。作業コピーの
  `infra/scripts/verify-backup.sh` を `sudo -u postgres` で実行し、
  `/opt` への書き込みは行わない。

  ```bash
  make verify-backup
  ```

- `/opt/jarvis/scripts/verify-backup.sh` の同期が必要な場合は、root で
  `make install-verify-backup` を明示的に呼び出す。

  ```bash
  sudo make install-verify-backup
  ```

- 同期と検証が分離されたため、日常的確認は `make verify-backup` のみで
  安全に実施できる（バックアップの読み取りとハッシュ検証のみ）。

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
