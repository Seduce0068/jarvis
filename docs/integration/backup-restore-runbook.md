---
type: Playbook
title: "JARVIS ローカルバックアップ復旧 Runbook"
description: "ローカル専用バックアップ（ADR-0004/BK-1）の復旧手順。破壊的操作を含むため、隔離先での検証を前提とする。"
tags:
  - jarvis
  - backup
  - restore
  - runbook
status: draft
---

## 概要

[backup-local.sh](../../infra/scripts/backup-local.sh) が作成した
ローカルバックアップを復旧する手順。復元対象は以下の3種類。

| 成果物 | 復元先 | 破壊性 |
| --- | --- | --- |
| `postgres-jarvis.sql.gz` | PostgreSQL `jarvis` DB | 高（上書き） |
| `qdrant-snapshot.bin` | Qdrant `jarvis_memories` | 高（上書き） |
| `config-*` / `config-policy.tar` | `/etc/jarvis` | 中（上書き） |
| `manifest.txt` | — | なし（検証用） |

## 安全原則

1. **まず検証**: 復元前に `make verify-backup` で SHA-256 と
   gzip/tar フォーマットを確認する（読み取り専用）。
2. **隔離先へ復元**: 本番 DB / Qdrant / `/etc/jarvis` を直接上書きしない。
   別名 DB・別コレクション・一時ディレクトリへ復元して検証する。
3. **サービス停止を検討**: 本番へ反映する場合は関連サービスを停止し、
   復元後に health check を実施する。
4. **秘密情報は別途**: バックアップに `bws.env` は含まれない。
   復元後に `bws.env` が無い場合はサービスが起動しないため、
   管理者が安全な経路で再配置する。

## バックアップsetの選択

```bash
# 一覧（postgres ユーザーで実行）
sudo -u postgres bash /opt/jarvis/scripts/verify-backup.sh
```

最新 set のパスを確認し、以降の手順で `$SET` に設定する。

```bash
SET="/var/lib/postgresql/jarvis-backups/<host>-<timestamp>"
```

## 手順1: PostgreSQL を隔離 DB へ復元（非破壊）

独自 DB 名（例: `jarvis_restore_test`）へ復元し、本番 `jarvis` は触らない。

```bash
# 新しい空 DB を作成（既存があれば失敗させ、上書きを避ける）
sudo -u postgres createdb jarvis_restore_test

# plain SQL dump を復元
sudo -u postgres sh -c "gzip -cd '$SET/postgres-jarvis.sql.gz' \
  | psql -v ON_ERROR_STOP=1 -d jarvis_restore_test"

# テーブル数を確認（値は出力しない）
sudo -u postgres psql -tAc \
  "SELECT count(*) FROM information_schema.tables WHERE table_schema='public'" \
  -d jarvis_restore_test
```

検証後、テスト DB を削除する（削除は明示的に実行）。

```bash
sudo -u postgres dropdb jarvis_restore_test
```

## 手順2: Qdrant snapshot を隔離コレクションへ復元（非破壊）

`jarvis_memories` を上書きせず、別名コレクションへ復元する。
Qdrant の snapshot は tar 形式なので、`snapshot_name` を指定して
`PUT /collections/{new}/snapshots/{name}` で復元できる。

Qdrant 公式ドキュメント（<https://qdrant.tech/documentation/snapshots/>）で
確認した手順。**新規コレクションへ復元する場合は `priority=snapshot` が必須**
（既定の `replica` だと既存データが優先され空コレクションになる）。

```bash
# 隔離コレクション名を明示して、アップロードした snapshot から復元
COLLECTION="jarvis_memories_restore_test"
SNAPSHOT="$SET/qdrant-snapshot.bin"

base="http://127.0.0.1:6333/collections/${COLLECTION}"
curl -fsS -X POST \
  "${base}/snapshots/upload?priority=snapshot" \
  -H 'Content-Type:multipart/form-data' \
  -F "snapshot=@${SNAPSHOT}"

# 復元結果を確認（点数は値を出さず件数のみ）
curl -fsS \
  "http://127.0.0.1:6333/collections/${COLLECTION}" \
  | jq '.result.points_count'
```

**注意**: snapshot は作成元と同一 minor 以降の Qdrant でのみ復元可能
（現行 v1.14.1）。検証後は隔離コレクションを削除する。

```bash
curl -fsS -X DELETE "http://127.0.0.1:6333/collections/${COLLECTION}"
```

**現状**: 隔離復元の自動スクリプトは未実装。手動検証が必要
（BK-1 の残課題として追跡）。

## 手順3: 設定ファイルを一時ディレクトリへ展開（非破壊）

```bash
TMP="$(mktemp -d)"
tar -xf "$SET/config-policy.tar" -C "$TMP"
cp "$SET"/config-*.toml "$TMP"/ 2>/dev/null || true
cp "$SET"/config-*.yaml "$TMP"/ 2>/dev/null || true

# 内容差分を確認（上書きしない）
diff -rq /etc/jarvis/policy "$TMP/policy" || true
rm -rf "$TMP"
```

## 本番反映（要判断・破壊的）

ここから先は破壊的操作を含むため、実行前に必ず以下を確認する。

- [ ] `make verify-backup` が fail=0
- [ ] 隔離先での復元検証が成功
- [ ] 関連サービス（`jarvis-context` 等）の停止予定を関係者と共有
- [ ] 旧データの退避先を確保
- [ ] `bws.env` の再配置方法を確認（バックアップには含まれない）

本番 `jarvis` DB への上書きは、`jarvis_restore_test` での検証が成功した
場合のみ、明示的に `psql -d jarvis` を実行して行う。

## 未実装・残課題

- Qdrant 隔離復元の自動スクリプト
- 本番反映の自動化（意図的に未実装: 破壊的操作は手動確認のまま）
- restore 演習の定期実施（`backup-redesign-notes.md` の安全ゲート参照）
