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

## レガシー資産の前提棚卸し（2026-11-15 read-only 確認）

> 実装置換時の確認観点。secret 値・実機上の有無/内容は未確認。

- [infra/scripts/backup.sh](../../infra/scripts/backup.sh): 互換ランチャー。`rust-script "$REPO_ROOT/rust/scripts/backup.rs"` を実行する（`rust-script` 前提）。
- [infra/systemd/jarvis-backup.service](../../infra/systemd/jarvis-backup.service): `Type=oneshot`, `User=jarvis`, `EnvironmentFile=/etc/jarvis/op.env`, `ExecStart=/usr/bin/rust-script /opt/jarvis/scripts/backup.rs`, `WorkingDirectory=/opt/jarvis/scripts`, `ReadWritePaths=/opt/jarvis /home/jarvis/.cache/rust-script`, `RuntimeDirectory=rclone`。コメントに `OP_SERVICE_ACCOUNT_TOKEN` 供給前提あり。
- [infra/systemd/jarvis-backup.timer](../../infra/systemd/jarvis-backup.timer): `OnCalendar=daily`, `Persistent=true`, `RandomizedDelaySec=1h`, `WantedBy=timers.target`（2026-11-15 時点ローカル未導入）。
- [rust/scripts/backup.rs](../../rust/scripts/backup.rs): 既定値 `--remote oci`, `--bucket jarvis-backups`, `--backup-dir /opt/jarvis/backups`。`op inject` で rclone 設定を `/run/rclone/rclone.conf` に生成し、`rclone` で `oci:<bucket>/<hostname>/<date>/...` へアップロード/世代削除する前提（OCI Object Storage）。PostgreSQL/Qdrant/`/etc/jarvis`/リポジトリ等の取得・配置前提も旧環境依存の可能性あり（詳細は実装時に再確認）。
- [rust/scripts/restore.rs](../../rust/scripts/restore.rs): 同様に `oci`/`jarvis-backups`/`/opt/jarvis/backups` 既定、`op inject` + rclone でリモートから取得し restore する前提（破壊的操作を含みうるため再設計・隔離検証が必須）。

**置換時の主な着眼点**: 実行ユーザー/パス配置、`rust-script`・`rclone`・`op` 依存の要否、保存先 URI/転送方式、資格情報供給（`bws`/`op`/マウント認証等）、対象データ取得コマンドと権限、dry-run/整合性検証、失敗通知、restore の安全手順（確認・隔離先・ロールバック困難性）。

## 安全に進めるための次ステップ（提案）

1. 保存先候補と RPO/RTO/保持ポリシーを決定する（人間判断を含む）。
2. 対象データ棚卸し（DB/Qdrant/設定/必要ファイル）と必要権限を確定する。
3. 非破壊なバックアップ実装（ローカル出力先）を先に作り、`--dry-run` 相当と整合性確認を整備する。
4. 復旧テスト手順を文書化し、テスト環境または隔離先で検証してから timer を有効化する。
5. 実機導入時は systemd/権限/秘密情報の変更を伴うため、変更差分とロールバック手順を明確にして実施する。

## 追加確認項目（2026-11-15、設計前チェックリスト）

> 実機の実在パス・容量・権限・稼働状態は環境依存のため、実装/導入前に read-only で再確認する（秘密情報の内容は確認しない）。

### BK-1: 対象データ・所在・復旧単位の棚卸し

- **設定**: `/etc/jarvis`（実体、所有者/パーミッション、必要ファイルの最小集合）。repo の [infra/config](../../infra/config) はテンプレート/参照であり実機設定と同一視しない。
- **DB**: PostgreSQL のデータディレクトリ/接続方式/ダンプに必要な権限・実行ユーザー、復旧時の整合性（停止順序、既存データ上書きの扱い）。
- **ベクトルDB**: Qdrant のデータ所在・snapshot/backup 手段・サービス停止要否、整合性確認方法。
- **アプリ/状態**: ログ、キャッシュ、`jarvis` サービスが参照する作業データ、復旧に不要な一時データの除外基準。
- **復旧単位**: 「設定のみ」「DBのみ」「Qdrantのみ」「全体」の可否と、部分復旧時の依存関係・検証手順（health endpoint、`systemctl status` 等）。
- **保持/検証**: 世代数・保持期間、チェックサム/マニフェスト、定期 restore test の実施場所（本番上書きを避ける隔離先）と成功判定。

### BK-2: systemd unit/timer 現行化時の確認項目（導入は別判断）

- **実行主体とパス**: 現行 unit は `User=jarvis`・`/opt/jarvis`・`/home/jarvis/.cache/rust-script`・`/run/rclone` 前提。現行ホストの配置/実行ユーザー/バイナリ・スクリプト配置に合わせて `User/Group`, `ExecStart`, `WorkingDirectory`, `ReadWritePaths`, `RuntimeDirectory` を再設計する。
- **認証/秘密管理**: 現行 unit は `/etc/jarvis/op.env`（`OP_SERVICE_ACCOUNT_TOKEN` 想定）参照、既存スクリプトは `op inject`/rclone 前提（SEC-2/BK-4 と連携）。採用する保存先・認証方式（bws/op/ローカル資格情報/マウント資格情報等）に合わせ、秘密情報の供給経路・最小権限・ローテーションを設計する（実機 secret の内容確認/変更は別判断）。
- **転送方式**: rclone + OCI Object Storage 前提を、選定した保存先（ローカル別ディスク/NAS/別ストレージ等）に応じた実装へ置換する。オフライン/マウント未接続時の失敗挙動と通知を明確化する。
- **hardening**: `ProtectSystem=strict`, `ProtectHome`, `PrivateTmp`, `NoNewPrivileges` 等を維持しつつ、バックアップ/restore に必要な最小の read/write パスのみ許可する（過剰権限を避ける）。
- **有効化前**: 手動実行で非破壊検証（dry-run/出力先確認/整合性確認）→ restore test（隔離先）→ `systemctl enable --now`、失敗時の停止/ロールバック手順を用意する。

### 実装・導入の安全ゲート（提案）

- **実装着手前に固定する**: 保存先・認証/秘密管理方式・対象データ・保持/世代・暗号化・復旧単位（RPO/RTO 目安）を決め、`--dry-run` 相当、整合性検証（checksum/manifest 等）、失敗時の終了コード/ログ/通知方針を設計に含める。
- **復旧可能性を先に証明する**: 本番データや実機サービスへ影響しない隔離先で restore test を行い、手順・所要時間・成功判定・部分復旧の制約を文書化してから定期実行を検討する。
- **権限/秘密情報の最小化**: 実行ユーザー・読み書き先・資格情報供給経路は最小権限とし、secret の値を repo/ログ/ドキュメントへ残さない。`bws`/`op` 等の方式変更や `/etc/jarvis` 配下更新は別判断・手順で行う。
- **実機変更は段階的に**: repo 変更だけで完了とみなさず、差分レビュー → 非破壊検証 → 必要な承認/権限取得 → `systemctl daemon-reload` 等の導入 → 手動実行確認 → timer 有効化の順で進める。`Persistent=true` による追従実行やオフマウント時挙動を事前に確認する。
- **未確定事項はブロッカーとして扱う**: 保存先未決定、認証方式未整合、restore 手順未検証、実機パス/権限未確認の状態で定期バックアップを有効化しない。

### 保存先候補の比較観点（BK-1、選定前チェックリスト）

- **可用性/到達性**: 常時接続か（内蔵/常時マウント）間欠接続か（外付け/NAS/別ホスト）。マウント未接続・認証失敗・ネットワーク断時の検知方法、バックアップ失敗時の再試行/次回実行方針、`Persistent=true` 追従との整合を確認する。
- **容量/保持**: 想定データ量（PostgreSQL dump、Qdrant snapshot、`/etc/jarvis`、必要ログ等）と世代数から必要容量を見積もり、枯渇時の挙動・監視・削除ポリシーを定義する。
- **性能/所要時間**: バックアップ/転送/restore の所要時間を測定し、RPO/RTO 目安とサービス停止・負荷への影響を評価する（初回は隔離先で測定）。
- **権限分離/保護**: バックアップ実行主体が必要最小限の読み取り元にアクセスでき、保存先では不要な書き換え・削除・横展開を抑制できるか（専用ユーザー/マウントオプション/書込専用世代化等）を確認する。暗号化鍵・資格情報の保管場所とローテーションも含める。
- **整合性/復旧検証**: checksum/manifest の保管、世代の選択方法、部分復旧/全体復旧の手順、隔離先での定期 restore test の実施可否を比較条件に含める。
- **運用コスト**: 監視・通知・障害対応・定期検証に必要な手順が、現行ローカル運用（systemd + `bws`/必要に応じた認証方式）と無理なく両立するかを確認する。

## 2026-11-16 追加: 読み取り専用プリフライト（安全ゲート実装）

[bak-preflight](../../infra/scripts/backup-preflight.sh)（`make backup-preflight`）を追加。
保存先・認証方式の決定に先立ち、秘密情報を一切読まずに前提を棚卸しする。

実機実行結果（2026-11-16、値は出力していない）:

| 項目 | 結果 |
| ---- | ---- |
| `postgresql.service` / `qdrant.service` | ともに active（バックアップ対象は存在） |
| `pg_dump` / `pg_dumpall` / `tar` / `sha256sum` | すべて PATH 上に存在 |
| `rclone` | 不在（ADR-0004 で OCI は非使用のため想定どおり） |
| `/etc/jarvis` | 存在。非 secret 設定5ファイル、secret 風ファイル1ファイル（`bws.env`） |
| repo の `op://` 参照テンプレート | 4 files（BK-4 判断待ち） |
| `jarvis-backup.timer` | 未導入（想定どおり。設計確定前に有効化しない） |

**判定**: fail=0（前提ツールは揃っている）、warn=2（secret ファイルの暗号化方針と
`op://` テンプレートの扱いは BK-4/SEC-2 の判断待ち）。
このスクリプトは読み取りのみで、保存先の選択やデータ取得は行わない。

### 仍未解決（ブロッカー）

- **保存先**: 未決定（ローカル別ディスク / 外付け / NAS 等）。ADR-0004 で OCI は除外。
- **認証/暗号化**: `bws.env` を含む `/etc/jarvis` をどう暗号化して保管するか未決定。
- **復旧検証**: 隔離先での restore test 手順が未確立。
- 上記が揃うまで定期バックアップ（timer）は有効化しない（既存の安全ゲートを維持）。

### `systemd-analyze verify` 的结果（2026-11-16 追記）

`systemd-analyze verify infra/systemd/*.service infra/systemd/*.timer` を
読み取り専用で実行した結果、他の unit は問題なしだが
`jarvis-backup.service` のみ以下を検出:

```text
jarvis-backup.service: Command /usr/local/bin/rust-script is not executable
```

実機の `rust-script` は `/home/koppe/.cargo/bin/rust-script` に存在し
`/usr/local/bin` には無い。ただしこの unit は `User=jarvis` /
`/opt/jarvis/scripts` / `/run/rclone` / `op.env` 前提のレガシーであり、
ExecStart パスだけ直しても導入可能にならない。**BK-1 の保存先・認証設計が
確定するまで unit 自体を再設計し、verify を通してから導入する**（現状は
未導入を維持）。

### レガシー・ランチャーを fail-closed 化（2026-11-16）

[infra/scripts/backup.sh](../../infra/scripts/backup.sh) と
[infra/scripts/restore.sh](../../infra/scripts/restore.sh) は、従来
`rust/scripts/backup.rs` / `restore.rs` へそのまま委譲していたが、
以下を確認したため **両方とも実行を拒否（exit 2）し、理由と代替手段を
表示する** ガードへ変更した:

- `backup.rs`: 既定 `--remote oci`（rclone）/`op inject`。さらに
  `--dry-run` でも `cleanup_local()` が 7 日超のローカルファイルを
  **削除する**（dry-run は remote の upload/cleanup のみをスキップ）。
- `restore.rs`: `--confirm` 指定で設定/PostgreSQL/Qdrant/リポジトリを
  上書きしサービスを再起動する。restore 手順は未検証。
- 両者とも ADR-0004（OCI 非使用）と BK-1 の未決定状態に反する。

参照・調査は `rust-script <script> --help` で直接可能（実装自体は残置）。
`make lint` の `--help` 起動確認も引き続き成功する。

## 2026-11-17: ローカルバックアップの実装と timer 導入準備（BK-1/BK-2）

ユーザーから「timer導入」の明示指示を受け、OCI を使わない前提で実装した。

### 決定（方針）

- **保存先**: `/var/lib/postgresql/jarvis-backups/<host>-<timestamp>/`（ローカルディスク、
  postgres:postgres 0700）。OCI/NAS は使用しない（ADR-0004）。
- **実行主体**: `postgres` OS ユーザー。`pg_dump -d jarvis` が peer 認証で動く。
  レガシー unit の `User=jarvis` は実機にユーザーが存在しないため使用不可。
- **対象**: PostgreSQL `jarvis` DB dump / Qdrant `jarvis_memories` snapshot /
  非 secret 設定 allowlist（mcp_allowlist, pii_patterns, otelcol-config, policy/）。
  **secret（`*.env`, `bws.env`, token）は一切読まない**。
- **完全性**: `manifest.txt` に各ファイルのサイズと SHA-256 を記録。
- **安全装置**: 既定は dry-run、`--apply` で実書き込み、保持削除は `--retain-days` opt-in。

### 実装物

- [infra/scripts/backup-local.sh](../../infra/scripts/backup-local.sh): 本体
  （`set -euo pipefail`、shellcheck クリーン）。
- [infra/systemd/jarvis-backup.service](../../infra/systemd/jarvis-backup.service)
  と [.timer](../../infra/systemd/jarvis-backup.timer) を現行化。
  `systemd-analyze verify` 成功。
- [infra/scripts/install-backup-timer.sh](../../infra/scripts/install-backup-timer.sh):
  導入スクリプト（dry-run 既定、`--apply` は root 必須、rollback 付き）。

### 検証（Runtime Proof）

- `sudo -u postgres ... backup-local.sh --backup-dir ...`（dry-run）成功。
- 実バックアップ1回成功: `postgres-jarvis.sql.gz` (1832 B),
  `qdrant-snapshot.bin` (247296 B), 設定4点, `manifest.txt` 生成。
  secret ファイルは生成物に不在（確認済み）、set の権限 0700。
- `systemd-analyze verify` で unit 2件エラーなし。
- `make install-backup-timer`（dry-run）成功。

### 未完了（要ユーザー操作）

このセッションでは **sudo が対話認証を要求するため特権適用が不可**。
実機導入には以下をユーザーが実行する必要がある:

```bash
cd ~/ドキュメント/作業用/jarvis/jarvis
sudo bash infra/scripts/install-backup-timer.sh --apply
systemctl list-timers jarvis-backup.timer --no-pager
```

restore 手順は [backup-restore-runbook.md](backup-restore-runbook.md) に文書化済み。
隔離先での実復元検証は未実施（BK-1 の残課題）。

### 整合性検証ツール（2026-11-17 追加）

[infra/scripts/verify-backup.sh](../../infra/scripts/verify-backup.sh)（`make verify-backup`）
で、manifest.txt に記録されたサイズと SHA-256 を実ファイルと照合する。
読み取り専用で、破壊操作は一切行わない。

実バックアップ2セットに対して実行し **ok=16 fail=0** を確認（1セット目は
ディレクトリ hash 修正前に作られたため config-policy.tar が manifest に無いが、
記録済みファイルはすべて整合）。SHA-256 照合に加え、gzip/tar の
フォーマット可読性も検査する（**復元は行わず**、構造のみ確認）。
