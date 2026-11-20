---
type: Design
title: "ローカルバックアップ復元の安全な経路（ADR-0004 準拠）"
description: "backup-local.sh の成果物を入力とし、整合性検証を必須ゲートとし、dry-run 既定・サービス安全停止を備えた復元設計。"
tags:
  - jarvis
  - backup
  - restore
  - adr-0004
  - safety
generated:
  by: "DeepSeek Harness (i5-restore-design)"
  at: "2026-11-19T14:41:24+09:00"
status: "draft"
---

## Context

- [ADR-0004](../../adr/0004-local-only-no-oci.md) は OCI 非使用とローカル systemd 運用を決定した。
  バックアップ保存先の再決定は当時未解決だったが、その後
  [backup-local.sh](../../infra/scripts/backup-local.sh) と
  [verify-backup.sh](../../infra/scripts/verify-backup.sh) によるローカル方式が実装された
  （[backup-redesign-notes.md](backup-redesign-notes.md) の 2026-11-17 節）。
- 現行バックアップは
  `/var/lib/postgresql/jarvis-backups/<host>-<YYYYMMDD-HHMMSS>/` に
  `manifest.txt` / `COMPLETE` / `postgres-jarvis.sql.gz` / `qdrant-snapshot.bin` /
  `config-<basename>` / `git-jarvis-head.tar.gz` を生成する。
- 一方 [restore.rs](../../rust/scripts/restore.rs) は OCI/rclone/`op inject` 前提で、
  ファイル名も `pg-<date>.sql.gz` / `qdrant-<date>.snapshot` /
  `configs-<date>.tar.gz` を想定しており現行セットと互換性がない。
  整合性検証も「ファイルが存在するか」のみで、サイズ/ハッシュ検証を持たない。
- [backup-restore-runbook.md](backup-restore-runbook.md) は隔離先での手動復元を
  文書化している。本設計文書の作成時点では自動スクリプトは未実装だったが、
  その後 [restore-local.sh](../../infra/scripts/restore-local.sh) として
  実装済みである（同文書「現行推奨: restore-local」および
  「未実装・残課題」の注記を参照。本番反映は `--confirm` + ユーザー承認時のみ）。
- 以上から、現行フォーマットを直接入力とし、検証をゲートとし、
  破壊操作を明示的に限定する復元経路が必要である。

## Decision

1. **新規のローカル復元ツールを追加する**（作業名 `restore-local`）。
   [backup-local.sh](../../infra/scripts/backup-local.sh) の成果物を直接入力とし、
   OCI/rclone/`op` への依存を持たない。実装言語は既存の backup/verify と揃えて
   bash を採用する（Rust 案は「代替案」で検討し却下）。
2. **検証ファースト**: 復元処理の前に
   [verify-backup.sh](../../infra/scripts/verify-backup.sh) 相当の検証
   （`manifest.txt` のサイズ/SHA-256、`COMPLETE`、必須コンポーネント、
   フォーマット可読性）を必ず通す。失敗時は一切書き込まない。
3. **三段の安全レベル**: 既定は dry-run（検証と計画のみ）。`--apply` で隔離先への
   非破壊復元を実行し、本番上書きには `--apply` と `--confirm` の両方を要求する。
4. **コンポーネント単位のスキップ**: `--skip-db` / `--skip-qdrant` /
   `--skip-configs` / `--skip-git` を設け、任意の組み合わせで復元できる。
5. **サービスの安全な停止/再起動**: 書き込み前に「利用可能かつ active な」
   systemd ユニットのみを停止し、成功・失敗のどちらでも `trap` で必ず再起動する。
   Qdrant については後述の不変条件を守り、停止後の HTTP upload を禁止する。
6. **既存 `restore.rs` の扱い（正式決定）**: **削除せず、明示的に非推奨のレガシーとして残す**。
   理由は ADR-0004 が OCI 関連資産を「凍結」する方針を取っており、削除は別判断であるため。
   ヘッダに非推奨コメントを付け、[backup-restore-runbook.md](backup-restore-runbook.md) と
   `infra/README.md` から参照を切る。既に `infra/scripts/restore.sh` は
   委譲を拒否する fail-closed ガードへ変更済みであり、実行経路は塞がっている。
   完全削除は将来の独立した決定事項として「未決定事項」に記録する。

## 入力フォーマット

### セットの配置と命名

| 項目 | 値 | 根拠 |
| --- | --- | --- |
| ルート | `/var/lib/postgresql/jarvis-backups` | `backup-local.sh:9` |
| セット名 | `<host>-<YYYYMMDD-HHMMSS>` | `backup-local.sh:92-94` |
| 選択（`--latest`） | `find ... | sort | tail -1`（名前順の末尾） | `verify-backup.sh:26-29` |

### `manifest.txt`

- 1 行目はヘッダ `# created_at=<UTC 時刻>`（レガシーセットでは欠落）。
- 以降は `<name> <bytes> <sha256>` の 3 フィールド（`backup-local.sh:135-144`）。
- 検証では `#` で始まる行と空行をスキップし、記録済み全ファイルのサイズと
  SHA-256 を実ファイルと照合する（`verify-backup.sh:62-79`）。

### コンポーネント

| マニフェスト名 | 内容 | フォーマット検証 | 復元先 |
| --- | --- | --- | --- |
| `postgres-jarvis.sql.gz` | `pg_dump -d jarvis` の plain SQL | `gzip -t` と pg_dump 完了フッタ | PostgreSQL `jarvis` |
| `qdrant-snapshot.bin` | `jarvis_memories` の snapshot（tar 形式） | `tar -tf` | Qdrant `jarvis_memories` |
| `config-<basename>` | 非秘密設定の個別ファイル | マニフェストの size/hash | `/etc/jarvis` 配下の対応パス |
| `git-jarvis-head.tar.gz` | `git archive HEAD`（`.git/` 不含） | `tar -tzf` | jarvis 作業ツリー |

- PostgreSQL ダンプのフッタ検証には文字列 `PostgreSQL database dump complete` を用いる
  （`verify-backup.sh:89`）。
- config allowlist は `mcp_allowlist.toml` / `pii_patterns.toml` /
  `casbin_model.conf` / `casbin_policy.csv` / `cedar_schema.cedarschema` /
  `policies.cedar` の 6 点のみ（`backup-local.sh:167-182`）。
  `bws.env` / `*.env` / token / `otelcol-config.yaml` は対象外。
- git は `--skip-git` またはリポジトリ未検出時に生成されない**任意**コンポーネント。
- `COMPLETE` は全ステップ成功後にのみ書かれる完了マーカー（`backup-local.sh:211-213`）。
- 注意: `verify-backup.sh:110-119` の `config-*.tar` 検査は旧ディレクトリ形式の
  レガシーであり、現行の `config-<basename>` には一致しない。
  設定の権威ある根拠はマニフェスト記載である。

### 必須コンポーネントの定義

新ツールは `verify-backup.sh` より既定で厳しく、以下をすべて満たすセットのみを受け付ける。

- `manifest.txt` が存在する。
- `COMPLETE` が存在する（無い場合は `--allow-legacy` の明示指定が必要）。
- マニフェスト記載の全ファイルが存在し、size/hash が一致する。
- 主要データコンポーネント（`postgres-jarvis.sql.gz` または `qdrant-snapshot.bin`）が
  少なくとも 1 つ存在する。
- 個別復元を指定したコンポーネントは、マニフェストに記載が無ければ fail-closed で拒否する
  （黙ったスキップをしない）。

## 復元手順

### ステップ 0: セット選択と計画

`--backup-set DIR` または `--latest` でセットを解決し、選択したセット名と
復元対象コンポーネント、停止予定サービス、書き込み先を出力する。既定はここで終了する。

```text
restore-local --latest                       # 検証 + 計画のみ（dry-run）
restore-local --latest --apply               # 隔離先へ復元（本番は触らない）
restore-local --latest --apply --confirm     # 本番上書き（破壊的）
restore-local --latest --apply --keep-isolated-db  # 隔離DBを保持（既定は検証後に削除）
restore-local --latest --skip-git --skip-configs --apply
```

### ステップ 1: 整合性検証（書き込み前ゲート）

選択セットに対し [verify-backup.sh](../../infra/scripts/verify-backup.sh) 相当の
検証を実行し、失敗時は終了コード 1 で即座に停止する。dry-run でも必ず実行する。

### ステップ 2: サービス管理

1. `systemctl is-enabled` / `is-active` で存在かつ active なユニットのみを列挙する。
2. DB/config/git の書き込み前に、依存サービス
   （`jarvis-policy`, `jarvis-mcp-gw`, `jarvis-event`, `jarvis-context`, `opencode`,
   `code-server`, `otelcol`）を停止する。
3. 停止したユニットを配列に記録し、`trap` で成功・失敗いずれでも逆順に再起動する。
4. **Qdrant 不変条件**: HTTP API を使う復元では `qdrant.service` を停止しない。
   REST Endpoint はプロセスが listening して初めて応答するため、
   「停止 → HTTP upload」の組み合わせは絶対に作らない。
   ファイル直接配置方式を選ぶ場合のみ Qdrant を停止し、その場合は HTTP upload を呼ばない。
   推奨は API 方式（後述）で、この方式では Qdrant は起動したままにする。

### ステップ 3: コンポーネント別復元

#### PostgreSQL

1. 隔離 DB（例 `jarvis_restore_test`）を作成し、
   `gzip -cd "$SET/postgres-jarvis.sql.gz" | psql -v ON_ERROR_STOP=1 -d <隔離DB>`
   で復元する。既存 DB がある場合は上書きせず失敗させる。
2. `information_schema.tables` の件数を検証する（値そのものはログに出さない）。
3. 検証後に `DROP DATABASE "<隔離DB>"` で隔離 DB を削除する。Qdrant の
   隔離コレクション削除と対称な後始末であり、既定では本番データのコピーを
   残さない。保持する場合のみ `--keep-isolated-db` を明示する。
4. `CREATE DATABASE` 成功後に `DROP` 完了前に異常終了した場合は、
   `restore-local.sh` の EXIT trap が残存隔離 DB を削除する。既存の
   サービス再起動 trap はそのまま維持し、後始末はその後に実行する。
5. 本番反映は `--confirm` 時のみ。旧 DB を退避してから
   `DROP DATABASE` / `CREATE DATABASE` → 復元 → 再度件数検証を行う。

#### Qdrant

1. 隔離コレクション（例 `jarvis_memories_restore_test`）へ
   `POST /collections/<name>/snapshots/upload?priority=snapshot` でアップロードする。
   `priority=snapshot` が必須（既定の `replica` では空コレクションになる）。
   手順は [backup-restore-runbook.md](backup-restore-runbook.md) と
   [Qdrant 公式ドキュメント](https://qdrant.tech/documentation/snapshots/) に準拠。
2. `points_count` を検証し、隔離コレクションを削除する。PostgreSQL の隔離 DB も
   検証後に DROP して同じく後始末する（保持オプション `--keep-isolated-db` は
   PostgreSQL 側のみ）。
3. 本番反映は `--confirm` 時のみ。同一コレクションへ API 復元するか、
   新規コレクションへ復元して alias を切り替えるかは「未決定事項」とする。
4. いずれの場合も Qdrant は active のまま操作し、停止後 HTTP 呼び出しをしない。

#### configs

1. `config-<basename>` を一時ディレクトリへ展開し、現行 `/etc/jarvis` と `diff` する。
2. マニフェストに allowlist 外の `config-*` が含まれていた場合は拒否する。
3. 本番反映は `--confirm` 時のみ。許可されたファイル名を元の配置パスへ個別にコピーする
   （再帰 tar 展開は行わない）。`bws.env` / `*.env` / `otelcol-config.yaml` は触らない。

#### git

1. `git-jarvis-head.tar.gz` を一時ディレクトリへ展開して内容を検査する。
2. 本番反映は `--confirm` 時のみ。作業ツリーが clean
   （`git status --porcelain` が空）の場合のみ上書きを許可し、
   未コミット変更がある場合は拒否して新規ディレクトリへの展開を提案する。

### ステップ 4: 事後検証

各サービスの health endpoint と `systemctl status` を確認し、
DB テーブル数 / Qdrant `points_count` / config 差分 / `git status` を再確認する。
再起動に失敗したユニットは name のみを stderr に出力する。

## 安全性と検証

- **ゲート**: 検証 fail 時はセット選択後すぐに停止し、ファイルを 1 バイトも書き換えない。
- **明示性**: `--apply` と `--confirm` の両方が無い限り本番へ書き込まない。
  `--confirm` のみの指定は無効として拒否する。
- **部分復元の制御（現行セマンティクス）**: `--skip-db` / `--skip-qdrant` /
  `--skip-configs` / `--skip-git` で指定したコンポーネントは復元対象から
  除外され、そのコンポーネントのマニフェスト存在ゲートと restore-input
  解決ゲートの対象外となる。`restore-local.sh` の CLI help
  `--skip-*  omit a component (and its manifest presence gate).` と一致する。
  選択（非スキップ）したコンポーネントの入力が欠ける場合は fail-closed で拒否する。
  スキップ指定があっても config allowlist とロール解決ループは無条件に実行され、
  allowlist 外の `config-*` や不正ロールは拒否される。
  （履歴: 初期設計では「スキップ指定は事前検証の対象からは除外しない」としていた。
  実装と CLI help はスキップ時にゲートを除外するセマンティクスで運用されているため、
  現行動作に合わせて本項目を更新した。旧記述は現行セマンティクスではない。）
- **同時実行防止（I81-A）**: `flock -n` は **restore-local.sh インスタンス同士**
  のみを直列化する。`backup-local.sh` はこのロックを取得しないため、
  backup timer とのインリーブ除外は担わない。timer 除外は
  `stop_services()` + `verify_stopped_services()` が担当する
  （`jarvis-backup.timer` を停止し、dispatch 前に inactive を確認して
  fail-closed する）。取得失敗は固定語彙 `[restore-local] LOCK FAIL:`
  で fail-closed とする。
- **ロックパスと inode 整合性**: 既定は安定パス `/run/jarvis/restore.lock`
  （`LOCK_FILE` で上書き可）。`/run` が無い環境、または非 root で
  `/run/jarvis` を作成できない場合は `/tmp/jarvis-restore.lock` へ
  フォールバックし WARN を明示する。`/tmp` は world-writable なので、
  inode 入れ替えの補完制御として open 前後の `stat -c '%d:%i'` 照合が必須。
  変化時は `LOCK_INODE_CHANGED` で fail-closed する。ロックファイルは
  削除しない（保持中プロセスの inode を壊し相互排除を破壊するため）。
- **子プロセスへの fd 漏洩防止**: bash の動的 fd（`{VAR}`）には FD_CLOEXEC が
  立たないため、`psql` / `curl` 等の子が fd を継承すると親終了後も古い
  flock が残り、後続復元が `LOCK_HELD` で拒否される。module dispatch 前に
  `exec {LOCK_FD}>&-` で子へ渡さない。代わりにロックファイルへ
  `pid=` / `acquired_at=`（非機微な値のみ）を記録し、stale 検出の
  手掛かりを残す。
- dry-run もロックを取得するが、`systemctl` を呼ばないため timer を触らない
  契約は変わらない。
- **秘密情報**: 設定ファイルの内容を標準出力・ログへ出さない。出力はファイル名、
  サイズ、ハッシュの先頭 12 文字、件数に限定する。`.env` / `bws.env` / token は
  読み取らず、config は allowlist のみ。
- **停止/再起動の保証**: 停止したユニットの記録を `trap EXIT` で保持し、
  途中で `set -e` による異常終了でも再起動を試みる。
- **隔離リソースの後始末**: 既定では検証直後に隔離 DB / 隔離コレクションを削除し、
  本番データのコピーを残さない。`--keep-isolated-db` は明示的な保持オプション。
  CREATE 後に失敗した場合も EXIT trap が隔離 DB を片付け、サービス再起動は維持する。
- **ロールバック**: 本番上書き前に旧 DB / 旧コレクション / 旧 config の退避先を必須とする。
  PostgreSQL の `DROP DATABASE` は退避なしでは実行しない。
- **検証コマンド**: 実装後は `bash -n` / `shellcheck` / `shfmt -d`、
  各コンポーネントのモックテスト、隔離先での統合テストを必須ゲートとする
  （この設計文書作成時点では未実行）。

## 代替案

| 案 | 判定 | 理由 |
| --- | --- | --- |
| 既存 `restore.rs` を拡張して流用 | 却下 | OCI/rclone/`op inject` 依存は ADR-0004 に反し、命名体系も非互換。ほぼ全面書き換えになり新規と変わらない。 |
| `restore.rs` を即時削除 | 今回は見送り | ADR-0004 はレガシー資産の凍結方針。履歴保全のため非推奨として残し削除は別決定とする。 |
| verify + 手動 Runbook のみ | 却下 | サービス停止/再起動とコンポーネント選択が手順依存でヒューマンエラーを防げない。 |
| `restic` / `borg` 等专业バックアップツールへ移行 | 今回は却下 | 現行 `backup-local.sh` のフォーマットを入力にする要件と新規依存が不要。将来の保存先再設計時に再評価。 |
| 本番へ直接復元（隔離ステップ省略） | 却下 | 破壊操作の事前証明ができずロールバックも困難。 |

## 実装分割案

以下の順に分割する。S4〜S7 は独立しており、複数 coding エージェントが並行する場合は
`infra/scripts/restore-local.d/` に `verify.sh` / `services.sh` / `pg.sh` / `qdrant.sh` /
`configs.sh` / `git.sh` として分離し、親が `restore-local.sh` から source して統合する
（書き込みスコープを重複させない）。

1. **S1: 骨格と引数解析**
   `infra/scripts/restore-local.sh` に `--backup-set` / `--latest` / `--apply` /
   `--confirm` / `--allow-legacy` / 4 つの `--skip-*` / `--help` を実装。
   書き込みゼロで dry-run 完了。受け入れ: `bash -n` / `shellcheck` 通過、
   不引数時に計画のみ出力。
2. **S2: セット解決と検証アダプタ**
   [verify-backup.sh](../../infra/scripts/verify-backup.sh) を単一セットに対して呼び出し、
   終了コードと出力を解釈して fail-closed 判定する。受け入れ: 正常セット、
   `COMPLETE` 欠落、hash 不一致、必須コンポーネント欠落のフィクスチャで
   期待終了コードが出ること。
3. **S3: サービス inventory と停止/再起動**
   利用可能かつ active なユニットのみ停止し、`trap` で逆順再起動。
   受け入れ: モック `systemctl` を PATH に置き、停止リストと
   エラー時の再起動呼び出しをアサート。Qdrant が停止対象に含まれないこと。
4. **S4: PostgreSQL 復元**
   隔離 DB 復元と件数検証、検証後の隔離 DB DROP、`--confirm` 時の本番反映と退避。
   受け入れ: モック `psql` / `gzip` で `ON_ERROR_STOP=1` と退避手順を確認。
5. **S5: Qdrant 復元**
   `priority=snapshot` 付き upload、`points_count` 検証、隔離コレクション削除、
   `--confirm` 時の本番反映。受け入れ: HTTP リクエスト順をモックし、
   `qdrant.service` の stop が 1 度も呼ばれないことをアサート。
6. **S6: configs 復元**
   一時展開、`diff`、allowlist 強制、`--confirm` 時の個別コピー。
   受け入れ: allowlist 外のマニフェスト項目を拒否し、秘密ファイルを
   一切読み取らないこと。
7. **S7: git 復元**
   一時展開と clean tree ガード、`--confirm` 時の上書き。
   受け入れ: 未コミット変更がある作業ツリーで拒否されること。
8. **S8: ドキュメントとレガシー整理**
   `docs/integration/index.md` への追記、Runbook からの相互リンク、
   `restore.rs` への非推奨ヘッダ、`infra/README.md` のレガシー記述更新。
9. **S9: 隔離統合テストと Runtime Proof**
   使い捨て DB / コレクション / ディレクトリで全コンポーネントを復元し、
   検証結果を記録。本番適用はユーザー承認後に行う（エージェント判断外）。

## 未決定事項

- **複数 PostgreSQL DB のバックアップ／復元（Round 87 発掘、Round 95 差し戻し）**
  - 現状: `jarvis-migrations.service` は `JARVIS_CONTEXT_PG_URL` と
    `JARVIS_POLICY_PG_URL` の両方へ同一 migration set を適用するが、
    `backup-local.sh` は `pg_dump -d jarvis` のみ、`restore-local.d/pg.sh` も
    主 DB 一つしか扱わない。両 URL が意図的に別 DB を指す構成では、
    承認履歴（`approval_requests`）と監査証跡（`audit_log` / `kill_switch`）が
    DR 時に失われる。
  - 現行テンプレートは同一シークレットを参照するため通常は単一 DB であり、
    本ギャップは「コードが支持する一方で未検証の構成」でのみ顕在化する。
  - 却下した実装（Round 95）: 追加 DB を `postgres-extra-<db>.sql.gz` /
    `role=other` で manifest に記録し `pg.sh` で列挙する方式は、既存の
    「複数 postgres row は ambiguous 拒否」回帰と dry-run 出力契約
    （本番 DB 名の表示）の2点で矛盾し、10 件の回帰が失敗したため差し戻した。
  - 判断が必要な論点: (1) 追加 DB を別 role で表現するか既存 postgres role を
    拡張するか、(2) 主 DB と追加 DB の復元順序と片方失敗時のロールバック、
    (3) 単一 DB セットとの後方互換を verifier のどのゲートで担保するか。
  - 判断後に実装し、S9 の隔離統合テストで両 DB 構成の往復を検証する。
  - 推奨方針（判断材料）: まず `JARVIS_POLICY_PG_URL` が
    `JARVIS_CONTEXT_PG_URL` と異なる値で運用される予定があるかを確認する。
    同一なら現行の主 DB のみの契約で十分であり、設計書へ「単一 DB 契約」と
    明記して終了する。異なるなら、追加 DB を同じバックアップセット内の別
    role として扱うのではなく、DB ごとに独立したセットディレクトリへ分ける
    案を採る。こうすれば「複数 postgres row は ambiguous 拒否」の既存ゲートと
    衝突せず、主 DB の復元契約も変えずに済む。
- 本番 PostgreSQL の上書き方式: `DROP/CREATE` か、新規 DB 復元後の alias/接続切替か。
- 本番 Qdrant の上書き方式: 同一コレクションへの API 復元か、新規コレクション +
  alias 切替か（snapshot の互換性は作成元と同一 minor 以降という制約あり）。
- config 復元時の所有者/パーミッション（現行実機値は未確認）。
- git 作業ツリーの上書きポリシー（clean 时必须か、常に新規ディレクトリか）。
- `COMPLETE` の無いレガシーセットの扱い（既定拒否 + `--allow-legacy` で合意済みか）。
- [verify-backup.sh](../../infra/scripts/verify-backup.sh) の `config-*.tar` 検査を
  現行形式へ修正するか、レガシー検査として残すか。
- ツールの配置先と配布方法（repo 内実行か `/opt/jarvis/scripts` への install か）。
- `restore.rs` の完全削除タイミング（今回は非推奨保持を決定済み）。
- 本番上書き前の退避先の具体 path と保持期間。
- 実装言語の最終確認（本設計は bash を推奨。Rust 化する場合は S1〜S7 の再見積もりが必要）。
