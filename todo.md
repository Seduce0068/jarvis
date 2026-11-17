---
type: Todo
title: "JARVIS クロスレビュー集約 TODO"
description: "5 ラウンド・25 エージェントの全体クロスレビューで得た指摘を、重複排除と妥当性検証を経て集約したタスク一覧。"
tags:
  - todo
  - cross-review
  - jarvis
generated:
  by: DeepSeek Harness (lead agent)
  at: "2026-11-17T19:56:05+09:00"
---

## 実施記録

- 実施日: 2026-11-17。対象: `jarvis` リポジトリ全体（Rust 5 crate、
  GPUI UI、systemd、CI、docs/ADR）。
- 構成: 5 ラウンド（セキュリティ / アーキテクチャ / 運用 / テスト /
  ドキュメント）× 5 視点 = 25 エージェント、失敗分 8 件を再起動。
  有効結果 14 エージェント、生指摘約 90 件。
- すべて読み取り専用（read / grep / glob）で実施。コード修正は行っていない。
- Runtime Proof: `cargo check --all-targets --locked` と `make ci-local` は
  ともに exit 0（全ゲートผ่าน）。未使用依存の警告 6 件のみ。
- 集約方針: 同一根因は 1 タスクに統合し、元 ID を括弧で保持。
  却下・前提修正は「妥当性検証」節に記録。優先度は P0（至急）→ P3（低）。

## P0: 認証・承認・シークレット（至急）

- [ ] **P0-1** 承認 API の既定無認証を解消（R1-AUTH-001）
  - **2026-11-18 一部対応**: opt-in strict ガード実装済み。本番有効化は残る。
    `require_approval_api_token`（既定 `false`）が `true` のとき、トークン
    未設定なら承認 endpoints は 503 で fail-closed
    （`config.rs:38`, `http.rs:122-136`）。既定は loopback-only の従来動作を
    維持し、トークン設定時は `X-Jarvis-Approval-Token` ヘッダを要求する。
  - **残る対応**: 実機の `/etc/jarvis/policy.env` へのトークン注入と
    `require_approval_api_token=true` 化は、UI / 既存フローへの影響を確認
    した上で判断する（値は確認・記録しない）。`approved_by` の認証 Identity
    紐付けは未対応。
- [x] **P0-2** MCP Gateway が承認トークンを送信していない（2026-11-17 対応済み: `new_with_token` + ヘッダ送信、4コンポーネント契約テスト済み。NOTE: 可用性障害は policy 側にも同じトークン設定が必要）（R1-AUTH-002）
  - `ApprovalClient::new(endpoint, None)` が既定。`new_with_token` を使い、
    設定時／未設定時の往復テストを追加（`lib.rs:40-42`,
    `approval.rs:76-78`）。設定すると全承認ツールが 401 で fail-closed
    になる可用性障害も併発している。
- [ ] **P0-3** `/v1/execute` と gRPC の principal 自己申告を廃止（R1-AUTH-003）
  - 呼び出し側が `opencode` 等を名乗れり、承認も自分で通せば実質任意操作。
    `/mcp` と同様にサーバ側固定または認証済み Identity から導出
    (`http.rs:28-33`, `grpc.rs:19-32`, `lib.rs:62-103`)。
  - **2026-11-18 実コード確認**: `/mcp` は `http.rs:42-45` → `mcp_server.rs`
    で `gateway.mcp_principal()` のサーバ側固定値を使用（対応済み）。
    一方 `/v1/execute`（`http.rs:28-33, 105-113`）と gRPC
    （`grpc.rs:19-32` + `mcp_gateway.proto:10-14`）はリクエストの
    `principal` をそのまま使用しており未対応。P0-3 は残る2経路を対象に
    継続（詳細は [remaining-tasks.md](docs/integration/remaining-tasks.md)）。
- [x] **P0-4** kill switch を承認経路へ貫通（2026-11-17 対応済み: approve/deny が `KillSwitchActivated` を返し HTTP 409 にマップ、回帰テスト追加）（R2-UI-001, R3-RUN-002）
  - `approve` / `deny` が `is_activated()` をチェックせず、発動中も決定が
    通る。ガード追加（409/403）と UI への状態表示・ボタン無効化。
    発動／解除の curl 手順ランブックも未整備。
  - 根拠: `policy/src/lib.rs:120-144`, `ui/src/approvals.rs:88-129`。
- [x] **P0-5** 子プロセスへの全シークレット継承を遮断（2026-11-17 対応済み: `subprocess.rs` の `apply_minimal_env` で env_clear + 最小限の変数のみ。upstream MCP / git spawn に適用。smoke 12/12）（R1-MCP-001,
    R1-SEC-001）
  - upstream MCP / git spawn に `env_clear()` が無く、`bws run` の
    `BWS_ACCESS_TOKEN` 等が継承される。spawn 前に `env_clear()` し
    `PATH` / `HOME` 等最小限を明示（`gateway.rs:110-113`,
    `sandbox.rs:225-229`）。サービス間でも bws プロジェクト分离が必要。
- [ ] **P0-6** otelcol の `0.0.0.0` バインドを `127.0.0.1` へ（R1-SEC-002）
  - **2026-11-18 現状**: repo 側 `infra/config/otelcol-config.yaml` の
    6リスナー（health 13133 / pprof 1777 / zpages 55679 / OTLP 4317,4318 /
    internal 8888）を `127.0.0.1` へ変更済み（`:8,10,12,18,20,165`）。
  - **実機対応は残る**: 実機は `/etc/jarvis/otelcol-config.yaml` を読み込む
    ため、repo テンプレートの同期と `ss -lnt` による確認は未実施。
- [ ] **P0-7** Grafana 認証情報の供給経路を確定（R1-SEC-003）
  - `otelcol-config.yaml` が `GRAFANA_CLOUD_AUTH` を要求するが unit は
    直接起動で未注入。実値を config / journal に残さない経路を統一。
  - **2026-11-18 判断材料**: [3案の比較](docs/integration/remaining-tasks.md)
    （bws run / root 管理 EnvironmentFile / ファイルベース secret）を記録済み。
    `otel.env` の `GRAFANA_CLOUD_TOKEN` / `INSTANCE` と config 要求の
    `GRAFANA_CLOUD_AUTH` は名前が不一致で、単なる env ファイル渡しでは解消しない。
    実機の公開範囲・認証方式は未確認で、判断は確定させていない。
- [x] **P0-8** UI の承認エラーが直後の refresh で消える（2026-11-17 対応済み: `action_status` フラグで poll と決定結果の表示を分離）（R2-UI-002）
  - `decide` が `view.error` を設定後 `apply_poll` が `None` で上書き。
    401 / 409 / タイムアウト理由が隠蔽される。順序または世代管理で修正
    (`approvals.rs:70-84,117-125`)。

## P1: データ整合性・アーキテクチャ・DR（高）

- [ ] **P1-1** ベクトル検索の順序を保持（2026-11-18 一部対応: 順序保持は実装済み、スコア保持は残。PG の `WHERE id = ANY` が順序を保たないため、Qdrant の relevance 順に再組み立てる reorder_by_ids を追加。純粋関数テスト2件。スコア自体の保持は MemoryEntry 共有型変更が必要で保留）（R2-DATA-001）
  - `ScoredPoint.score` を破棄し `WHERE id = ANY($1)`（ORDER BY なし）で
    再取得 → ランキングが失われハイブリッド検索が ID 突き合わせに退化。
- [ ] **P1-2** PG / Qdrant 二段書き込みに補償を追加（R2-DATA-002）
  - Qdrant upsert 失敗時に PG だけ残り、再送でも自己修復されない。
    outbox または起動時整合性チェック＋再埋め込み。
- [ ] **P1-3** マイグレーションをバージョン管理へ移行（R2-DATA-003）
  - 毎起動 `batch_execute("001_initial.sql")` で `IF NOT EXISTS` のみ、
    `_sqlx_migrations` 不在。sqlx migrate またはデプロイ時 CLI に分離。
- [ ] **P1-4** バックアップの保全先を外部化（R3-DR-001）
  - `/var/lib/postgresql/jarvis-backups` は本番データと同一親ディスクで
    SPoF。最低 1 世代を別媒体へ。RPO/RTO も併せて決定。
- [ ] **P1-5** timer を実機で有効化し復元演習へ（R3-DR-004, R3-DR-005）
  - repo 側は完了、実機は未導入 / inactive。隔離先での実復元検証も未実施。
    `make timer-status` → `sudo ... install-backup-timer.sh --apply` →
    確認 → 復元演習の順（ユーザー実行が必要）。
- [ ] **P1-6** バックアップを暗号化・署名し保持期間を設定（R3-DR-002,
    R3-DR-003）
  - manifest は平文 SHA-256 のみ、署名なし。`--retain-days` は opt-in で
    unit が渡していない。ディスク枯渇と改ざん検知不能のリスク。
- [ ] **P1-7** 共有ドメインモデルを一本化（R2-ARCH-001, R2-ARCH-005,
    R2-ARCH-009）
  - `jarvis-core` が policy / mcp-gw / UI から未参照で、承認型が
    4 種類（`String+i64` / `Uuid+DateTime` / …）に分裂。core を真の
    共有源にするか、proto を単一情報源として core を削除する設計判断が必要。
  - **2026-11-18 調査（判断材料）**: `grep -rl jarvis_core` で core を参照するのは
    jarvis-event(5ファイル) と jarvis-context(4ファイル) のみ、policy / mcp-gw は
    0 ファイル。`ApprovalRequest` 系は core / policy(policy.rs) の2定義に加え、
    HTTP DTO が policy(http.rs) と mcp-gw(approval.rs) の2種類で、合計4系統。
    -wide な型統一は回帰リスクが高いため、どちらを単一情報源にするかの判断を
    先に確定し、その後スライス移行する方針を推奨（値・認証情報は未確認）。
- [x] **P1-8** テストスクリプト 3 本を CI ゲートへ接続（2026-11-17 対応済み: ci.yml extra_gates に check-shell / safety-test / test-verify-backup / test-review-run を追加、ローカル全成功）（R4-SCRIPT-001,
    R3-DR-006, R4-GATE-006）
  - `test-review-run.sh` / `test-legacy-guards.sh` /
    `test-verify-backup.sh` が PR ゲートで未実行。Runtime Proof で ci.yml
    `extra_gates` に呼び出しが無いことを確認済み。
- [x] **P1-9** テスト仕様レイヤとエビデンスを整備（R4-SPEC-001〜007）
  - **2026-11-18 全6スライス対応済み**: policy / mcp-gw / context / backup /
    event / ui の6仕様とハッシュ一致エビデンス6件。check-evidence.sh が
    必須4セクション・Traceability・index 双方向リンクを自動検証。
  - （解消済み）旧状態: `specs/test-*.md` / `evidence/` が 0 件。必須 4 セクション、
    Traceability、基準 ID、tier、29119 Annex A がすべて同根で未充足。
    優先: policy → mcp-gw → context(PII) → backup → その他。
- [x] **P1-10** 成果物 7 ファイルへ OKF frontmatter 付与（2026-11-17 対応済み: 全7ファイルに type/title/description/tags/generated を付与、H1 を削除し本文を H2 開始へ。okf_validate 全成功、MD025/MD041/MD001 なし。既存 MD013 等は未解消）（R5-FMT-001,
    R5-ADR-001）
  - `adr/0003`, `adr/0004`, `docs/architecture/README.md`, `report.md`,
    `remaining-tasks.md`, `tool-matrix.md`, `dsh.md` が H1 開始。
    `type` 必須・本文 `##` 開始へ。RFC 3339 違反（R5-FMT-002）も同時修正。

## P2: ゲート・テスト・CI（中）

- [ ] **P2-1** PII フィルタと normalizer にテスト追加（2026-11-17 一部対応: pii.rs に from_config を追加し再帰 redact / enabled=false パススルー / 不正パターンスキップの3テスト。normalizer 側は未対応）（R4-TEST-001,
    R4-TEST-002, R2-DATA-008)
  - `pii.rs` / `normalizer.rs` に `#[cfg(test)]` が皆無。無効パターンの
    サイレントスキップ、`enabled=false` バイパス、policy deny 時の
    dispatch 抑制を検証。設定読込失敗の fail-closed 化も。
- [ ] **P2-2** review-gate の permissions と依存を修正（R4-GATE-001,
    R4-GATE-002, R4-GATE-004, R4-GATE-005）
  - **2026-11-18 監査**（[CI-1 監査節](docs/integration/remaining-tasks.md)）:
    監査対象は未配線の参照実装 `ci/review-gate.yml` のみ。安全版
    `.github/workflows/review-gate.yml` は手動 dispatch 限定で
    `make ci-local` のみを実行し、merge / comment / 状態変更を行わない。
    安全版は現状維持とし、参照実装と混同しない。
  - 監査で判明した修正点: `checks: write` は未使用で削除検討、
    `gh pr comment` は `pull-requests: write` を要求（旧「read 欠落」は
    監査で否定）、fork PR では既定トークンが読み取り専用、
    `gh pr merge --auto --squash --delete-branch` は `contents: write`
    不足。OpenCode CLI は未固定スクリプトのパイプ実行、runner 切替時も
    導入 step が無条件。セッション権限の自動受け付けと認証情報の
    供給経路は未決定。auto-merge は人間の承認なしに任意 PR を対象に
    しうるうえ branch protection 未確認、`concurrency` も未設定
    （安全版は PR 単位 group を設定済み）。
  - **判断は確定させない**: 権限・認証情報・自動承認・auto-merge の
    方針が確定するまで参照実装は未配線のままとし、安全版を配線経路
    とする。完全配線（PR トリガー / cross-review / auto-merge）は
    方針次第で、P0-3 と同じく「一部実装済み（安全版のみ）」扱い。
- [x] **P2-3** UI を CI へ組み込み（2026-11-17 対応済み: ci.yml extra_gates に `make ui-test` を追加、7 tests 成功。UI clippy の undocumented_unsafe_blocks は既に付与済み）（R2-UI-005, R5-RULES-001）
  - `ui/` は独立 workspace で CI 対象外。amd64 ジョブで `make ui-test`。
    UI clippy にも `-D clippy::undocumented_unsafe_blocks` を付与。
- [x] **P2-4** `ci-local` と CI の差分を解消（2026-11-18 対応済み: `ci-local` の説明を「主要ゲートのローカル代替（aarch64 クロス/Docker 依存ゲートは除外）」へ訂正。実機に aarch64-linux-gnu-gcc / Docker が無いため組み込まず、除外理由を明文化）（R4-GATE-006, R4-SPEC-010）
  - `extra_gates`（aarch64 check, `--ignored` test）が `ci-local` に無い。
    組み込むか「主要ゲートのローカル代替」へ説明を訂正。
- [x] **P2-5** シェル静的検証をゲート化（2026-11-17 対応済み: `ci/scripts/check-shell.sh` で全26スクリプトに bash -n / shellcheck。shfmt は既存ドリフト16件のため警告のみ。ci-local に組込み）（R4-SCRIPT-003）
  - `bash -n` / `shellcheck` / `shfmt -d` がリポジトリ内のどの自動ゲート
    にも無い。`extra_gates` に追加。
- [x] **P2-6** 未使用依存・矛盾バージョンを整理（2026-11-17 対応済み: cargo 警告の未使用依存 workspace 8件 + per-crate 6件を削除し Cargo.lock 更新。`cargo check --locked` / clippy / 全テスト / smoke 12/12 成功。警告0）（R2-DEBT-001〜003,
    R2-DEBT-008, R1-SUPPLY-002)
  - `prost-build 0.13`（他は 0.14）、`config`（figment と併用）、
    `postgres` / `sqlx` / `secrecy` / `clap` / `thiserror` が未使用。
    Runtime Proof の cargo check 警告 6 件と一致。
- [x] **P2-7** `deploy.rs` を fail-closed 化（2026-11-18 対応済み: `make deploy` を無効化し ADR-0004 準拠の `make deploy-mcp-gw` を案内。`deploy.sh` は既に fail-closed 済み。`make deploy-dry` は参照用として維持）（R2-DEBT-004）
  - `make deploy` が ADR-0004 否定のリモート aarch64 配送を実行可能。
    `deploy.sh` と同じ fail-closed へ。
- [ ] **P2-8** デプロイスクリプトへ `-e` と HTTP ステータス検証（2026-11-17 一部対応: health check を `%{http_code}=200` 判定へ強化、dry-run 成功。`-e` は rollback ロジックへの影響があるため未適用）（R1-SEC-008,
    R1-SUPPLY-001)
  - `set -uo pipefail` で配置失敗が fail-open。ヘルス確認も `curl -f` /
    `200` 判定が無い。Runtime Proof で `set -uo pipefail` 確認済み。
- [ ] **P2-9** systemd hardening を全 unit へ底上げ（2026-11-18 一部対応: 全9 service に `UMask=0077`、欠落 unit に `ProtectControlGroups` / `RestrictSUIDSGID` を追加、自社スクリプトの backup に `SystemCallFilter=@system-service`。`systemd-analyze verify` 全 unit 成功。**SystemCallFilter を code-server / opencode / otelcol / qdrant のサードパーティバイナリへ追加するのは実機起動確認が必要なため保留**。`/etc/jarvis` の ReadWritePaths 個別ファイル化も書き込み先の把握が必要で保留）（R1-SEC-004,
    R1-SEC-009）
  - otelcol / opencode / code-server / qdrant / backup で
    `SystemCallFilter` 等が欠落。全 unit に `UMask=0077` も追加。
    `/etc/jarvis` の `ReadWritePaths` も個別ファイルへ絞る。
- [x] **P2-10** `.gitignore` を `infra/config/*.env` へ拡張（R1-SEC-005）
  - **2026-11-18 判断記録済み（「採用しない」）**:
    [判断記録](docs/integration/remaining-tasks.md) に、値なし参照テンプレート
    を ignore すると更新履歴が失われ構成ドリフト（BK-4）を招くため tracked
    維持とする記録を追加。`.gitignore` とテンプレートは未変更。
  - **残る対応**: 将来テンプレートへ秘密値が混入した場合は、そのファイルを
    個別に ignore 対象へ移す判断が必要（全体への blanket ignore は避ける）。
- [x] **P2-11** setup スクリプトの固定 DB パスワードを除去（2026-11-17 対応済み: `JARVIS_DB_PASSWORD` 環境変数必須化、code-server 生成パスワードの標準出力を廃止、infra/README に手順追記）（R1-SEC-007）
  - 固定文字列をハードコード、生成パスワードを標準出力へ表示。
    `secret_scan.py` が SQL の `WITH PASSWORD` を検出できない問題も。

## P3: ドキュメント・レガシー整理（低）

- [x] **P3-1** ADR-0001 を現行方針へ整合（2026-11-18 対応済み: 比較テーブルの IaC/秘密情報/バックアップ/Audit Log の旧前提に ADR-0004 による置換を注記。status: accepted と決定本文は維持）（R2-ARCH-004, R2-DEBT-005,
    R5-ADR-003, R5-ADR-004）
  - OCI / 1Password / Object Storage 前提が `status: accepted` のまま。
    ADR-0004（インフラ）と ADR-0002（ランタイム）で置換された旨を明記。
- [x] **P3-2** 切れた参照と陳腐な記述を更新（2026-11-17 対応済み: AGENTS.md の存在しないルート文書2件を削除し実在ディレクトリ構成へ更新、TODO 正本と手動 review-gate の記述を実態に合わせ。MD013 等は既存のまま）（R5-ADR-006, R5-ADR-009,
    R5-ADR-010, R3-RUN-004）
  - ルートの旧アーキテクチャ文書 2 件が不在。`docs/architecture/` の実在
    パスへ。設定置き場所（`/etc` / `/opt` / `$HOME/jarvis`）の対応表も作成。
- [x] **P3-3** ローカル運用ランブック整備（R3-RUN-001, R3-RUN-003,
    R3-RUN-005, R3-RUN-006）
  - （解消済み）旧状態: 起動→検証→停止（停止手順は 0 件）、手動変更のロールバック、
    `make verify-backup` が `/opt` へ書き込む矛盾、clippy 表記の不統一。
- [ ] **P3-4** markdownlint / 索引を整備（2026-11-17 一部対応: `adr/index.md` と `adr/log.md` を追加。**2026-11-18 監査: 新規 lint 0・baseline 456 issues 残存**、リンク切れ0。既存 MD013 の全解消は未実施）（R5-FMT-003〜006）
  - テンプレートの `type` 欠落、MD013（docs 156 行 / adr 18 行）、
    `adr/index.md` と `log.md` の未配置。
- [ ] **P3-5** レガシー資産の最終判断（R2-DEBT-006, R2-DEBT-011,
    R3-DR-015, R5-RULES-002）
  - **2026-11-18 現状**: 4下位項目のうち3件は判断・担保済み。
    - `op://` テンプレート4件（`context.env` / `event.env` / `policy.env` /
      `otel.env`）は値なし参照テンプレートとして tracked 維持と判断済み
      （[判断記録](docs/integration/remaining-tasks.md)）。実機起動は
      `bws.env` + `bws run` で、テンプレートは参照しない。
    - OpenTofu state（BK-3）は「ociは使わない」方針のもと OCI リソース
      削除を実行しないと判断済み。後始末スクリプトは参照用に保留。
    - 破壊的な `rust/scripts/restore.rs` は `infra/scripts/restore.sh` が
      fail-closed（exit 2）で委譲を止め、`make safety-test` が回帰担保。
    - ライセンスは `LICENSE` と `ui/Cargo.toml` が AGPL-3.0-or-later、
      `rust/Cargo.toml` の workspace が `MIT OR Apache-2.0`（5 crate 継承）。
      法的判断のため自動変更せず、ユーザー判断待ち。
  - **残る未完了はライセンス方針のみ**。判断は確定させない（ライセンス
    変更・OCI 実操作・テンプレート削除はいずれも別途判断）。
- [x] **P3-6** UI の監査性と細部（R2-UI-003, R2-UI-004, R2-UI-006〜009）
  - **2026-11-18 一部対応**: 期限切れ強調、テスト数表記（7）、
    `approved_by` 固定値とトークン注入経路を README に記録。
    （解消済み）旧状態: リダイレクトでトークン送信、ポーラー競合。
- [ ] **P3-7** マイナーな設定・静的ガード（2026-11-18 一部対応: `web_max_bytes` の u64→usize キャストを clamp-then-try_from に修正し巨大値で wrap しないようにした。回帰テスト追加。その他の項目は未対応）（R1-MCP-004〜006, R4-SCRIPT-005〜009,
    R2-DATA-009〜011, R1-SUPPLY-003）
  - 機微パス判定、タイムアウト clamp、テストの辞書順依存、エラー型丸め、
    arm64 apt ミラーの HTTP など。

## 重複の吸収（同一根因の統合）

- シークレット継承: R1-SEC-001 + R1-MCP-001 → P0-5
- ADR-0001 の OCI 矛盾: R2-ARCH-004 + R2-DEBT-005 + R5-ADR-003
  + R5-ADR-004 → P3-1
- OKF 形式違反: R5-FMT-001 + R5-ADR-001 → P1-10、
  R5-FMT-002 + R5-ADR-002 → P1-10 内
- 未使用 / 矛盾依存: R1-SUPPLY-002 + R2-DEBT-001/002/003/008
  + R2-DATA-004 → P2-6
- CI 回帰テスト未接続: R4-SCRIPT-001 + R3-DR-006 + R4-GATE-006
  + R4-SPEC-010 → P1-8 / P2-4
- PII テスト欠落: R2-DATA-008 + R4-TEST-001 → P2-1
- kill switch: R2-UI-001 + R3-RUN-002 → P0-4
- `verify-backup` の read-only 矛盾: R3-DR-014 + R3-RUN-001 → P3-3
- デプロイ `-e` 欠落: R1-SEC-008 + R1-SUPPLY-001 → P2-8
- UI CI 未配線: R2-UI-005 + R5-RULES-001 → P2-3
- テスト仕様レイヤ欠落: R4-SPEC-001〜007 → P1-9（1 根因に統合）

## 妥当性検証（却下・修正・Runtime Proof）

- **却下**: R2-DEBT-009「casbin + cedar は重複」→ 意図的な二段評価
  （Casbin 高速 RBAC → Cedar ABAC）であり負債ではない。
- **前提修正**: 委譲時に「ADR-0002 が `adr/` に無い」と想定したが、
  `adr/0002-runtime-agnostic.md` は実在（R5-ADR-007）。さらに
  `status: accepted` で OpenCode 固定を置換している点が新たな指摘
  （R5-RULES-003）に置き換わった。
- **誤検知として却下**: `remaining-tasks.md:57` の「ADR-0004 に準拠」は
  社内 ADR への言及で、外部規格への過剰主張ではない（R4-SPEC-008）。
- **Runtime Proof で確認**: otelcol `0.0.0.0`（R1-SEC-002）、
  `.gitignore` の一致範囲（R1-SEC-005）、`set -uo pipefail`
  （R1-SEC-008）、CI `extra_gates` にテストスクリプトが無い
  （R4-SCRIPT-001）、`proptest` / `mockall` 未使用（R4-TEST-003）、
  UI テスト 7 件 pass（ci-local ログ）。
- **Runtime Proof で実害なし**: `prost-build 0.13` は未使用宣言で
  Cargo.lock は 0.14.4 に解決、`cargo check` 成功（設定上の矛盾のみ）。
  `aws-sdk-s3` / `aws-config` も lock 未混入（R2-ARCH-006）。
  doc test / feature 組み合わせは現状該当なし（R4-TEST-005）。
- **外部事実のため未確認**: 共有ワークフロー `rust-gates.yml@v1` の内部、
  OTel 0.33/0.34 の互換性、Qdrant snapshots API の返却順序、
  実機の `/etc/jarvis` 権限・timer 状態・FW 到達性、
  `JARVIS_POLICY_APPROVAL_TOKEN` の本番設定値（bws 管理）。
- **失敗エージェントのカバー**: `r1-sec-privacy` / `r4-test-ui` /
  `r3-ops-systemd` / `r3-ops-observability` / `r5-doc-drift` /
  `r5-doc-cross` / `r2-arch-event` は途中失敗。いずれも他エージェントの
  指摘（PII= P2-1、UI=P0-4/P2-3、systemd=P2-9、otel=P0-6/7、
  drift=P3-2/3、event=R4-TEST-002）で概ねカバー。
  **未カバー**: 環境適合（R3-ops-env、Kubuntu / GPU / ポート競合）のみ。
  次回レビューで補うこと。

## 追加調査（unknown-tasks-survey との重複吸収、2026-11-17）

[docs/integration/unknown-tasks-survey.md](docs/integration/unknown-tasks-survey.md) の5観点並列調査（49件入力→35件採用）と本 TODO を照合し、重複15件＋部分重複10件を吸収した。以下は本 TODO に未掲載だった10件で、親エージェントが Runtime Proof で確認した高優先度3件を先頭に置く。

### 高優先度（新規）

- [x] **P0-9** drift 検査の allowlist 欠落 fail-open（2026-11-17 対応済み: 配備版 allowlist 欠落時に `MISSING (deployed)` + danger=1。回帰テスト追加）（A-H2）
  - `infra/scripts/check-deploy-drift.sh:19-31`：配備版 allowlist が無いと allowlist 検査と `web.fetch` danger 判定ごとスキップされ、`drift`/`danger` が立たない。欠落時に `MISSING` + `danger=1` を設定し回帰テストを追加。
- [ ] **P0-10** kill switch 既定トークン `changeme` を起動時拒否（A-H3）
  - **2026-11-18 現状**: opt-in strict ガードは実装済み（一部対応）。
    `require_kill_switch_admin_token`（既定 `false`）が `true` のときだけ
    既定トークンで起動を拒否し、既定のままなら従来どおり warn のみで継続する。
    **本番有効化は残る**: `true` 化はトークン注入を値を見ずに確認した後に
    判断する。運用手順は
    [local-operations-runbook.md](docs/integration/local-operations-runbook.md)
    の P0-10 節を参照。
  - 根拠: `rust/jarvis-policy/src/config.rs:38`（opt-in フィールド）、
    `config.rs:66`（既定値 `PLACEHOLDER_KILL_SWITCH_TOKEN` = `"changeme"`）、
    `rust/jarvis-policy/src/lib.rs:35-48`（true のときのみ拒否、既定は warn）。
    ガードの既定値は `false` なので既存デプロイの起動動作は変わらない。
- [ ] **P0-11** `store_memory` に PII フィルタ未適用（A-H4）
  - **2026-11-18 判断記録済み（未実装）**: [ADR-0002](adr/0002-runtime-agnostic.md)
    の「記録（未決定）: PII の保存時扱い」に、PII フィルタは LLM 送信前の
    `build_context` にのみ適用し、`store_memory` は生の `content`/`metadata`
    を保存する旨を明記。保存時マスキングは復元不能・検索品質低下の根拠。
  - **残る判断**: 保存時暗号化・フィールド単位マスキング・保持期限の要否は
    未決定（A-M19 / P2-1 と同時判断）。判断を確定させない。

### 中・低優先度（新規）

- [x] **P1-11** jarvis-event の unbounded channel（A-M2）
  - **2026-11-18 完了**: bounded(1024) + try_send + warn&drop に統一。
    sources.rs の型変更で unbounded ブリッジも除去済み（grep 0件）。
  - （解消済み）旧状態: `normalizer.rs` の unbounded channel と send 失敗握り潰し。bounded(1024) + try_send + warn&drop を導入。
- [ ] **P1-12** Webhook 4ルートに認証・署名検証なし（A-M3）
  - **2026-11-18 判断記録済み（未実装）**: [設計記録](docs/integration/remaining-tasks.md)
    に、loopback bind + Tailscale の信頼境界を前提に当面共有トークン /
    HMAC 署名は導入しない旨を明記。実装・設定変更は行っていない。
  - **残る判断**: 導入要否は実機の公開範囲決定後。Tailscale 外への公開や
    リバースプロキシ経由の外部受取が発生した場合は、認証・署名検証と
    レート制限を再評価する（実機の公開範囲は未確認）。
- [x] **P2-12** drift 検査対象が Cedar 系3ファイルに及ばない（2026-11-18 対応済み: policies.cedar / cedar_schema.cedarschema / pii_patterns.toml を cmp 対象へ追加。配備版パスは env で上書き可能、回帰テストで stale schema 検出を確認）（A-M8）
  - `check-deploy-drift.sh:19-47` は binary/allowlist/`casbin_policy.csv`/unit のみ。`policies.cedar`・`cedar_schema.cedarschema`・`pii_patterns.toml` を `cmp` 対象へ追加（P0-9 と同じ枠組み）。
- [x] **P3-8** 承認ボタンのキーボード操作・アクセシビリティ属性（A-M15）
  - **2026-11-18 完了**: focusable / tab_stop / Role::Button / aria_label /
    Tab・Shift+Tab ナビを実装。gpui-pre 0.3.12 が Enter/Space を ClickEvent へ
    合成するため on_key_down は二重送信防止のため追加していない。
  - `ui/src/approvals.rs:172-196` が `div` + `on_click` のみ。`role`／Tab フォーカス／Enter・Space 決定を追加（P0-8 の状態管理と同じ箇所）。
- [x] **P3-9** policy 保留承認を ntfy へ橋渡し（A-M17）
  - **2026-11-18 判断: 実装見送り**: 公開 ntfy トピックへの機微 `context`
    送信リスク。非公開トピック／id・resource のみの契約が確定次第、再検討
    （[unknown-tasks-survey.md](docs/integration/unknown-tasks-survey.md) CFG-2）。
  - `cordis.patch.yml:75-89` は DSH permission prompt 用で jarvis-policy キューとは別系統。`failOnStartupError: false`（OPS-1）と整合させ、ペイロードに機微引数を含めない設計で追加。
- [x] **P3-10** `#[allow(dead_code)]` の未使用クライアント（2026-11-18 対応済み: Normalizer 構造体から未使用の ha/android/policy/opencode/context フィールドを削除し `#[allow(dead_code)]` も除去。クライアントは spawn タスクへ引数で渡す形に維持）（A-L4）
  - `normalizer.rs:11` struct 全体に属性、`:13-17` の `ha_client`/`android_client` 等は参照なし。削除するか `sources.rs:70,265` の WebSocket 取得へ配線。
- [x] **P3-11** ADR-0002 残課題を ID 化（A-L8）
  - **2026-11-18 判断**: ADR-0002 §Status rationale に「ACP session / doctor の
    自動契約テストは未実装（段階移行の範囲外）」と明記済み。範囲外固定として
    判断を記録し、新規タスク化はしない（必要になった時点で起票）。
  - `adr/0002-runtime-agnostic.md:65`「ACP session / doctor の自動契約テストは未実装」。範囲外固定とするか残タスク登録して判断を記録。
