---
type: Design-Notes
title: "バックアップ再設計メモ（BK-1, ADR-0004 対応）"
description: "OCI 非使用のローカル運用前提で、バックアップ保存先・構成・既存レガシー資産の差分を整理するメモ。"
tags:
  - jarvis
  - backup
  - adr-0004
  - operations
generated:
  by: "dsh (iceman-contributor-free)"
  at: "2026-11-15T13:10:00+09:00"
status: draft
---

## 背景（根拠）

- [adr/0004-local-only-no-oci.md](../../adr/0004-local-only-no-oci.md): 実行基盤はローカル Kubuntu + systemd。OCI は使用しない。バックアップ保存先の再決定は未解決。
- 2026-11-15 時点の実機確認（ADR-0004 記載）: `jarvis-backup.timer` 未導入、rclone 設定なし、バックアップジョブ未稼働。
- 現行の主要 jarvis データ/設定（運用上の想定対象）:
  - PostgreSQL（監査ログ等、サービス構成依存）
  - Qdrant（ベクトル記憶、`/var/lib/qdrant` 等は実機構成に依存）
  - `/etc/jarvis` 配下設定
  - リポジトリ/ワークスペース状態（必要範囲の定義が必要）
- 既存レガシー資産（OCI/旧パス/旧認証前提）:
  - [infra/systemd/jarvis-backup.service](../../infra/systemd/jarvis-backup.service): `User=jarvis`, `/opt/jarvis/...`, `rust-script backup.rs`, `/run/rclone`, `/etc/jarvis/op.env` 前提
  - [infra/systemd/jarvis-backup.timer](../../infra/systemd/jarvis-backup.timer): daily timer（unit 内容の現行化が必要）
  - [infra/scripts/backup.sh](../../infra/scripts/backup.sh): `rust/scripts/backup.rs` への互換ランチャー
  - [rust/scripts/backup.rs](../../rust/scripts/backup.rs), [rust/scripts/restore.rs](../../rust/scripts/restore.rs): OCI Object Storage + rclone + `op inject`/`OP_SERVICE_ACCOUNT_TOKEN` 等の前提を含むレガシー実装（要再設計）
  - [infra/README.md](../../infra/README.md): 「Daily backups run via `jarvis-backup.timer` … OCI Object Storage」等の記述が残る（現行実態と乖離）

## 決めるべき事項（BK-1）

1. **保存先**
   - 候補例: ローカル別ディスク、外付け SSD/HDD、NAS（SMB/NFS/rsync）、将来の別ホスト等
   - 非目標（現時点）: OCI Object Storage（ADR-0004）
   - 制約: 可用性、オフライン時挙動、容量、ネットワーク依存、マウント管理
2. **対象データと優先度**
   - DB dump（PostgreSQL）、Qdrant snapshot、`/etc/jarvis`、必要なアプリデータ/ログの範囲
   - 「復旧に必須」/「あると便利」を分離し、RPO/RTO の目安を決める
3. **認証・秘密情報**
   - `op` 依存を維持するか、`bws`/ローカル env/別手段へ統一するか
   - バックアップ先アクセス資格情報の保管・ローテーション方針
4. **暗号化と保全**
   - 転送時/保存時暗号化、鍵管理、世代管理（保持期間・世代数）、整合性検証（checksum/restore test）
5. **実行方式と運用**
   - systemd timer 継続可否、実行ユーザー/権限、`ReadWritePaths`/`Protect*` 設定の現行化
   - 失敗通知（journal/監視連携）、手動実行手順、復旧演習（dry-run/restore test）頻度
6. **復旧手順**
   - 「最新」選択の定義、部分復旧（設定のみ/DBのみ）可否、破壊的復旧の確認手順
   - 依存サービス停止/再開順序と検証項目（health endpoint 等）

## 実装更新の論点（BK-1/BK-2/SEC-2 連携）

- 旧パス（`/opt/jarvis`, `/home/jarvis`, `/run/rclone`）と旧ユーザー（`jarvis`）前提を、現行ホスト/運用ユーザー/配置に合わせて見直す。
- rclone + OCI 固有設定を、選定した保存先に応じた転送実装へ置換する。
- 認証/秘密管理の依存を見直す（2026-11-15 repo 確認）:
  - [infra/systemd/jarvis-backup.service](../../infra/systemd/jarvis-backup.service): `/etc/jarvis/op.env`（`OP_SERVICE_ACCOUNT_TOKEN` 想定の注記あり）を参照
  - [rust/scripts/backup.rs](../../rust/scripts/backup.rs), [rust/scripts/restore.rs](../../rust/scripts/restore.rs): `op inject` 呼び出しを含む
  - [infra/config/policy.env](../../infra/config/policy.env), [context.env](../../infra/config/context.env), [event.env](../../infra/config/event.env), [otel.env](../../infra/config/otel.env): 参照テンプレートに `op://` 記述が残る（BK-4）
  - 現行 core services は bws 系運用の記載があり、バックアップ系の最終方式は BK-1/SEC-2 で整合判断が必要（実機 secret 内容は未確認）
- `jarvis-backup.service`/`.timer` は現行化後にローカル導入し、`systemctl enable --now` と復旧テストを行う（実機変更のため別判断・手順が必要）。
- [infra/README.md](../../infra/README.md) のバックアップ/復旧/更新手順は現行実態に合わせて更新が必要（ドキュメントドリフト）。

## 安全に進めるための次ステップ（提案）

1. 保存先候補と RPO/RTO/保持ポリシーを決定する（人間判断を含む）。
2. 対象データ棚卸し（DB/Qdrant/設定/必要ファイル）と必要権限を確定する。
3. 非破壊なバックアップ実装（ローカル出力先）を先に作り、`--dry-run` 相当と整合性確認を整備する。
4. 復旧テスト手順を文書化し、テスト環境または隔離先で検証してから timer を有効化する。
5. 実機導入時は systemd/権限/秘密情報の変更を伴うため、変更差分とロールバック手順を明確にして実施する。
