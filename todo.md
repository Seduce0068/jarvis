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
  - **2026-11-19 設計調査**: 3案を比較し **案A（bws run 経由で注入）を推奨**。
    otelcol.service に `EnvironmentFile=-/etc/jarvis/bws.env` を追加し、
    `bws run --project-id <ID> -- otelcol-contrib` で起動。値は bws vault に
    集約し repo/config/ディスク平文へ置かない。既存4サービスと同じ既知経路で
    repo 側変更が小さい。ローテーションは bws 更新＋otelcol 再起動。
  - **ユーザー確認が必要**: (1) Grafana へのテレメトリ送信継続とデータ範囲、
    (2) Basic 認証の API key スコープ・ローテ周期、(3) 既存 bws project に追加
    するか otelcol 専用 project を新設するか、(4) bws 側 secret の正確な名前、
    (5) `GRAFANA_CLOUD_OTLP_ENDPOINT` の供給元、(6) 再起動による数秒の送信停止
    が許容か、(7) 実機 config と repo テンプレートの同期方針。
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

- [x] **P1-1** ベクトル検索の順序を保持（2026-11-19 完了: `search_vectors` が `(id, score)` を返し `attach_scores()` で `MemoryEntry.score: Option<f32>`（serde default）へ付与。proto に加算フィールド、HTTP/gRPC DTO も後方互換でスコアを返す。legacy JSON 回帰含む純粋テスト5件追加）（R2-DATA-001）
  - `ScoredPoint.score` を破棄し `WHERE id = ANY($1)`（ORDER BY なし）で
    再取得 → ランキングが失われハイブリッド検索が ID 突き合わせに退化。
- [x] **P1-2** PG / Qdrant 二段書き込みに補償を追加（2026-11-19 対応済み: `pending_embeddings` outbox を新設し Qdrant 失敗時に id/content/metadata/ vector を記録、起動時 reconcile で再 upsert・成功時のみ削除。回帰テスト5件追加）（R2-DATA-002）
  - Qdrant upsert 失敗時に PG だけ残り、再送でも自己修復されない。
    outbox または起動時整合性チェック＋再埋め込み。
- [x] **P1-3** マイグレーションをバージョン管理へ移行（2026-11-19 対応済み: 毎起動 `batch_execute` を `sqlx::migrate!()` へ移行、既存 DB は 001 が no-op で version 記録後に 002 適用。新規依存なし・baseline 手順をコメント明記）（R2-DATA-003）
  - 毎起動 `batch_execute("001_initial.sql")` で `IF NOT EXISTS` のみ、
    `_sqlx_migrations` 不在。sqlx migrate またはデプロイ時 CLI に分離。
- [ ] **P1-4** バックアップの保全先を外部化（R3-DR-001）
  - `/var/lib/postgresql/jarvis-backups` は本番データと同一親ディスクで
    SPoF。最低 1 世代を別媒体へ。RPO/RTO も併せて決定。
- [ ] **P1-5** timer を実機で有効化し復元演習へ（R3-DR-004, R3-DR-005）
  - **2026-11-19 一部対応**: 実機 timer 導入完了（ユーザー実行: `sudo -n bash infra/scripts/install-backup-timer.sh --apply` exit 0）。`active` / `enabled`、次回発動 2026-11-20 00:25:56 JST。手動バックアップ1回実行し整合性検証 **ok=30 fail=0**（COMPLETE marker / postgres footer / qdrant tar / config 8ファイルすべて OK）。配置先 `/etc/systemd/system/jarvis-backup.{service,timer}` と `/opt/jarvis/scripts/backup-local.sh`（いずれも root:root）をエージェントが読み取り確認。
  - **残る対応**: 隔離先での実復元演習は未実施。
  - （旧状態）repo 側は完了、実機は未導入 / inactive。隔離先での実復元検証も未実施。
    `make timer-status` → `sudo ... install-backup-timer.sh --apply` →
    確認 → 復元演習の順（ユーザー実行が必要）。
- [ ] **P1-6** バックアップを暗号化・署名し保持期間を設定（R3-DR-002,
    R3-DR-003）
  - manifest は平文 SHA-256 のみ、署名なし。`--retain-days` は opt-in で
    unit が渡していない。ディスク枯渇と改ざん検知不能のリスク。
- [x] **P1-7** 共有ドメインモデルを一本化（2026-11-19 全スライス完了: core 正規形＋policy re-export、mcp-gw の `outcome_from_wire()` 集約、UI の `decision_from_json()` / `request_to_core()` 境界変換、旧 root 型と TryFrom の削除。UI の独立 workspace に jarvis-core path dep 追加、ui-test 22件 pass。競合で壊れた UI の legacy 2-variant テストは「明示拒否」へ更新）（R2-ARCH-001, R2-ARCH-005,
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

- [x] **P2-1** PII フィルタと normalizer にテスト追加（2026-11-19 完了: pii.rs は from_config/再帰 redact/キー redact/衝突一意化/深度ガード、normalizer.rs は変換7件＋emit/overflow/本文非漏洩/相関ログ含む15件。 policy deny 分岐のモック差し替えは seams が無いため未検証で別タスク候補）（R4-TEST-001,
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
- [x] **P2-3** UI を CI へ組み込み（2026-11-17: ci.yml extra_gates に `make ui-test` を追加、7 tests 成功。**2026-11-20 訂正・実装**: 旧完了記録の「UI clippy の undocumented_unsafe_blocks は既に付与済み」は監査 I68-B で実装と不一致（Makefile:45,114 に無く、`ui/Cargo.toml` は独立 workspace で workspace lints 未継承）。`Makefile:45`（ci-local）と `Makefile:114`（ui-test）へ `-D clippy::undocumented_unsafe_blocks` を実際に付与し、記録と実装を一致させた）（R2-UI-005, R5-RULES-001）
  - `ui/` は独立 workspace で CI 対象外。amd64 ジョブで `make ui-test`。
    UI clippy へ `-D clippy::undocumented_unsafe_blocks` を付与（2026-11-20 実装、Makefile:45 / :114）。
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
- [x] **P2-8** デプロイスクリプトへ `-e` と HTTP ステータス検証（2026-11-19 完了: HTTP 200 検証に加え `rustup target add` 失敗と必須スクリプト（backup.rs / restore.rs）欠落を warn+continue から `bail!` の fail-fast へ変更。dry-run exit 0 とヘルス fail-closed は維持、deploy rust-script pass）（R1-SEC-008,
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
- [x] **P2-13** pre-commit を主軸・CI を最終ゲートとする二段構成を完成（2026-11-20 監査 I68-B 対応: `hooks.json` の pre-commit に `make secret-scan`（`|| true` 無し・値非出力の `ci/scripts/secret_scan.py`）を追加しシークレット検出で fail-closed。Markdown lint は docs/ の baseline 456 issues（P3-4）が残るため `|| true` を維持し、その方針を description に明記。`.github/workflows/ci.yml` の `clippy_args` に `-D warnings` を追加。check/test の `--locked` は共有ワークフロー `rust-gates.yml@v1` 内部で未確認のため呼び出し側では設定せずコメントで現状を記録。git hook の実インストール（`git config core.hooksPath`）は行わず設定ファイルのみ編集）（共通指示 §4 / §15, R4-GATE-006）

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
- [x] **P3-7** マイナーな設定・静的ガード（2026-11-19 完了: `secret_scan.py` の SAFE_PATHS 免除を前方一致から完全一致へ（look-alike ファイルの fail-open を遮断）、`review-run.sh` の dsh バックエンド引数順のコントラクトテスト2件追加。残る提案: TOML 構文 gate（Python tomllib 利用可否は未確認））（R1-MCP-004〜006, R4-SCRIPT-005〜009,
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
  - **2026-11-19 一部対応**: 空／未設定トークンを `config.rs::validate_kill_switch_admin_token()` と `kill_switch.rs::hash_matches()` で無条件 fail-closed 化（実起動経路 `load_from_path` にも検証を追加、全トークンゲートで空提示を拒否、回帰6件）。プレースホルダ `changeme` の拒否は依然 opt-in ガード依存（warn-only 互換を維持）。ガード既定値 false→true の反転は本番トークン注入確認後の運用判断として残る。
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
  - **2026-11-19 設計調査**: 4ソースの送信実体は repo 内で未確認。案A（4ルート共通の `webhook_token`、ヘッダ `x-jarvis-event-token` 再利用、未設定なら従来どおりオープン）を最小案として推奨。Gmail/Calendar が Pub/Sub push なら OIDC JWT 検証が別途必要。ハードニング候補: レート制限・timestamp/nonce・HMAC・ボディサイズ上限・strict fail-closed・401/429 メトリクス。
  - **ユーザー確認が必要**: 実機 8085 の公開範囲、4ルートの実際の送信者、共通/ルート別 secret の選択、admin_token からの暗黙継承の有無、HMAC 等を今すぐ入れるか、BWS secret 名とローテ方針、strict モード要否。
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

## 追加指摘（swarm コンボ 2026-11-18）

### 実施記録

- 構成: `swarm-project-review` + `swarm-todo-discovery` を R1〜R5 並列実行。
  初回 80 エージェント（観点 4 × 対象 4 × 5 ラウンド、深度 1）で 695 件取得したが、
  tool 出力制限で R2 以降が欠落したため、コンパクト化して再取得
  （40 エージェント、190 件、上位 100 件を返却）。**合計 120 エージェント起動**。
- 集約: 同一根因を 1 件へ統合（例: approval_api_token 未配線 5 件、
  gRPC unwrap 5 件、RegisterMcpServer 4 件）。`confirmed` は親が
  `path:line` を実読して確認。既存 TODO との重複は追記せず、
  完了記録の矛盾のみ「訂正」として明記。
- 却下: ADR-0002 の「PII 保存時マスクしない」記録と同一の
  `store_memory` 指摘、既存 P0-1/P0-3 と同一の principal 申告、
  Casbin+Cedar の「重複」主張（意図的な二段評価）は採用しない。
- 未配線の参照実装 `ci/review-gate.yml` に関する指摘は、方針確定まで
  既存 P2-2 の判断（安全版のみを配線経路とする）を維持し、優先度を中へ留める。

### 既存記録の訂正

- [x] **P0-2 の「対応済み」を撤回し本番配線を実装**（2026-11-18 対応済み: `lib.rs:41-46` を `ApprovalClient::new_with_token(config.policy_http_addr, config.approval_api_token)` へ結線。トークン値は差分に不含。全ワークスペース 141 tests pass / clippy exit 0） — 根拠
  `rust/jarvis-mcp-gw/src/lib.rs:41-43` / R1-AR01,R1-CF01,R1-AP01,R2-FG1 /
  重大度 high / confirmed。2026-11-17 の完了記録は誤り。本番 `McpGateway::new`
  は依然 `ApprovalClient::new(...)`（トークンなし）で、
  `new_with_token` は `approval.rs:80` の内部委譲とテスト
  （`:378,399,436`）のみ。grep で全 9 箇所を確認済み。
  policy 側で `require_approval_api_token=true` にすると全承認ツールが
  503 で fail-closed となり機能停止する。

### 優先度 高（critical / high）

- [x] **gRPC `RegisterMcpServer` が無認証で任意コマンドを起動する**（2026-11-19 対応済み: `JARVIS_MCP_GW_ADMIN_TOKEN` を設定項目に追加し、`x-jarvis-grpc-token` metadata を全 gRPC RPC で検証。トークン設定時は `ExecuteTool` も必須、未設定時は `ExecuteTool` は従来動作を維持し `RegisterMcpServer` は fail-closed（Unauthenticated）。定数時間比較、新規認証テスト4件＋config 2件 pass、full tests / clippy / ci-local exit 0、commit `0760de2`） — 根拠
  `rust/jarvis-mcp-gw/src/grpc.rs:38-48`, `proto/mcp_gateway.proto:20-23`,
  `src/gateway.rs:102-117` / R3-AU1,R1-AR01,R1-CF01,R1-AP01 / 重大度 critical /
  confirmed。proto に認証フィールドが無く interceptor も無い。
  allowlist・policy 承認を迂回した任意プロセス常駐の経路。
- [x] **gRPC `ExecuteTool` の unwrap + `panic="abort"` で 1 リクエストが全停止**（2026-11-18 対応済み: `from_str` を `Status::invalid_argument`、`to_string` を `Status::internal` へ変換し公開ハンドラの unwrap を除去。回帰テスト `invalid_args_json_returns_invalid_argument_without_panicking` 追加、targeted 1 pass / full 141 tests pass / clippy exit 0） — 根拠
  `rust/jarvis-mcp-gw/src/grpc.rs:28,34`, `rust/Cargo.toml:105` /
  R2-ER1,R1-AR01,R1-CF01,R1-AP01 / 重大度 high / confirmed。
  `serde_json::from_str(&req.args).unwrap()` を不正 JSON で発火させられる。
  dev/test は unwind のためテストで顕在化しない。
- [x] **kill switch 状態が既定 `None` で再起動時に非常停止が解除される**（2026-11-19 対応済み: opt-in ガード `require_kill_switch_state_path`（既定 false）を追加し、`load()` 内で検証して true 時は未設定なら起動失敗。既定パス変更は実機権限未確認のため見送り。新規テスト7件、policy 67 tests pass） — 根拠
  `rust/jarvis-policy/src/config.rs:72`, `src/kill_switch.rs:159-162` /
  R4-DR1,R1-AR01,R1-CF01 / 重大度 high / confirmed。
  `require_kill_switch_admin_token`（P0-10）とは別の永続化問題。
- [x] **Cedar スキーマ検証が `#[cfg(test)]` 内のみで本番に接続されていない**（2026-11-19 対応済み: `CedarEngine::new` で schema を解決し `Validator::validate(Strict)` を実行、違反時は `bail!` で起動失敗。既存 shipped テストは共通ヘルパーへ集約し維持、新規2テスト追加。policy 67 tests pass） — 根拠
  `rust/jarvis-policy/src/policy.rs:236,353-366` / R1-ST1 / 重大度 high /
  confirmed。起動時に投入されたポリシーは型検査されない。
- [x] **`backup.rs --dry-run` がローカルバックアップを削除する**（2026-11-18 対応済み: `cleanup_local` に `dry_run` を伝播し、true 時は `remove_file` せず削除予定をログ出力。選定条件は従来どおり。`rustfmt --check` / `rust-script --test` 1 pass / CLI help exit 0） — 根拠
  `rust/scripts/backup.rs:94-97,304-327` / R4-DR2,R2-FG2,R1-AR11 /
  重大度 high / confirmed。`cleanup_remote` にのみ dry_run が渡り
  `cleanup_local` は無条件実行。ヘルプの「no actual upload/delete」と矛盾。
- [x] **復元処理が失敗シグナルを伝播せず成功を報告する**（2026-11-19 対応済み: 全外部コマンドで `status.success()` を検査し失敗時 `bail!`。gunzip→psql ストリームは両プロセスの終了コードを検証。成功ログは全検証後に1行のみ。Qdrant API 復元・dry-run 非破壊性は維持。rust-script --test 5 tests pass） — 根拠
  `rust/scripts/restore.rs:117,257-259,285-291,326-329,390-399` /
  R1-AR2,R2-ER2,R4-DR2,R4-DR3 / 重大度 high / confirmed。
  gunzip/psql 終了コード・Qdrant upload の HTTP ステータス・git clone 成否を
  検査せず「Restore completed」を出力。
- [ ] **jarvis-event が jarvis-context/policy をインプロセス化しサイドカー境界を侵害** — **2026-11-19 スライス1-2 対応済み: トレイト seams に加え、context.proto へ source/timestamp を加算追加、`GrpcContextProvider` と `ContextMode`（既定 in_process／opt-in grpc）を実装。source/timestamp の wire 往復を実ネットワークなしで検証。残るスライス: 既定 grpc 化と型依存削除／policy 型依存削除＋ADR 改訂** — 根拠
  `rust/jarvis-event/Cargo.toml:47-50`, `src/lib.rs:30-33`,
  `src/normalizer.rs:104-116` / R1-AR2,R1-AR11,R1-CF11 / 重大度 high / confirmed。
  event 起動に PG/Qdrant と context 設定が必須化され障害分離を喪失。
- [ ] **承認の決定・保留がメモリのみで永続化・通知・監査が無い** — **2026-11-19 スライス1-3＋強化対応済み: 再接続（接続世代の破棄＋上限3回リトライ、URL 非ログ）、専用 `ApprovalError::Persistence`（HTTP 503・固定本文、QueueFull と区別可能）。残るスライス: opt-in ロールアウトのみ** — **スライス1-3: `reconcile_pending()` を `PolicyEngine::new()` から呼び、未決定行を待機者なしで再登録、期限切れ行を Expired 決着＋audit（Noop では no-op）。R4 で `record_request` 失敗を fail-closed 化（メモリ登録取消し＋QueueFull/503）、`mark_expired` 失敗を warn 可視化。残るスライス: opt-in ロールアウト／再接続ロジック／専用 Persistence エラー型** — **スライス1-2: `ApprovalPersistence` seams＋`PgApprovalPersistence`（CAS＋audit INSERT を同一 tx、コミット後のみ watch::send）。003 migration（decided_by 追加、`IF NOT EXISTS`）、mock でコミット前不发火／rollback 非公開／二重 decide を検証。pg_url 設定時のみ注入、未設定は Noop 不変。`Client::transaction` が `&mut` を要求するため Mutex でラップ。残るスライス: 起動リコンサイル／opt-in ロールアウト。DB エラーは範囲外の http.rs を壊さないため既存 `QueueFull`（503）へマップ（専用バリアントは将来課題）** — **設計決定: 案A（jarvis-context と同一 PostgreSQL を共有、001 の approval_requests / audit_log を再利用、003 で decided_by 列を additive 追加）を推奨。4スライス（seams → PgPersistence → 起動リコンサイル → opt-in ロールアウト）。watch channel は維持し DB ポーリングしない。コミット前に audit INSERT を同じ tx で書き、コミット後のみ watch::send。再起動で復活するのは記録と UI 可視性のみで HTTP 待機者は復活しない** — 根拠
  `rust/jarvis-policy/src/approval.rs:5-6,41-45` / R1-ST1,R1-AR01 /
  重大度 high / confirmed。再起動で保留が消え、誰がいつ承認したか残らない。
- [x] **承認リクエストの id・タイムスタンプがクライアント申告で黙ってすり替わる**（2026-11-19 対応済み: id と requested_at はサーバ生成、expires_at は server timeout でクランプし生成値をレスポンスで返却。不正値は 400 で拒否。回帰テスト6件追加、policy 73 tests pass） — 根拠
  `rust/jarvis-policy/src/http.rs:43-51,216,221-222` /
  R2-ER1,R1-AP01,R1-ST01 / 重大度 high / confirmed。
  `Uuid::parse_str(...).unwrap_or_else(new_v4)` と `unwrap_or_default()`。
- [x] **`web.fetch` の機密ファイル判定がファイル名末尾のみ**（2026-11-19 対応済み: `reject_sensitive_path` を canonicalize 済みパスの全 components 走査へ拡張し `.ssh` / `.aws` / `.gnupg` / `.kube` / `.config/gcloud` 配下や `id_rsa*` / `credentials` / `known_hosts` を拒否。新規2テスト追加、sandbox 12 tests pass） — 根拠
  `rust/jarvis-mcp-gw/src/sandbox.rs:250-267` / R3-AU1,R1-ST01 /
  重大度 high / confirmed。`.ssh/id_rsa` や `.aws/credentials` が
  filename ベースの拒否リストをすり抜ける。
- [x] **イベントキュー溢れ時にドロップしつつ `success: true` を返す**（2026-11-19 対応済み: HTTP は 503、gRPC は `RESOURCE_EXHAUSTED` を返す。既存 `emit()` の warn&drop は維持し、ハンドラ側で `event_sender().try_send()` の結果を検査。回帰テスト4件追加、event 29 tests pass） — 根拠
  `rust/jarvis-event/src/normalizer.rs:21-37`, `src/http.rs:133-139`,
  `src/grpc.rs:35-36` / R2-ER2,R1-AR11 / 重大度 high / confirmed。
- [x] **登録済み upstream MCP サーバがメモリのみで再起動時に消える**（2026-11-19 対応済み: `McpGatewayConfig.upstreams`（TOML/env 対応）を追加し `main.rs` 起動時に `register_mcp_server` で登録、失敗時は起動失敗（fail-closed）。実行時 RPC 登録は維持。新規テスト4件、mcp-gw 64 tests pass） — 根拠
  `rust/jarvis-mcp-gw/src/gateway.rs:50,102-120`, `src/main.rs:44-65` /
  R4-DR1,R1-ST01 / 重大度 high / confirmed。設定駆動の起動時登録が無い。
- [x] **jarvis-context の memory/context API に認証・認可が無い**（2026-11-19 対応済み: `admin_token`（`JARVIS_CONTEXT_ADMIN_TOKEN`）を追加し全 `/v1/*` と `/metrics` を `x-jarvis-context-token` でゲート。**R4 判断で `/metrics` は Prometheus スクレイプ互換のため認証除外に変更**。未設定時は従来どおり。context 35 tests pass） — 根拠
  `rust/jarvis-context/src/http.rs:55-64` / R3-AU2 / 重大度 high / confirmed。
  `/v1/context/build`, `/v1/memory`, `/v1/memory/search` が認証レイヤーなし。
- [x] **jarvis-event の emit API が無認証で主体が `jarvis` にハードコード**（2026-11-19 対応済み: `admin_token`（`JARVIS_EVENT_ADMIN_TOKEN`）を追加し HTTP は `x-jarvis-event-token` / gRPC は metadata で検証。定数時間比較、health/metrics は除外、Debug redaction。未設定時は loopback 互換。event 40 tests pass。固定 principal "jarvis" は runtime 非依存契約として維持） — 根拠
  `rust/jarvis-event/src/http.rs:130-139` / R3-AU2 / 重大度 high / confirmed。
- [x] **`build-selfhost.sh` が OpenCode ポートを 49374→49375 に書き換え unit と乖離**（2026-11-19 対応済み: 当該 sed 置換1行を削除。スクリプトに 49375 残存なし、`opencode.service` と AGENTS.md の port 表が 49374 で統一、`bash -n` OK） — 根拠
  `infra/scripts/build-selfhost.sh:43` / R2-FG3 / 重大度 high / confirmed。
  AGENTS.md の port 表（49374）とも不一致。
- [x] **`secret_scan.py` が docs/ を safe path に格下げして実資格情報をゲート失敗にしない**（2026-11-19 対応済み: `SAFE_PATHS` から `docs/` を除外し、PLACEHOLDER 判定に `replace[_-]?me` を追加。値非出力契約を維持、`make secret-scan` / legacy guards 22 tests pass） — 根拠
  `ci/scripts/secret_scan.py:32-41` / R5-VU4 / 重大度 high / confirmed。
- [x] **`#[instrument]` が HTTP リクエスト本文全体をログへ記録し機微情報を漏洩**（2026-11-19 対応済み: check/request/approve/deny の `#[instrument(skip(engine, req))]` 化で本文をスパンから除外。health/metrics/check の認証スコープ不変） — 根拠
  `rust/jarvis-policy/src/http.rs:198-201` / R5-VU1 / 重大度 high / confirmed。
- [x] **`install-backup-timer.sh` が `backup-local.sh` を無バックアップで上書きしロールバックで削除**（2026-11-19 確認: `script_dst.bak-$ts` への退避と rollback での復元が既に実装済み（:62, :48）。src/dst/unit の3ファイルが同じ枠組みで処理されている。コード変更不要） — 根拠
  `infra/scripts/install-backup-timer.sh:52` / R2-ER3 / 重大度 high / confirmed。
- [x] **bootstrap 再実行が本番 `config/jarvis.env` をトランケートし秘密情報を失う**（2026-11-19 対応済み: `write_private_if_absent` を追加し `create_new(true)` で原子的に存在確認、既存ファイルは警告のみで一切変更しない。dry-run / `--help` / Termux dry-run / `make lint` pass） — 根拠
  `scripts/bootstrap/bootstrap.rs:183` / R4-DR4 / 重大度 high / confirmed。
  `SERVER_ENV`（REPLACE_ME プレースホルダ）で無条件書き込み。
- [x] **`Makefile clean` が上書き可能な `JARVIS_ROOT` を検証せず `rm -rf` を実行**（2026-11-18 対応済み: `/`・`$HOME`・`.`・`..`・相対パス・空を exit 2 で拒否。実機テストで全危険値が拒否され `/bin` 無傷を確認） — 根拠
  `Makefile:6,119-123` / R4-DR4 / 重大度 high / likely。
  `make clean JARVIS_ROOT=/` 等で破壊的范围が拡大。

### 優先度 中

- [x] `sandbox_type` / `sandbox_config` が `let _ =` で読み捨てられ no-op — 2026-11-19 対応済み: legacy パース互換を維持したまま doc に ADR-0002 と未使用を明記し、明示設定時に load で warn を発行。既定 none/{} ではノイズなし / `src/sandbox.rs:29-37` / R1-ST1,R1-CF01 / confirmed
- [x] 承認ウィンドウが gateway 側 300 秒固定で policy 設定と drift — 2026-11-19 対応済み: `JARVIS_MCP_GW_APPROVAL_WINDOW_SECS` を追加し `expires_at` と HTTP timeout を単一値から算出。policy 側は共有定数 `DEFAULT_APPROVAL_TIMEOUT_SECS=300` と compile-time assert で同期。回帰テスト7件 / `src/approval.rs:21-31`, `policy/src/config.rs:25` / R2-FG1,R1-CF01,R1-ST01 / confirmed
- [x] HTTP と gRPC で context 正規化・エラー契約が不一致（内部エラーが空 context / 200 deny に潰れる）— 2026-11-19 対応済み: `without_nulls` を `pub(crate)` の単一実装とし両ハンドラの `is_allowed` 直前に適用、client 側の重複変換を削除。HTTP/gRPC 間の同一判定回帰テスト3件追加、policy 83 tests pass / `policy/src/grpc.rs:27`, `policy/src/http.rs:203-207`, `client.rs:80-86` / R2-ER1,R1-AP01 / confirmed
- [x] 設定ファイルが CWD 相対の `config/*.toml` — 2026-11-19 対応済み: 既定を `/etc/jarvis/{policy,mcp_gateway}.toml` へ変更し `JARVIS_{POLICY,MCP_GW}_CONFIG_PATH` で上書き可能。smoke スクリプトも env 追随。実機は Env provider で設定供给のため既定ファイル不在でも起動可 / `policy/src/config.rs:51-58`, `mcp-gw/src/config.rs:38-46` / R1-CF01 / confirmed
- [x] 同名ツールの upstream 選択が HashMap 反復順依存 — 2026-11-19 対応済み: 登録時に他サーバのツール名衝突を検出して拒否（最初の登録が優先）。check と insert を単一 write ガード内で実施し競合も防止。回帰テスト3件 / `mcp-gw/src/gateway.rs:79-89,143-156` / R1-AR01 / confirmed
- [x] PII フィルタが JSON キー名を対象外のまま — 2026-11-19 対応済み: PII パターンに一致する object キーも `[REDACTED]` に置換（通常キーは保持、部分一致対応、深度 128 ガード）。回帰テスト5件追加、既存 fixture を非 PII キーへ更新 / `context/src/pii.rs:204` / R1-ST2 / likely（既存 P2-1 と別観点）
- [x] `embedding_model` は設定可能だが Qdrant 次元が 384 固定 — 2026-11-19 対応済み: `embedding_dim`（既定 384、`JARVIS_CONTEXT_EMBEDDING_DIM`）を追加しコレクション生成に使用。既存コレクションは起動時に実次元を読み取り不一致なら明確なエラー。回帰テスト8件追加、context 53 tests pass / `context/src/builder.rs:253-255` / R1-CF11,R1-AR2 / confirmed
- [x] PostgreSQL 既定 URL に固定パスワード — 2026-11-19 対応済み: 既定値を `postgres://localhost:5432/jarvis` へ変更し認証を libpq へ委譲。LEGACY_TOML もパスワードレス化、回帰テスト3件追加 / `context/src/config.rs:54` / R1-CF11 / confirmed
- [x] `CheckResponse` が `allowed` 真偽値のみで拒否理由を返さない — 2026-11-19 対応済み: proto に後方互換の `reason = 2` を追加し HTTP/gRPC 両ハンドラが固定語彙（allowed/kill_switch/rbac_denied/cedar_denied/evaluation_error）を返す。jarvis-core 共有モデルも serde default で対応 / `policy/proto/policy.proto:19-21` / R1-AP01 / confirmed
- [x] MCP `tools/list` が upstream inputSchema を破棄し cursor も無視 — 2026-11-19 対応済み: 登録時に upstream の inputSchema を保持し `tools/list` で返却（ネイティブ優先・汎用フォールバック維持）。cursor 無視・nextCursor 非発行を契約として明記、回帰テスト3件追加 / `mcp-gw/src/mcp_server.rs:59-99,120-126` / R1-AP01,R5-DC1 / confirmed
- [x] otelcol 設定の配布経路がビルドスクリプトから欠落 — 2026-11-19 対応済み: `otelcol-config.yaml` を `.bak-<ts>` 退避付きで `install -o root -g root -m 0644 /etc/jarvis/` へ配置。bash -n OK / `infra/scripts/build-selfhost.sh:26-28` / R1-AR3 / confirmed（既存 P0-6 と関連）
- [x] Grafana の job セレクタ / service ラベルが scrape 設定と不一致 — 2026-11-19 対応済み: ダッシュボード側を実在 job 名（`jarvis-*` / `qdrant` 等）へ統一し `service` ラベルと `jarvis/` 接頭辞を除去。otelcol 側の job_name は時系列継続のため未変更。JSON 有効・旧セレクタ残存0 / `dashboards/jarvis-overview.json:46` / R1-AR3,R5-VU3 / confirmed
- [x] filelog レシーバの入力が空（`/var/log/jarvis/*.log` を誰も書かない）— 2026-11-19 対応済み: journald receiver へ置換し `jarvis-*` unit を対象化、logs パイプラインも接続。子が追加した無効キー `offset_storage_dir` は親が削除し `otelcol-contrib validate` exit 0 / `otelcol-config.yaml:52` / R5-VU3 / confirmed
- [x] `check-shell` が shellcheck 未インストール時に SKIP して成功扱い — 2026-11-19 対応済み: 未検出時に fail-closed でエラー終了。CI runner へ shellcheck を導入するため `.github/workflows/ci.yml` の apt_packages にも追加。shfmt 警告のみの挙動は維持 / `ci/scripts/check-shell.sh:22-33` / R5-VU4 / confirmed
- [x] `check-evidence.sh` が skipped 数・仕様シナリオ数を照合せず合格 — 2026-11-19 対応済み: `Scenario:` 行数と `passed+failed+skipped` を照合し不足時に `SCENARIO_COUNT_MISMATCH`。1シナリオが複数テストへ展開される既存設計のため下限判定を採用、既存6証跡すべて合格（ok=6 errors=0） / `ci/scripts/check-evidence.sh:82-85` / R3-TQ4 / confirmed
- [x] deploy がヘルスチェック失敗を警告のみで完了報告 — 2026-11-19 対応済み: `evaluate_health_checks()` を追加し異常なら `bail!` で非ゼロ終了（全失敗ポートを列挙）。dry-run の skip と 200 は維持。回帰テスト4件追加、deploy rust-script pass / `rust/scripts/deploy.rs:385-389` / R4-DR2 / confirmed（既存 P2-8 と関連）
- [x] `verify-backup.sh` がマニフェスト記載ファイルのみ検査し必須ペイロード欠落を合格させる — 2026-11-19 対応は **COMPLETE-gated な部分修正に過ぎなかった**: `:69` の `if [ -f "$set/COMPLETE" ]` 配下でのみ postgres/qdrant/config-* の実在を必須化し、完全セットの欠落は `REQUIRED MISSING` で失敗。COMPLETE が無い legacy セットは検査自体がスキップされ、`created_at` ヘッダもペイロードも無い manifest だけのディレクトリが exit 0 で通過する fail-open が残存。子の指摘で既存フィクスチャ2件が新チェックで失敗したため親が必須ペイロードを追加し `test-verify-backup` 23 tests pass を記録したが、この23件に fail-open 期待が含まれていたため完了主張は過大。完全修正は Round 65 残り指摘の I69-B を参照 / `infra/scripts/verify-backup.sh:62-79` / R4-DR3 / confirmed
- [x] jq 欠落時にバックアップ整合性検証をスキップして verified と報告 — 2026-11-19 対応済み: apply 時に jq が無ければ `fail()` で非ゼロ終了（verified に到達しない）。dry-run は jq 不要を維持、bash -n OK / `infra/scripts/install-backup-timer.sh:96` / R2-ER3 / confirmed
- [ ] ダウンロードしたリリースバイナリにチェックサム/署名検証が無い — `infra/scripts/setup-selfhost.sh:34-37` / R4-DP3 / confirmed。
  - **2026-11-19 調査**: repo 内に SHA256SUMS / 署名鍵 / 公開ダイジェストは存在せず（glob/grep 0件）、Qdrant / otelcol-contrib / bws / code-server の取得元を照合できない。勝手なハードコードや外部取得は追加せず、未検証の旨と推奨対応（リリース側でバージョン固定の SHA256SUMS + 署名を公開し取得・照合）を setup-selfhost.sh のコメントに明記。情報源が用意されるまで未解決。
- [x] `/health` が固定 `"OK"` を返し劣化を検知できない — 2026-11-19 対応済み: kill switch 発動時とポリシー来源欠落時に 503 `{"status":"degraded","reason":...}` を返す。固定語彙のみで理由文字列・パスは非開示、正常時は 200 維持。新規テスト3件追加、policy 86 tests pass / `policy/src/http.rs:187-189` / R4-DR1 / confirmed
- [x] シグナル処理・ドレインが無くデプロイ時停止で保留作業が失われる — 2026-11-19 対応済み: Ctrl-C / SIGTERM を `tokio::select!` で join! と競わせ停止時に tracer flush 付きで終了。biased で実エラーを優先。完全なコネクションドレインは serve 側の変更が必要で別タスク / `policy/src/main.rs:59` / R4-DR1 / confirmed
- [x] axum 0.7 と 0.8 が workspace 内で二重解決 — 2026-11-19 調査済み: 原因は tonic 0.14.6 が axum 0.8.9 を要求（tower-http 0.6.11 は axum 非依存で仮説は否定）。axum 0.8 更新は4 crate・146参照の破壊的変更、tonic downgrade は gRPC 一式の連鎖変更となるため**現状維持**と判断し、根拠とリスクを `rust/Cargo.toml` のコメントに記録 / `rust/Cargo.toml:79` / R4-DP1 / confirmed
- [x] 配線済み review-gate が PR ではなく既定ブランチを検証 — 2026-11-19 対応済み: 未配線の参照実装 `ci/review-gate.yml` の3ステージ checkout に `refs/pull/<number>/head` を追加（workflow_dispatch は `pr_number` 入力）。auto-merge・権限・認証情報の方針は据え置き、YAML 有効 / R1-AR4 / confirmed（未配線の参照実装は既存 P2-2 の判断を維持）
- [x] レビューゲートのシグナル解析が `grep ... || true` で fail-open — 2026-11-19 対応済み: `fail_closed()` / `count_findings()` を導入し解析エラー・信号ファイル不在で `verdict=UNKNOWN` かつジョブ失敗。merge-gate / auto-merge は APPROVE/CONDITIONAL のみ通過。未配線方針は維持 / `ci/review-gate.yml:145-148` / R2-ER4 / likely（未配線）
- [ ] cloud-init が NOPASSWD root + docker グループ / 固定 DB パスワード — `infra/opentofu/cloud-init.yaml:41-43,133` / R3-AU3 / confirmed（ADR-0004 で未使用のレガシー、削除判断は既存 P3-5 に委譲）
- [x] デプロイスクリプトが全 unit を単一ユーザーへ書き換え — 2026-11-19 対応済み: policy/mcp-gw/event/context の4サービスは User/Group をテンプレート宣言どおり保持し、その他5ユニットのみ従来の置換を適用（認可境界の権限分離を維持） / `infra/scripts/build-selfhost.sh:40-42` / R3-AU3 / confirmed
- [x] `check-deploy-drift.sh` が配備済み Policy/PII ファイル欠落を検知しない — 2026-11-19 対応済み: allowlist と同じ枠組みで `MISSING (deployed)` / `MISSING (repo)` と danger=1 を設定。既存回帰22 tests pass / `infra/scripts/check-deploy-drift.sh:44` / R3-TQ3 / confirmed（既存 P0-9/P2-12 と枠組み共有）
- [x] `install-mcp-gateway.sh` のヘルス確認が HTTP ステータスを検証せずロールバックしない — 2026-11-19 対応済み: `curl -fsS -w '%{http_code}'` で 200 を明示検証し、失敗時は既存のロールバックフローへ。本文/トークンは出力せず、bash -n OK / `infra/scripts/install-mcp-gateway.sh:28` / R4-DR3 / confirmed
- [x] 放棄された承認が pending のまま残りキュー枠を枯渇 — 2026-11-19 対応済み: `Entry.deadline` を保存し `prune()` が期限切れ/放棄エントリを回収して待機側へ Expired 通知。`try_request` 前段で実行。回帰テスト1件追加、policy 88 tests pass / `policy/src/approval.rs:148-162` / R5-VU1 / likely
- [x] web.fetch の SSRF 検証が DNS 解決の TOCTOU で迂回可能 — 2026-11-19 対応済み: 検証済み IP を `resolve_to_addrs()` で reqwest に固定（SNI/証明書検証は元ホスト名を維持）、リダイレクト毎に再検証・再ピン留め。`.no_proxy()` でプロキシ迂回も防止。回帰テスト2件追加 / `mcp-gw/src/sandbox.rs:359-365` / R3-AU1 / likely
- [x] メトリクスが HTTP ステータス別のみで kill switch / 承認の状態が観測不可 — 2026-11-19 対応済み: `jarvis_kill_switch_activated` (0/1) と `jarvis_approval_pending`（保留数）の gauge を追加。`/metrics` スクレイプ時に状態を反射、ラベルゼロで理由/principal/id は非送出 / `policy/src/http.rs:155-169` / R5-VU1 / confirmed
- [x] 登録済み MCP サーバの子プロセス死活を検知せず孤立登録が残る — 2026-11-19 対応済み: `ProcessLiveness` で `try_wait()` を実行前検査し、死亡/状態不明の登録を削除してエラー。`Box<ChildHandle>` 化で clippy large-enum も解消。回帰テスト2件追加 / `mcp-gw/src/gateway.rs:36-42` / R5-VU1 / likely
- [x] Qdrant スナップショット 0 件が warn のみで成功扱い — 2026-11-19 対応済み: `require_snapshot_name()` を追加し空一覧/空名前を `bail!` で失敗扱い。回帰テスト3件追加、backup rust-script pass / `rust/scripts/backup.rs:198` / R1-ST2 / confirmed

### 優先度 低

- [x] バックアップ/リストア CLI が OCI/1Password 前提の説明のまま — 2026-11-19 対応済み: 両スクリプトの module doc / `about` / `long_about` を「レガシー・非推奨、現行は backup-local.sh / restore-local.sh」へ更新。rust-script マニフェストと restore.rs 非推奨ヘッダは維持 / `rust/scripts/backup.rs:31,35,79` / R5-DC2 / confirmed（ADR-0004 準拠の文言へ）
- [x] トークン予算ドキュメントが文字数とバイト数を混同 — 2026-11-19 対応済み: `ESTIMATED_UTF8_BYTES_PER_TOKEN=4` を導入しトークン≠文字数≠UTF-8 バイトを明記、多バイト文字では過大見積もりになる旨も記載。`saturating_mul` でオーバーフロー防止 / `context/src/builder.rs:195-196` / R5-DC2 / confirmed
- [x] OpenCode クライアントのモジュール doc フロー図が実装の認可順序と不一致 — 2026-11-19 対応済み: 実順序（event → context → policy[dispatch 前] → OpenCode → MCP gateway[各呼び出し前再判定]）へ修正。コメントのみ変更 / `event/src/opencode.rs:7-8` / R5-DC2 / likely
- [x] Grafana README のクエリがダッシュボード JSON の実式と不一致 — 2026-11-19 対応済み: 4パネルのクエリを実式へ同期し、長行はコードブロックへ移動して markdownlint MD013 を解消。注意書きも「per scrape job」へ修正、lint 0 issues / `infra/grafana/README.md:14-17` / R5-DC3 / confirmed
- [x] OTel `SdkTracerProvider` が shutdown されずバッチスパンが失われる — 2026-11-19 対応済み: `TracerGuard`（Drop で shutdown）を追加し provider を main 全体へ延長。正常・エラー・早期リターンの全経路で flush、二重 shutdown も無害 / `policy/src/main.rs:27-30` / R4-DP1 / confirmed

### メモ（要判断・却下）

- **要判断**: Casbin allow が Cedar の ABAC/forbid を短絡する
  （`policy/src/lib.rs:113-125` / R1-AR01,R1-ST01,R3-AU1）。既存 TODO の
  妥当性検証では「Casbin 高速 RBAC → Cedar ABAC の意図的な二段評価」として
  重複主張を却下済みだが、「Cedar の forbid が効かない」点は別問題。
  設計意図の確認が要るため未採用。
- **要判断**: `opencode.jsonc:17-28` の jarvis-dev サブエージェント allow-all が
  ルートの秘密情報 deny を迂回しうる（R1-AR4 / likely）。開発用の明示設定か
  セキュリティ境界の侵害かを判断が必要。
- **却下**: `store_memory` の PII マスキング要求は既存 P0-11 と同一。
  ADR-0002「記録（未決定）」の現行挙動を記録済みのため追記しない。
- **却下**: `/v1/execute` / gRPC の principal 自己申告は既存 P0-3 と同一。
- **却下**: `web_max_bytes` キャスト等は既存 P3-7 と同一。
- **未調査**: 初回 workflow の 5 担当（R1-AP21,R1-ST21,R2-ER31,R3-TQ21,R4-DR31）
  は出力不正のため「未レビュー」。再取得で全観点をカバーしたため
  未カバー率は 5/80 = 6.25%（20% 閾値未満）。
- **制約**: 初回結果は tool 出力制限で R2 以降が欠落。再取得は
  review 観点を 1 観点/ラウンドへ縮小したため、R2〜R5 の全観点は
  初回のコンパクト情報と合わせてカバーした（完全網羅ではない）。

## 追加指摘（swarm コンボ 2026-11-19 第2イテレーション）

### 実施記録

- 構成: review と discovery を R1〜R5 で並列実行。**40 エージェント**
  （観点 2 × 対象 4 × 5 ラウンド、深度 1、無制限プロファイル）、
  189 件の生指摘から上位 100 件をコンパクトに返却。出力不正・
  秘密情報による破棄は 0 件。前回の tool 出力制限を避けるため、
  最初からコンパクト出力設計を採用した。
- 重複排除: 既存の未着手 75 件と前回追記（2026-11-18）を
  既知情報として全エージェントへ配布し、同一内容の報告を禁止。
  R6 で既存 P0-3 / P0-10 / P1-2 / P1-7 / P1-12 / P2-1 等と
  同一の指摘を除外した。
- 却下: gRPC データプレーン RPC が `admin_token` 未設定時に
  無認証な点は、2026-11-19 の実装で「互換性のため意図的に
  残した」設計判断なので「要判断」として記録し、障害としては
  採用しない。Casbin/Cedar の短絡は前回同様「要判断」を維持。

### 優先度 高（critical / high）

- [x] **Qdrant 復元が停止後に HTTP upload しており復元不能**（2026-11-19 対応済み: `restore.rs` から `systemctl stop/start qdrant` を削除し、稼働中の upload API へ POST する順序へ変更。`error_for_status()` を追加して HTTP 失敗を検出。rustfmt / `rust-script --test` 2 tests pass / full tests / clippy / `ci-local` exit 0） — 根拠
  `rust/scripts/restore.rs:316-320,326-329` / R4-DR2 / 重大度 critical /
  confirmed。`systemctl stop qdrant` の後に
  `localhost:6333/.../snapshots/upload` へ POST しており、
  スナップショットが取り込まれない。2026-11-19 に導入した
  バックアップ timer の復元演習前に要修正。
- [x] **`backup-local.sh --retain-days` に境界値検証がなく全バックアップを削除し得る**（2026-11-19 対応済み: 引数欠落・非整数・0以下を exit 2 で拒否。実機テスト5パターンすべて拒否、正常値 `7` の dry-run は通過、`bash -n` OK） — 根拠
  `infra/scripts/backup-local.sh:25` / R3-TQ3 / 重大度 high / confirmed。
- [x] **HTTP ツール実行面に認証がなく `admin_token` も効かない**（2026-11-19 対応済み: `x-jarvis-admin-token` ミドルウェアを `/v1/execute` と `/mcp` に追加。トークン設定時のみ必須で未設定時は従来どおり（互換維持）。approval/grpc トークンとは別ヘッダで定数時間比較。回帰テスト4件 pass、full tests / clippy / ci-local exit 0） — 根拠
  `rust/jarvis-mcp-gw/src/http.rs:57-58` / R3-AU1 / 重大度 high /
  confirmed。gRPC 側だけ認証を導入したため `/v1/execute` と
  `/mcp` は無認証のまま。
- [x] **承認要求と承認決定が同一トークンで保護され自己承認が可能**（2026-11-19 対応済み: `approval_admin_token` を新設し request/list/status と approve/deny でトークンを分離。admin token 未設定時は api token へフォールバック（互換＋warn）。定数時間比較、回帰テスト追加。policy http 12 tests / config 11 tests pass） — 根拠
  `rust/jarvis-policy/src/http.rs:84-93` / R3-AU1 / 重大度 high / confirmed。
- [x] **`check-shell.sh` が `set -uo pipefail`（`-e` 無し）で fail-open**（2026-11-19 対応済み: `set -euo pipefail` 化、`git ls-files` 失敗とチェック対象0件を明示 exit 1、shfmt WARN／shellcheck SKIP の警告扱いは維持。実機で通常 exit 0・失敗注入2パターンとも exit 1 を確認、`ci-local` exit 0） — 根拠
  `ci/scripts/check-shell.sh:6` / R2-ER4 / 重大度 high / confirmed。
  チェック対象ゼロや `git ls-files` 失敗が成功扱いになる。
- [x] **`build-selfhost.sh` の再デプロイで policy 設定が入れ子コピーされる**（2026-11-19 対応済み: 既存 `/etc/jarvis/policy` を `.bak-<ts>` へ退避してから `cp -r "${POLICY_SRC}/." "${POLICY_DST}/"` で内容のみ直下へコピー。`bash -n` / dry-run / `ci-local` exit 0。実機 /etc への書き込みは未実施） — 根拠
  `infra/scripts/build-selfhost.sh:26` / R4-DR3 / 重大度 high / confirmed。
  `cp -r .../policy /etc/jarvis/` が既存 `/etc/jarvis/policy` の
  中に `policy/` を作り実効設定が更新されない。
- [x] **`install-backup-timer.sh` のロールバックが有効化済み timer を停止・無効化しない**（2026-11-19 対応済み: `rollback()` の先頭で unit 存在確認後に `systemctl stop` / `disable` を追加（失敗は無視）、その後ファイル復元→`daemon-reload` の順序を維持。成功経路は無変更、`bash -n` / dry-run exit 0） — 根拠
  `infra/scripts/install-backup-timer.sh:63` / R4-DR3 / 重大度 high / confirmed。
- [x] **最新バックアップ選択が pg 系のみで他コンポーネントの欠落を検証しない**（2026-11-19 対応済み: `validate_backup_set()` を新設し選択日付の pg/qdrant/configs 3コンポーネントを事前検証、欠落時は `bail!` で部分復元を拒否。`--dry-run` でも検証。fake rclone で部分セット exit 1／完全セット exit 0 を実機確認、5 tests pass） — 根拠
  `rust/scripts/restore.rs:194-216` / R4-DR2 / 重大度 high / confirmed。
- [x] **`store_memory` が呼び出し側指定 id で既存メモリを無条件上書き**（2026-11-19 対応済み: HTTP 保存時にサーバ側 `Uuid::new_v4()` を生成し生成 id を JSON レスポンスで返却。回帰テスト2件追加、context 25 tests pass。**gRPC 側にも同種の問題が残存 — 下記 follow-up を参照**） — 根拠
  `rust/jarvis-context/src/http.rs:133-136` / R3-AU2 / 重大度 high / confirmed。
- [x] **承認 UI の approve/deny POST 成功経路とトークン送出が無テスト**（2026-11-19 対応済み: `approve_2xx_is_ok_and_sends_token` / `deny_2xx_is_ok_and_sends_token` を追加し 2xx → Ok と `X-Jarvis-Approval-Token` 送出を検証。`make ui-test` 16 tests pass、反復実行でも安定） — 根拠
  `ui/src/policy_client.rs:131-146` / R3-TQ4 / 重大度 high / confirmed。
- [x] **承認トークンの SECURITY NOTE が実在しない環境変数名を記載**（2026-11-19 対応済み: コメントを `JARVIS_POLICY_APPROVAL_API_TOKEN` へ修正。`config.rs:57` の `Env::prefixed("JARVIS_POLICY_")` とフィールド `approval_api_token` から実名を確認。`ci-local` exit 0） — 根拠
  `rust/jarvis-policy/src/http.rs:73-79` / R5-DC1 / 重大度 high / confirmed。
  実装は `JARVIS_POLICY_REQUIRE_APPROVAL_API_TOKEN` 等だが
  コメントは `JARVIS_POLICY_APPROVAL_TOKEN` と書かれている。
- [x] **承認キュー枯渇が HTTP 200 の通常拒否として返る**（2026-11-19 対応済み: `ApprovalError::QueueFull` を新設し HTTP `/v1/approval` では 503 を返す。通常 deny は従来どおり 200 decision。旧 `request()` は Denied 互換ラッパーとして維持。approval 8 tests / policy http 10 tests pass。**仕様書 `specs/test-policy-approval.md` の REQ-7 は旧記述のまま — 下記 follow-up を参照**） — 根拠
  `rust/jarvis-policy/src/approval.rs:109-113` / R5-VU1 / 重大度 high / confirmed。
- [x] **kill switch の非常停止ログが永続化失敗後の実状態と矛盾**（2026-11-19 対応済み: `persist()` を `Result<()>` 化し全失敗経路で fail-closed の `arm()` を維持した上で Err を返す。`activate()` は未永続化時に「NOT PERSISTED」warn を出し、成功時のみ従来ログ。新規回帰テスト追加、kill_switch 関連 12 tests pass） — 根拠
  `rust/jarvis-policy/src/kill_switch.rs:61-65` / R5-VU1 / 重大度 high / confirmed。
- [x] **現行ローカルバックアップが Git リポジトリを含まない**（2026-11-19 対応済み: `backup-local.sh` が既定で `git archive HEAD` を生成し manifest に name/size/sha256 を記録、`--skip-git` で opt-out。tracked HEAD のみで `.env` / `*.key` / `.git/config` は非包含を fail-closed 検証。隔離ディレクトリでの実バックアップ成功。**ただし timer unit が `--repo-dir` を渡さないため実機 timer 実行時は WARN スキップ — 下記「補足発見」を参照**） — 根拠
  `infra/README.md:274-279` / R5-VU3 / 重大度 high / confirmed。
- [x] **`web.fetch` の宛先ドメイン制限（egress allowlist）が未実装**（2026-11-19 対応済み: `JARVIS_MCP_GW_EGRESS_ALLOWLIST`（カンマ区切り、完全一致と `*.` サフィックス）を追加。未設定時は無制限で互換、設定時は IP リテラルを拒否し初期 URL と全 redirect hop に適用。新規3テスト追加、mcp-gw 68 tests pass） — 根拠
  `docs/integration/tool-matrix.md:162` / R1-ST4 / 重大度 high / confirmed。

  **訂正（2026-11-20 追記・I84-B）**: 上記「未設定時は無制限で互換」は
  2026-11-19 時点の履歴であり現行ではない。その後 unset/blank は
  `DenyAll`（全宛先拒否・fail-closed）へ変更済み。現行契約は
  `docs/integration/tool-matrix.md:171-175` が正
  （`rust/jarvis-mcp-gw/src/config.rs:92` も no unrestricted fallback を明記）。
- [x] **`review-run.sh` のランナー契約が OpenCode CLI 形で固定され ADR-0002 と矛盾** — 2026-11-19 対応済み: `JARVIS_REVIEW_RUNNER` と `build_runner_args()` でランナーごとの argv を吸収（dsh: `--profile <name> --json <prompt>`、opencode 他は従来どおり）。既存4契約テスト pass、スタブで dsh 分岐を確認 / `ci/scripts/review-run.sh:11-13` / R1-AR4 / 重大度 high / confirmed
  `ci/scripts/review-run.sh:11-13` / R1-AR4 / 重大度 high / confirmed。
- [x] **埋め込み失敗を空検索結果に変換して成功を返す**（2026-11-19 対応済み: `query_embedding_outcome()` を新設し endpoint 未設定は明示 skip、設定済み embed() 失敗は `anyhow::Context` 付きで Err 伝播。公開シグネチャ不変、回帰テスト5件追加。full tests / clippy / ci-local exit 0） — 根拠
  `rust/jarvis-context/src/builder.rs:115-121` / R2-ER2 / 重大度 high / confirmed。
- [x] **`expires_at` 付きメモリが失効後も検索・直近コンテキストに混入**（2026-11-19 対応済み: `is_expired` / `retain_unexpired` を追加し `expires_at <= now`（UTC・境界包含）を失効と判定。SQL と Rust の両方でフィルタ、Qdrant 関連度順は維持。UTC 境界・順序保持の回帰テスト追加、full tests pass） — 根拠
  `rust/jarvis-context/proto/context.proto:28` / R2-FG2 / 重大度 high / confirmed。
- [x] **バックアップ/リストアが Qdrant コレクション名をハードコード**（2026-11-19 対応済み: `backup-local.sh` と `restore-local.d/qdrant.sh` で `JARVIS_QDRANT_COLLECTION`（既定 `jarvis_memories`）を唯一の情報源とし、隔離コレクション名も `<collection>_restore_test` へ派生。カスタム名の dry-run で反映を確認、bash -n OK） — 根拠
  `rust/jarvis-context/src/config.rs:17` / R2-FG2 / 重大度 high / confirmed。
- [x] **`kill_switch::arm()` が `SystemTime::duration_since` を unwrap し非常停止経路が abort**（2026-11-19 対応済み: unwrap を match に置換し、時計失敗時は `activated_at=0` センチネルと CLOCK_FAILURE_REASON を記録して fail-closed を維持。正常時は既存挙動不変、kill_switch 関連12 tests pass） — 根拠
  `rust/jarvis-policy/src/kill_switch.rs:126-129` / R2-ER1 / 重大度 high / likely。
- [x] **PII パターン設定の読み込み・解析失敗が黙ってデフォルトへフォールバック**（2026-11-19 対応済み: I/O 失敗・解析失敗を構造化 `warn!`（path + error）へ変更し、既定の fail-closed デフォルトパターン適用は維持。既存 PII テスト5件 pass） — 根拠
  `rust/jarvis-context/src/pii.rs:25-27` / R1-AR2 / 重大度 high / confirmed。
- [x] **`secret_scan.py` が追跡ファイルの読み取り失敗を黙ってスキップ**（2026-11-19 対応済み: `errors` リストに記録し末尾でゲート失敗。出力はパスと例外クラス名のみで値は非開示。実機で壊れた symlink を投入し exit 1 を確認、`make secret-scan` / `test-legacy-guards` pass） — 根拠
  `ci/scripts/secret_scan.py:61-64` / R2-ER2 / 重大度 high / likely。
- [x] **OpenCode の秘密ファイル read deny が bash allow で実質無効化**（2026-11-19 対応済み: repo ルートと infra テンプレートの両 opencode.jsonc で秘密パス deny を bash/task/glob/grep/list/patch へ拡張、サブエージェントにも実効 permission を追加。残存リスク〔bash メタ文字・cwd 外絶対パスは wildcard にフォールバック〕は OS サンドボックス併用の別タスク候補として記録） — 根拠
  `infra/config/opencode/opencode.jsonc:17-19` / R1-AR3 / 重大度 high / confirmed。

### 優先度 中

- [x] gRPC Check が不正 JSON コンテキストを空 Object にフォールバック — 2026-11-19 対応済み: `Status::invalid_argument` へ変換し回帰テスト2件追加（不正 JSON 拒否・正常系受理）。`policy/src/grpc.rs:27` / R1-AR1 / confirmed（既存 P2-1 と別経路）
- [x] kill switch の永続化失敗が unauthenticated として返される — 2026-11-19 対応済み: `KillSwitchError::{InvalidAdminToken, Persistence}` を型で分離し、R4 で gRPC は `Internal` / HTTP は 500 へマップ（認証失敗のみ Unauthorized）。fail-closed は維持 / `kill_switch.rs:147-150` / R2-ER1 / confirmed
- [x] web.fetch リダイレクトで Location 欠落時に通常レスポンスへフォールスルー — 2026-11-19 対応済み: 3xx 判定を単独化し Location 欠落/非 UTF-8/空/パース不能を `ClientError` で拒否。回帰テスト2件追加、mcp-gw 84 tests pass / `mcp-gw/src/sandbox.rs:118-130` / R2-ER1 / likely
- [x] Cedar ポリシディレクトリの列挙エラーが黙って破棄 — 2026-11-19 対応済み: `metadata` probe と `read_dir` の失敗を warn で可視化（パス名と OS エラーのみ）。deny-by-default と NotFound 時の info 挙動は維持 / `policy/src/policy.rs:101-107` / R2-ER1 / likely
- [x] kill switch の `activated_by` が常に `"system"` で監査項目が機能せず — 2026-11-19 対応済み: `activate_with_actor()` / `arm_with_actor()` を追加し既存 `activate()` は "system" の互換ラッパーに。永続化失敗時の fail-closed re-arm は従来どおり。回帰テスト3件追加 / `kill_switch.rs:133` / R2-FG1 / confirmed
- [x] Casbin `add_policy` / `remove_policy` が公開 API を持たず設定ファイル手編集に依存 — 2026-11-19 対応済み: `CedarEngine` と `PolicyEngine` に永続化付き add/remove を公開（Strict 検証・パストラバーサル拒否・ロールバック）。認証は transport 側の責任としてコメント明記。回帰テスト4件追加 / `policy.rs:190,205` / R2-FG1 / confirmed
- [x] `Gateway::execute` が public なのに policy/approval を適用せず doc と不一致 — 2026-11-19 対応済み: module doc と execute doc を実態（低レベル実行 primitive、policy/allowlist/approval は `McpGateway::execute_tool` が担当）へ修正。コード・シグネチャ・テストは無変更 / `gateway.rs:11-16` / R2-FG1 / confirmed
- [x] バックアップの旧ファイル削除が失敗を無視して件数を成功報告 — 2026-11-19 対応済み: 削除結果を match し失敗は warn＋成功件数から除外。dry-run 抑制は維持。回帰テスト3件追加、backup rust-script 4 tests pass / `backup.rs:331` / R2-ER2 / confirmed
- [x] deploy がビナリ・スクリプト欠落を警告スキップして完了報告 — 2026-11-19 対応済み: 実実行前に必須4バイナリを走査し欠落時は `bail!` で非ゼロ終了（SSH 前に停止）。回帰テスト3件 pass / `deploy.rs:308-311` / R2-ER2 / confirmed
- [x] gRPC EmitEvent が不正入力をデフォルト値に置換して success を返す — 2026-11-19 対応済み: `event_from_request()` を新設し不正 id/source/payload/timestamp を `INVALID_ARGUMENT` で拒否（timestamp 0 のみ未指定扱い）。回帰テスト7件追加、event 40 tests pass / `event/src/grpc.rs:27-32` / R2-ER2 / confirmed
- [x] WS 受信エラーに生メッセージ本文を埋め込みログへ流出 — 2026-11-19 対応済み: `decode_ha_message()` ヘルパでエラーを再構成し source/bytes/エラー分類のみを記録。回帰テストで本文・トークン様文字列の非漏洩を検証（テスト文字列は secret scanner に VALUE-LIKE と誤検出されない値へ調整） / `event/src/sources.rs:248-249` / R2-ER2 / likely
- [x] `restore.rs` の `restart_services` が systemctl start のみで再起動していない — 2026-11-19 対応済み: `systemctl restart` へ変更し `run_sudo` の終了ステータス検査を維持。dry-run ガードと対象一覧は不変、restore 5 tests pass / `restore.rs:408` / R2-FG2 / confirmed
- [x] `deploy.rs --dry-run` がヘルスチェック未実行なのに healthy と報告 — 2026-11-19 対応済み: dry-run では curl を一切呼ばず `DRY RUN: health check skipped` のみを出力。実機 dry-run で当該ログを確認し `healthy` の出現0回を検証 / `deploy.rs:164` / R2-FG2 / confirmed
- [x] Cedar 認可コンテキストが生のツール引数で呼び出し元が ABAC 属性を捏造可能 — 2026-11-19 対応済み: `policy_context()` で `{tool, principal, resource, requires_approval}` のみをサーバ生成し `is_allowed` に渡す。同梱 Cedar ポリシー/スキーマは context 属性を参照しないため条件は不変。回帰テスト追加 / `mcp-gw/src/lib.rs:86-89` / R3-AU1 / likely
- [x] AllowList の `add_tool` / `remove_tool` が無認証・無永続化の公開ミューテーション API — 2026-11-19 対応済み: 呼び出し箇所がテスト以外に無いため `#[cfg(test)] pub fn` へ絞り本番バイナリから公開面を除去。同期検証テスト1件追加 / `allowlist.rs:95-106` / R3-AU1 / confirmed
- [x] Qdrant の不正 point id が nil UUID にフォールバック — 2026-11-19 対応済み: `parse_point_id()` / `entries_from_raw_points()` を新設し、欠落・数値・不正文字列は warn 付きでスキップ。nil/random UUID の代用を除去。回帰テスト3件追加、context 38 tests pass / `builder.rs:305` / R1-ST2 / likely
- [x] OTLP エンドポイント書式エラーが起動を失敗させテレメトリが致命依存 — 2026-11-19 対応済み: exporter 生成失敗時に warn を出しテレメトリなしで起動継続（JSON ログと metrics は維持）。subscriber install 後に warn 発出 / `policy/src/main.rs:22-25` / R4-DR1 / likely
- [x] `try_join!` による二重リスナ起動で片方のエラーがプロセス全体を終了 — 2026-11-19 対応済み: `tokio::join!` で両 future を待ち、`match (grpc, http)` でエラーを集約。片方のみの失敗はそのエラーを伝播し、両方なら両方の情報を返す / `policy/src/main.rs:59` / R4-DR1 / likely
- [x] ワークスペースに MSRV と lint 宣言がなく CI とローカルのゲートが不一致 — 2026-11-19 対応済み: `rust-version = "1.100"`（実ツールチェーン確認）と `[workspace.lints]`（unsafe_code forbid / clippy undocumented_unsafe_blocks deny）を追加し5 crate が継承。警告0・clippy/ci-local exit 0 / `rust/Cargo.toml:11-16` / R4-DP1 / confirmed
- [x] Qdrant クライアントの互換性チェックが起動時に無効化 — 2026-11-19 対応済み: `skip_compatibility_check()` を削除し `check_connection()` を追加して到達不能時に起動失敗（fail-closed）。回帰テスト1件追加（子の `expect_err` が Qdrant: Debug を要求しコンパイル失敗したため親が match ベースへ修正）/ `builder.rs:236` / R4-DP2 / confirmed
- [x] `deploy-mcp-gw-safe.sh` のロールバックが repo 側 allowlist 変更を戻さない — 2026-11-19 対応済み: `repo_allowlist_changed` フラグを追加し step2 で差分検出時に rollback で `cp -a` 復元。退避は既存を維持、dry-run/root チェック不変。子が指摘した /etc allowlist バックアップの上書きは別タスク候補として残る / `infra/scripts/deploy-mcp-gw-safe.sh:65` / R4-DR3 / confirmed
- [x] `jarvis-mcp-gw.service` のベースディレクトリが開発環境の絶対パスに固定 — 2026-11-19 対応済み: `EnvironmentFile=-/etc/jarvis/jarvis-mcp-gw.env` を追加し `${JARVIS_MCP_GW_BASE_DIR:-<dev既定値>}` で上書き可能に。hardening・bws run は維持、`systemd-analyze verify` exit 0 / `infra/systemd/jarvis-mcp-gw.service:52` / R4-DR3 / confirmed
- [x] MCP プロトコルバージョン定数がスモークテストとドリフト検査で不一致 — 2026-11-19 対応済み: smoke の protocolVersion を `2025-11-25` へ更新し drift 検査と同期。両方を揃える旨のコメント追加、bash -n OK / `infra/scripts/smoke-mcp-gw.sh:101` / R4-DP3 / confirmed
- [x] upstream MCP ツールのエラー本文が API 応答とログへそのまま流出 — 2026-11-19 対応済み: `safe_error_summary()` で本文を完全に除外し構造メタデータのみの分類へ。トークン/パス/URL クエリが要約に出ない回帰テスト2件追加。子のテスト用偽シークレットが secret_scan に誤検出されたため親がリネーム / `gateway.rs:200-203` / R5-VU1 / confirmed
- [x] イベント処理失敗ログに `event_id` / `correlation_id` が欠落 — 2026-11-19 対応済み: `process_event` を借用へ変更し `tracing::error!` に両フィールドを構造化追加。payload はログに渡さない。event 40 tests pass / `normalizer.rs:64` / R5-VU2 / confirmed
- [x] トークン予算超過によるコンテキスト切り捨てが無観測 — 2026-11-19 対応済み: 純粋関数 `apply_token_budget()` と `TruncationReport` を追加し超過時に warn を1回発行（before/after バイトとセクション名のみ、内容や PII は非出力）。回帰テスト2件追加、context tests pass / `builder.rs:197-203` / R5-VU2 / confirmed
- [x] HTTP ハンドラが内部エラーの理由を 500 に変換して捨てる — 2026-11-19 対応済み: `log_internal_error()` で anyhow の context チェーンを `tracing::error!` に記録し、クライアント応答は裸の 500 を維持。`#[instrument]` から req を除外し生データ/PII をログに出さない。新規テストでチェーン記録を検証 / `context/src/http.rs:120-123` / R5-VU2 / confirmed
- [x] Qdrant スナップショット取得に HTTP タイムアウトがなくバックアップがハングし得る — 2026-11-19 対応済み: reqwest blocking Client に 30 秒タイムアウトを設定（`JARVIS_QDRANT_TIMEOUT_SECS` で上書き可）。全リクエストに適用 / `backup.rs:181` / R5-VU2 / likely

### メモ（要判断・却下）

- **要判断**: gRPC データプレーン RPC（`ExecuteTool`）が `admin_token` 未設定時に
  無認証な点（`grpc.rs:49-53` / R1-ST1）。2026-11-19 の実装で
  loopback 互換のため意図的に残した経路。`RegisterMcpServer` は
  fail-closed 済み。データプレーンも認証必須にするかは運用判断。
- **要判断**: Casbin allow が Cedar を短絡する
  （`policy/src/lib.rs:113-125` / R1-AR1）。前回と同じく意図的な
  二段評価だが、Cedar の forbid が効かない点は未解決。
- **要判断**: OpenCode の `bash` allow が secrets deny を実質無効化する
  （`infra/config/opencode/opencode.jsonc:17-19` / R1-AR3）。
  開発用許容か境界侵害かを判断する必要がある。
- **却下（重複）**: 既存 P0-3（principal 自己申告）、P0-10（kill switch 既定
  トークン）、P1-2（PG/Qdrant 二段書き込み）、P1-7（jarvis-core 分裂）、
  P1-12（Webhook 認証）、P2-1（PII テスト）、P0-7（Grafana 認証）、
  axum 二重依存、otelcol debug exporter、`op inject` 依存、
  `restore.rs --local-only`、Qdrant スナップショット先頭取得、
  `setup-selfhost.sh || true`、初回 review の全 high 指摘。
- **制約**: 観点を 2/ラウンドに絞ったため、前回のような全観点網羅ではない。
  既知情報として前回の全 high を配布し重複は回避できた。

## 補足発見（2026-11-19 イテレーション3 / R4 レビュー）

- [x] **restore.rs のバックアップ命名体系が現行 `backup-local.sh` の出力と不一致（実バックアップから復元不能）**（2026-11-19 対応済み: 新規 `restore-local` と4モジュールを追加。現行セットの manifest/COMPLETE を直接入力とし、検証ゲート・dry-run 既定・`--apply --confirm`・コンポーネント別スキップ・Qdrant 非停止を実装。`bash -n` / shellcheck 0、dry-run 書き込みゼロ、モック検証4件 pass、`ci-local` exit 0。**隔離統合テスト S9 と実機復元演習は残る**） — 根拠
  `rust/scripts/restore.rs:133-135`, `infra/scripts/backup-local.sh:35` /
  R4 レビュー / 重大度 high / confirmed。
  - `restore.rs` は rclone リモート上のフラットな日付命名
    （`pg-<date>.sql.gz` / `qdrant-<date>.snapshot` / `configs-<date>.tar.gz`）
    を探す。
  - 一方、2026-11-19 に実機導入した `backup-local.sh` は
    `<host>-<stamp>/` ディレクトリ内に
    `postgres-jarvis.sql.gz` / `qdrant-snapshot.bin` / `config-*.tar`
    を生成する（ADR-0004 のローカル専用運用）。
  - 今回追加した `validate_backup_set()` は `restore.rs` 内部の整合性を
    改善したが、**実バックアップセットは依然として見つからない**。
  - 復元演習の前に、`restore.rs` をローカルバックアップ体系へ接続する
    （またはレガシー rclone 経路を明示的に廃止する）必要がある。

- [ ] **timer unit が `--repo-dir` を渡さず Git バックアップがスキップされる**（2026-11-19 repo 側対応済み: `jarvis-backup.service` の `ExecStart` に `--repo-dir /home/koppe/ドキュメント/作業用/jarvis/jarvis` を追加し、`ProtectHome=yes` → `read-only` へ変更して git repo を読めるようにした（`jarvis-mcp-gw.service` と同じ境界）。`systemd-analyze verify` exit 0。**実機への unit 再適用は root 必須・ユーザー実行が残る**） — 根拠
  `infra/systemd/jarvis-backup.service:11`, `infra/scripts/backup-local.sh:112-127` /
  R4 レビュー（イテレーション4） / 重大度 high / confirmed。
  - `backup-local.sh` は既定で `git archive HEAD` を生成するが、
    実機 unit は `/opt/jarvis/scripts/backup-local.sh` を実行するため
    近傍に git repo が無く、auto-detect が失敗して WARN を出し
    Git のみスキップする。
  - 実機 timer で Git を必ず含めるには `ExecStart` に
    `--repo-dir /home/koppe/ドキュメント/作業用/jarvis/jarvis` を追加し、
    root で unit を再適用する必要がある（ユーザー実行が必要）。

## 残タスク（restore-local S8 / S9）

- [x] **S8: ドキュメントとレガシー整理**（2026-11-19 対応済み: `index.md` 追記、runbook に相互リンクと3段の安全レベル例示、`restore.rs` に非推奨ヘッダ（**rust-script の cargo マニフェストブロックより後に配置** — 先頭に置くと依存が認識されず `--help` が失敗する）、`infra/README.md` に現行状態を追記。`rust-script --help` / rustfmt / ci-local exit 0、新規 markdownlint 違反なし（既存 baseline の MD013/MD028 は据え置き）） — 根拠
  `docs/integration/local-restore-design.md:250-252` / 優先度 中。
  `docs/integration/index.md` への追記、`backup-restore-runbook.md`
  からの相互リンク、`restore.rs` への非推奨ヘッダ、
  `infra/README.md` のレガシー記述更新。
- [ ] **S9: 隔離先での全コンポーネント統合テストと実機復元演習** — **2026-11-19 一部対応: `infra/scripts/test-restore-local.sh` を新規作成し合成フィクスチャで dry-run/apply 経路を自動検証（47 tests pass、本番リソース非接触を多重ガード）。実機の隔離先復元演習はユーザー承認待ちで残る** — 根拠
  `docs/integration/local-restore-design.md:253-255` / 優先度 高。
  使い捨て DB / Qdrant コレクション / ディレクトリで全コンポーネントを
  復元し Runtime Proof を記録する。本番適用はユーザー承認後に実施。

## follow-up（2026-11-19 Round 2 / R4 レビュー）

- [x] **仕様書 REQ-7 をキュー枯渇の 503 挙動へ同期**（2026-11-19 対応済み: 仕様 REQ-7 と TC-Q1 を更新、証跡を実測 53 passed / 0 failed で再生成（`evidence/test-run-20261119-cc55d64f971a.json`）、両 index 更新、`make check-evidence` ok=6 errors=0） — 根拠
  `specs/test-policy-approval.md:44,177` / R4 レビュー / 優先度 中。
  - 現状は「max_pending 超過は approval queue full で即 Denied」とあり、
    HTTP 層が 503 を返す実装と乖離している。
  - 「ストア層 `request()` は Denied 互換を維持、`try_request()` は
    `ApprovalError::QueueFull`、HTTP `/v1/approval` は 503」と明記する。
  - 仕様変更後は `check-evidence.sh` のハッシュ一致エビデンスを再生成し、
    `make check-evidence` で検証する。

## follow-up（2026-11-19 Round 3 / R4 レビュー）

- [x] **gRPC の `store_memory` も呼び出し側指定 id で上書き可能**（2026-11-19 対応済み: proto に `StoreMemoryResponse { id }` を追加し戻り値を変更。grpc.rs で `Uuid::new_v4()` を生成して指定 id を破棄し、生成 id をレスポンスで返す。回帰テスト2件追加、context grpc 2 tests pass。proto3 のフィールド追加は後方互換） — 根拠
  `rust/jarvis-context/src/grpc.rs:47-70`, `proto/context.proto:7,22` /
  R4 レビュー / 優先度 高。
  - HTTP 側はサーバ生成 UUID へ修正済みだが、gRPC は
    `Uuid::parse_str(&req.id)` をそのまま `MemoryEntry.id` に使用。
  - `returns (Empty)` のため生成 id を返すには proto 変更が必要
    （後方互換を考慮したバージョン設計）。当面は既存 id 指定時に
    拒否する方式を検討。
  - 多層防御として `builder.rs:134` の `ON CONFLICT (id) DO UPDATE`
    の見直しも検討対象。

## 完了記録（2026-11-19 Round 5 / R4 対応）

- [x] **HTTP エラー分類を型ベースへ変更** — `error.rs` に `ErrorKind` と
  `McpGatewayError::ClientError` を追加し、`http.rs` の `http_status_for` を
  downcast ベースへ。19個のメッセージ接頭辞判定（`is_client_error`）を廃止。
  R4 で発見した 400→500 契約後退は `sandbox.rs` / `gateway.rs` の
  入力検証エラーを `ClientError` へ型化して復元（内部エラーは 500 を維持）。
  mcp-gw 59 tests pass。
- [x] **MCP inputSchema を実装へ整合** — `filesystem.read/list` の `path` を
  required から外し既定値 `"."` を description に明記。無視される
  `max_bytes` を `filesystem.read` から削除し、`web.fetch` は実装の
  clamp 範囲 [1024, 1048576] へ合わせた。子のテストが `"required"` キー
  欠落でパニックしたため親が `get()` ベースへ修正、対象テスト pass。

## 完了記録（2026-11-19 Round 7 / R4 対応）

- [x] **証跡の skipped 数を検証** — `check-evidence.sh` に `SKIPPED_TESTS` を追加。
  ただし実証跡1件が Docker 依存 `#[ignore]` の `skipped=1` だったため、
  親が「notes に `ignored` の明記がある場合のみ許可」へ調整（数値の書き換えは
  証跡改ざんになるため却下）。`make check-evidence` ok=6 errors=0、
  legacy guards 22 tests pass。
- [x] **bootstrap の `config/mobile.env` も保護** — `write_private_if_absent`
  へ統一し既存ファイルは警告のみでスキップ。無条件上書きの `write_private`
  定義を削除。`rustfmt --check` / `--help` pass。

## 完了記録（2026-11-19 Round 9）

- [x] **Qdrant スナップショットの取得元を生成レスポンスへ統一** —
  `backup-local.sh` が POST レスポンスを破棄して一覧の `.result[0].name`
  を無条件に使っていた問題を修正。POST 応答の `.result.name`
  （旧形式の文字列も可）のみを根拠とし、名前が無ければ明示エラーで中止。
  dry-run の書き込みゼロを確認、bash -n OK。

## 完了記録（2026-11-19 Round 10）

- [x] **event / context の設定パスも絶対パス化** — 457 の policy/mcp-gw に
  続き `/etc/jarvis/{event,context}.toml` 既定と
  `JARVIS_{EVENT,CONTEXT}_CONFIG_PATH` 上書きに対応。Env provider から
  `config_path` を除外し、実機は Env 供给でファイル不在でも起動可。
  新規パス解決テスト6件追加。

## follow-up（2026-11-19 Round 14 / R4 レビュー）

- [x] **`deploy-mcp-gw-safe.sh` の /etc allowlist バックアップが段階間で上書きされる** — 2026-11-19 対応済み: バックアップ名を `.bak-step2-$ts` / `.bak-step3-$ts` へ分離し、rollback は step3→step2 の逆順で復元。step2 のみ変更時の専用フラグも追加、bash -n OK
  — 根拠 `infra/scripts/deploy-mcp-gw-safe.sh:68,114` / R4 レビュー / 優先度 低。
  step2 と step3 の両方が同じ `allowlist.bak-$ts` 名でコピーするため、
  両方で変更があった場合に rollback がパッチ済み内容を復元してしまう。
  バックアップ名を段階別にする等の修正は今回のスコープ外（repo 側復元のみ対応）。

## follow-up（2026-11-19 Round 17 / R4 レビュー）

- [ ] **OpenCode の設定だけでは塞がらない秘密読取経路** — 根拠
  `opencode-agent/src/executor.rs:578-651`（ローカル依存ソース）/
  R4 レビュー / 優先度 中。
  - bash コマンドに `& ; | > < $ \` ( ) { } \` や改行が含まれると
    permission target が `"*"` へフォールバックし、`cat $HOME/.aws/credentials`
    等のパス deny を迂回し得る。
  - read/edit のパスが cwd 外絶対パスの場合も正規化失敗で wildcard に落ちる。
  - 設定の wildcard allow を維持する以上 JSONC だけでは完全遮断不可。
    OS サンドボックス（ProtectHome 等）または実行前フックでの補完を検討。

## バックログ補充（2026-11-19 Round 19 / review + discovery）

8体（敵対的レビュー4・発掘4）の並列実行から得た指摘を、重複排除と
Runtime Proof付きで集約した。根拠は各項目の `path:line` を参照。

### 優先度 高

- [x] **jarvis-context の gRPC が `admin_token` を完全にバイパス** — 2026-11-19 対応済み: gRPC 3 RPC の `into_inner()` 前に `x-jarvis-context-token` metadata を定数時間検証。HTTP と同じ `admin_token` を共有、未設定時のみ loopback 互換。ContextConfig の Debug も redacted 化。回帰テスト7件追加 / `rust/jarvis-context/src/grpc.rs:112-119` / B3 / 高 / confirmed。
- [x] **Webhook・memory API・upstream エラーが tracing へ生本文を流出** — 2026-11-19 対応済み: 4 Webhook と context store/search を `skip(payload/req)` 化、upstream エラーを `upstream_error_summary()`、HA ハンドシェイクを metadata-only へ。回帰テスト4件追加。子のテスト定数名 `SECRET` が secret_scan に誤検出されたため親が `PROBE` へリネーム / B3 / 高 / confirmed。
- [x] **otelcol が journald を読めず JARVIS ログが空** — `infra/systemd/otelcol.service:9-10`, `otelcol-config.yaml:51-52` / B4 / 高 / confirmed。親が `id koppe` で systemd-journal 非所属を確認し、unit に `SupplementaryGroups=systemd-journal` を追加（systemd-analyze verify exit 0）。**実機への unit 再適用は root 必須・ユーザー実行が残る**。
- [x] **deploy ロールバックが退避ファイル欠落時に新規 unit/allowlist/バイナリを残す** — 2026-11-19 対応済み: 各復元 if に else 分支（install 先の明示削除）を追加、退避コピー失敗時は `die` で新規配置前に中止。ロールバック後に drift 検査を情報表示で実行。残る判断点: 退避失敗時の部分適用と repo allowlist は削除せず警告（意図的） / B4 / 高 / confirmed。
- [x] **HTTP 型ベースエラー分類の取りこぼし（入力エラーが 500 に後退）** — 2026-11-19 対応済み: sandbox.rs の bare `bail!` を全除去し入力由来エラーを `ClientError` へ型化（内部エラーは 500 維持）。HTTP 400/500 境界の回帰テスト追加 / B1 / 高 / confirmed。
- [x] **egress allowlist が TOML 設定を黙って無視し空値で fail-open** — 2026-11-19 対応済み: `McpGatewayConfig.egress_allowlist` を正式フィールド化し TOML/env 両対応、空値・不正値は DenyAll（fail-closed）。unit に `JARVIS_MCP_GW_EGRESS_ALLOWLIST`（既定 example.com、ホスト側で上書き）を追加。起動ログに実効ホストを出力 / B1 / 高 / confirmed。
- [x] **P1-1 ベクトル検索スコアの保持**（重複: 本体は行86の P1-1。こちらはバックログ補充時の重複エントリ） — `context/src/builder.rs:15,228` / B5 / 高。
- [x] **P1-2 PG/Qdrant 二段書き込みの補償**（重複: 行89の P1-2 で 2026-11-19 対応済み） — `context/src/builder.rs:198` / B5 / 高。
- [x] **P1-3 マイグレーションのバージョン管理移行**（重複: 行92の P1-3 で 2026-11-19 対応済み） — `context/src/builder.rs:396` / B5 / 高。
- [ ] **Grafana Cloud アラート／失敗通知が未整備** — `docs/integration/remaining-tasks.md:667` / B7 / 高 / needs_user。kill switch 発動・承認タイムアウト・バックアップ失敗の通知先を確定。
- [ ] **bws.env の復元・再配置経路が未定義** — `docs/integration/backup-restore-runbook.md:70-72,186` / B7 / 高 / needs_user。ディスク故障後にサービスが起動できない。
- [ ] **pprof/zpages が常時有効でネットワーク制限も未判断** — `otelcol-config.yaml:144`, `otelcol.service:27-32` / B7 / 高 / needs_user。

### 優先度 中

- [x] gRPC ハンドラが `ErrorKind` を無視し全エラーを `Status::internal` に変換 — 2026-11-19 対応済み: `grpc_status_for()` で ClientError→InvalidArgument / Forbidden→PermissionDenied / Internal→Internal へマップ。Internal は固定メッセージ＋詳細は tracing ログへ隔離。HTTP/gRPC 合同契約テスト追加 / `mcp-gw/src/grpc.rs:64-85` / B1 / 中。
- [x] tools/list の inputSchema 優先順位が実際のルーティングと逆転 — 2026-11-19 対応済み: `resolve_target()` ヘルパでルーティングとスキーマ選択を単一化し upstream 優先へ統一。旧テストの期待値を更新（理由コメント付き）/ `mcp_server.rs:51-59`, `gateway.rs:223-234` / B1 / 中。
- [x] 承認 gauge / 一覧が期限切れ放棄エントリを prune せず過大報告 — 2026-11-19 対応済み: `list_pending()` の読取側で `now > deadline` をフィルタし副作用なしで実態を反映。回帰テスト追加 / `policy/approval.rs:155,249-256` / B2 / 中。
- [x] kill switch status API が無認証で自由文字列 reason / actor を開示 — 2026-11-19 対応済み: HTTP は `x-jarvis-kill-switch-token` ヘッダ / gRPC は同名 metadata で dedicated admin token を検証。`KillSwitchStatus::redacted()` で reason/actor を固定語彙へ丸め、内部監査用 `status()` は生文字列維持 / B2 / 中。
- [x] kill switch admin token 比較が非定数時間 — 2026-11-19 対応済み: `constant_time_eq` を kill_switch モジュールの `pub(crate)` 共通ヘルパへ集約し、HTTP 承認トークンゲートと kill switch の全比較が同じ実装を共有 / `kill_switch.rs:135,217` / B2 / 中。
- [x] 失効フィルタが Qdrant top-K 選択後に適用され recall が低下 — 2026-11-19 対応済み: Qdrant payload に expires_at が無いため `EXPIRY_OVERFETCH_FACTOR=2` で候補を広げ、SQL 权威ゲート＋Rust retain の後に `take(limit)` で trim。純粋回帰3件追加 / `context/builder.rs:228-259` / B3 / 中。
- [x] PII キー redact がキー衝突で別フィールドを上書き — 2026-11-19 対応済み: `unique_key()` で `[REDACTED]#1` 等のサフィックスを付与し通常キーとの衝突も回避。回帰テスト4件追加 / `context/pii.rs:124-164` / B3 / 中。
- [x] check-deploy-drift がサービス停止時の E2E SKIP を OK と報告 — 2026-11-19 対応済み: サービス停止時を `DANGER: サービス停止` + `danger=1`（exit 2）へ変更。RESULT 文言も SKIP を OK と扱わない旨を明記。deploy の厳密 gate（:191）が exit 2 を受け取るため誤合格経路も閉じた / `check-deploy-drift.sh:169-182` / B4 / 中。
- [x] 日次バックアップが verify-backup を通さない（導入時のみ検証） — 2026-11-19 対応済み: `jarvis-backup.service` に `ExecStartPost=` で `verify-backup.sh --latest` を追加（接頭辞 '-' なしで失敗時 unit failed）。`install-backup-timer.sh` は verify 作業コピー欠落時に jq と同じ fail-closed。`systemd-analyze verify` / bash -n OK / `jarvis-backup.service:12` / B4 / 中。
- [x] check-shell が予測可能 `/tmp` パスを使用 — 2026-11-19 対応済み: `mktemp -d` + `trap 'rm -rf "$tmpdir"' EXIT` で出力先を隔離。旧 `/tmp/check-shell.err` 等への参照0件、shfmt 警告のみ・shellcheck 必須の挙動は維持 / `check-shell.sh:25,27,39,43` / B4 / 中。
- [x] jarvis-backup.service に `Wants=postgresql.service qdrant.service` が欠落 — 2026-11-19 対応済み: repo unit へ追加し systemd-analyze verify exit 0。**実機への unit 再適用は root 必須・ユーザー実行が残る** / `jarvis-backup.service:4` / B7 / 中 / confirmed。
- [x] S9 のうち隔離先統合テストのみ自動化可能（重複: 行683 の S9 で 2026-11-19 一部対応済み、`test-restore-local.sh` 47 tests pass） — `docs/integration/local-restore-design.md:253-255` / B5 / 中。
- [x] AGENTS.md の adr/ 一覧が ADR-0002/0003/0005 を欠落 — 2026-11-19 対応済み: Project Structure ツリーと References に実在する5件すべてを追記 / `AGENTS.md:37-39` / B8 / 中。
- [x] ADR-0005 の「ui-build/ui-run 追加」が実装済みなのに未完了 — 2026-11-19 対応済み: 該当チェックを [x] へ更新（本番自動起動は未実装で [ ] 維持）/ `adr/0005:104` / B8 / 中。
- [x] ADR-0003 の web.fetch `requires_approval=false` 記述が古い — 2026-11-19 対応済み: 現行 `true`・人間承認必須・allowlist 同期確認へ更新し、旧記述は「（履歴）」と明記。ADR の決定内容（ネイティブ実装・SSRF 対策）は不変 / `adr/0003:35-40` / B8 / 中。
- [x] infra/README のバックアップ節が自己矛盾（未導入記述と導入済み記述が併存） — 2026-11-19 対応済み: 現行状態（timer 導入済み・OCI 不使用・ローカル完結）を前面へ出し、旧 OCI 記述は「履歴（2026-11-15 時点）」へ集約。関連ドキュメントのリンクは維持 / `infra/README.md:268-293` / B8 / 中。
- [x] mcp_allowlist.toml の「承認必須ツールは tools/list から非表示」コメントが旧挙動 — 2026-11-19 対応済み: 3箇所のコメントを「tools/list に表示され、呼び出しは人間承認を待つ」へ更新。`requires_approval` 値と実コードは不変 / `infra/config/mcp_allowlist.toml:81,98,115` / B8 / 中。

### メモ（要判断・却下）

- **却下（既知判断）**: ADR-0002 の `store_memory` 生保存は意図的な設計判断（B5 の P0-11 提起は既存 ADR の記録どおり）。再評価はユーザー判断待ち。
- **stale**: P0-3 は `/mcp` の server-side principal guard で対応済み（B5 / confirmed）。opt-in ガードの本番有効化は残る。
- **speculative**: NAT64 prefix (64:ff9b::/96) と予約 IPv4 レンジの SSRF すり抜け — `sandbox.rs:484-508` / B1 / speculative。通常環境では NAT64 無く未検証。
- **実機確認済み**: P0-6 は実機 4317/4318/8888 すべて `127.0.0.1`（B5 / confirmed）。repo テンプレートの全6リスナー版を実機へ適用するかは方針判断。

## 完了記録（2026-11-19 Round 21）

- [x] **otelcol traces/logs が debug exporter のみで Grafana Cloud へ送信されない**
  — `infra/config/otelcol-config.yaml` に `otlp/grafana` exporter を追加
  （endpoint / auth は `${env:...}` 参照、値はファイルに記載しない）。
  traces / logs パイプラインは `[debug, otlp/grafana]` の両方へ出力。
  `GRAFANA_CLOUD_OTLP_ENDPOINT` と `GRAFANA_CLOUD_AUTH` の供給経路
  （P0-7）は未確定のまま、実機テンプレート同期と bws 側の値設定は
  ユーザー判断として残る。ダミー env で `otelcol-contrib validate` exit 0。

## 完了記録（2026-11-19 Round 25）

- [x] **実機承認待ちタスクの手順を一元化**
  — `docs/integration/remaining-tasks.md` に「確認 → 修正 → 再確認」の
  6ブロック（timer 再適用 / S9 実復元演習 / P1-4 外部化 / P0-7 Grafana /
  P1-12 Webhook / 共通前提）を追加。`backup-restore-runbook.md` の
  「隔離復元未実装」記述を現行状態（restore-local + 47 tests pass）へ更新。
  値・秘密情報・推測日付は記載せず、sudo が必要なコマンドを明示。
  実機操作・外部送信は行っていない。
- [x] **追跡 TOML の構文パース gate 追加** — 2026-11-19 対応済み:
  `ci/scripts/check-toml.sh` を新規作成（tomllib → tomli → fail-closed、
  `git ls-files -z '*.toml'` を NUL 区切りでパース、失敗時はファイル名のみ出力）。
  Makefile の `ci-local` と `make check-toml`、ci.yml の `extra_gates` に配線。
  追跡 TOML 9件すべてパース成功、bash -n / shellcheck / ci-local OK。

## 完了記録 / 新規指摘（2026-11-19 Round 26）

### R4 で修正した指摘

- [x] 承認トークンの空文字設定が fail-open — `config.rs::validate_approval_tokens()`
  で起動時拒否、`http.rs::require_approval_token` で空ヘッダを常に 401。
  回帰8件追加。
- [x] mcp-gw の gRPC / HTTP admin token 比較が長さ不一致で短絡 —
  両方を長さ包含型の定数時間比較へ統一（kill_switch と同等）。
- [x] MCP `tools/call` が anyhow チェーン全体をクライアントへエコー —
  `McpGatewayError::kind()` で分類し Internal は固定メッセージのみ、
  詳細は tracing ログへ隔離。upstream 本文は `safe_error_summary` 経由に統一。

### 新規指摘（未対応）

- [x] Qdrant outbox reconcile の head-of-line blocking — 2026-11-19 対応済み: 004 migration で `attempts` / `next_retry_at` を additive 追加、失敗時に指数 backoff（上限1440分）で後ろへ退避、成功時のみ削除。先頭100行が恒久失敗しても後続行が再試行される純粋回帰テスト追加 / `builder.rs` の
  `RECONCILE_BATCH_LIMIT=100` が `created_at ASC` のため、先頭100行が恒久失敗すると
  101行目以降が永久に再試行されない。attempts / next_retry_at 列または
  created_at カーソルによるラウンドロビン＋dead-letter が必要（likely）。
- [x] UI の core 境界変換がデッドコード化 — 2026-11-19 対応済み: `approve/deny` のレスポンス解析へ `decision_from_json` を結線、`decide()` の POST 前に `request_to_core` で UUID を確定（不正/stale id は fail-closed）。doc を実装事実へ訂正、`#[allow(dead_code)]` 削除。UI 27 tests pass / `decision_from_json` /
  `request_to_core` が `#[allow(dead_code)]` で実 RUN 経路から呼ばれず、
  doc の「legacy fallback を受ける」記述も実装（明示拒否）と矛盾（confirmed）。

### 設計決定（実装は未着手）

- **jarvis-event のサイドカー境界**: 案A（完全 sidecar 化）を推奨。
  スライス1で `ContextProvider` / `PolicyGate` トレイト seams、スライス2で
  context gRPC クライアント（BuildContextRequest に source/timestamp を加算追加）、
  スライス3で既定 grpc 化と in-process 依存削除、スライス4で policy も型依存削除。
  ADR-0001/0002 は経過措置を明記後、スライス3完了時に例外条項を削除。
- **バックアップ外部化**: 案C（USB短期＋HDD中長期の2世代）を段階導入推奨。
  まず USB ミラー、暗号化は外部コピーのみ age、署名は既存 OpenSSH
  `ssh-keygen -Y sign`。RPO/RTO・媒体の接続形態・鍵保管先はユーザー確認待ち。

### R4 で修正した指摘（2026-11-19 Round 27）

- [x] mcp-gw の `admin_token = ""` が gRPC / HTTP 両面で認証バイパス（critical）
  — `validate_admin_token()` で起動時拒否、両ゲートで trim 後空の提示トークンを
  常時拒否。空同士成功の既存テストは期待値を更新。
- [x] `/v1/execute` が `error.to_string()` を 500 ボディへエコー（high）
  — `ErrorKind` で分岐し Internal は固定文言＋tracing ログのみ、
  ClientError は固定語彙、Forbidden は固定ポリシー文言へ。
- [x] 「定数時間」コメントと実装の乖離（low）— 長さ包含型で短絡は無いが
  反復回数は入力長依存、と実装事実へ訂正。真の定数時間化は別タスク候補。
- 親が子の出力で4点を修正: `tokio_postgres::Client::transaction()` の
  `&mut` 要求（Mutex 化＋残る参照3箇所）、clippy の `unused_mut` /
  `dead_code`（`is_retry_due` を `#[cfg(test)]` 化、テスト用フィールドに
  `#[allow(dead_code)]`）、空白トークンの `contains("")` 誤検証、
  secret_scan 誤検出（テスト変数名を `probe_path` へ）。

### Round 28 のレビュー／発掘結果

- [x] UI が存在しない `JARVIS_POLICY_APPROVAL_TOKEN` を読み認証有効時に操作不能
  （confirmed high）— UI を `JARVIS_POLICY_APPROVAL_API_TOKEN` /
  `JARVIS_POLICY_APPROVAL_ADMIN_TOKEN` の scope 分割へ修正。旧名は警告付き
  フォールバック。Debug redaction と env→ヘッダ契約テスト9件追加、UI 36 tests pass。
- [x] gRPC が ClientError / Forbidden の生テキストを返し HTTP サニタイズを迂回
  （confirmed high）— `safe_client_error_text` を `pub(crate)` 化して共有、
  Forbidden は固定ポリシー文言へ。漏洩を期待していたテストを更新。
- [x] Pg `record_request` 失敗の握り潰し（confirmed high）— メモリ登録取消し＋
  `QueueFull`(503) で fail-closed、`mark_expired` は warn 可視化。DB 失敗注入2件追加。
- [x] **migration が jarvis-context に独占され policy が先起動すると 003 未適用** — 2026-11-19 対応済み: `jarvis-migrations.service`（Type=oneshot / RemainAfterExit、bws 経由で URL 注入、秘密情報なし）を新設し policy / context 双方が `After=` + `Wants=` で依存。既存 policy→context 順序は維持、`systemd-analyze verify` 3 unit で循環なし。install-backup-timer.sh の配置枠組みにも組み込み。
  （confirmed high）— `jarvis-policy.service` に `After=jarvis-context.service`
  を追加すると**循環依存**になる（context が policy に After を持つため、
  `systemd-analyze verify` で検出）ので repo 側ではコメントで注意喚起に留めた。
  真の解決は migration 用 oneshot unit の分離、または policy 起動時 migration
  実行／必須テーブル CHECK。初回導入時は context を先に起動するか
  `cargo sqlx migrate run` を手動実行する必要がある。
- [x] **PgApprovalPersistence が単一接続で再接続しない**（likely medium）— 2026-11-19 対応済み: 接続世代（client + driver）を Mutex で保持し、失敗時に旧 driver を abort して再接続。初期1回＋再接続2回の上限付きリトライ、上限到達時は `ApprovalError::Persistence`。再試行判定の pure テスト追加 —
  PostgreSQL 再起動後に永続化が恒久停止する。接続死活監視＋再生成、
  または明示的な Persistence エラー通知が必要。
- [x] **ドキュメントドリフト5件**（i31-todo-discovery / 中〜低）— 2026-11-19 対応済み: 旧トークン名を「UI 側エイリアス」と明記、specs/test-policy-approval.md に REQ-12/13/14＋R-9＋TC-A3/A4/A5 追加、ui/README と ADR-0005 の固定テスト件数を削除、smoke コメントを現行挙動へ、require_server_side_principal を「実装済み opt-in」へ更新。spec 変更に伴い evidence JSON を実測（144 tests）再生成し index も同期 —
  policy.env / ui/README / remaining-tasks.md の旧トークン名、
  specs/test-policy-approval.md の2トークン分離未反映、
  ui/README と ADR-0005 のテスト件数（実測27→36）、
  smoke-mcp-gw.sh:4 の旧「承認必須ツール非表示」コメント、
  remaining-tasks.md の `require_server_side_principal`「未実装」記述。
- 親が子の出力で3点を修正: UI テストの lifetime 明示、`or_else`→`or`、
  systemd 循環依存の回避（コメント化）。

### Round 29 のレビュー結果（未対応）

- [x] **起動時リコンサイルした保留承認に実行中のタイムアウト決着経路が無い** — 2026-11-19 対応済み: `prune()` を async 化し、期限切れ recovered 行を `mark_expired` のコミット成功後にのみメモリから除去。コミット失敗時はエントリを残し次回スウィープ／起動 reconcile が再試行。回帰テスト追加。
  （confirmed high）— 復活した live 行の期限が切れても DB は undecided のまま、
  次の `try_request` の prune でメモリから消える。operator は decide できず
  監査も次回再起動まで欠落。`prune` と `mark_expired` を単一決着経路へ集約し、
  コミット成功後にのみメモリから除去する必要がある。
- [x] **`decide` とタイムアウトの順序反転で永続化・メモリ・応答が不一致** — 2026-11-19 対応済み: コミット結果を権威としてメモリを上書きする方式へ統一。`deciding` フラグで in-flight コミットを検知しタイムアウト側は通知を待つ（上限100回×100ms）。期限切れ後の decide は AlreadyDecided(409)＋warn で明示拒否。競合テスト追加。
  （likely high）— DB コミット後〜再ロック前にタイムアウトがメモリを Expired に
  すると、DB は approved なのに待機側は Expired。期限切れ後の decide を明示拒否し、
  決着を単一 CAS 結果から導出する。
- [x] **reconcile が `approval_max_pending` を無視**（confirmed medium）— 2026-11-19 対応済み: live 行は空きスロット数だけ再登録、超過分は `mark_expired` で audit 決着。復活／期限切れ／超過件数を INFO ログで可視化、回帰テスト追加 —
  DB に上限超過の undecided 行があると再起動時に全行がメモリへ載り、
  新規承認が期限切れまで QueueFull で連続拒否される。
- [x] **認証ヘッダ／失敗ログのテストが定数・ソース文字列の自己確認に形骸化** — 2026-11-19 対応済み: `jarvis-core` に `JARVIS_CONTEXT_TOKEN_HEADER` を追加し context gRPC / HTTP / event provider の3箇所が共有定数を参照。自己確認テストを横断一致検証へ更新。
  （confirmed medium）— クライアント／サーバ／HTTP でヘッダ定数が別定義なのに
  各テストは自側の定数とリテラルを比べるだけ。共有定数化または横断契約テスト、
  および tracing subscriber での構造化ログ検証が必要。
- [x] **mcp-gw `/v1/execute` が `#[instrument(skip(gateway))]` でreq 全体をログへ** — 2026-11-19 対応済み: `skip(gateway, req)` へ変更。PROBE センチネルの非漏洩回帰テスト追加。スパン捕捉アサーションは並列実行で flaky なため削除し、非漏洩アサーションのみ維持（理由をコメント化）。
  （confirmed high）— ツール引数（書き込み内容・URL 等）がスパン属性として
  OTLP/Grafana へ送出され得る。`skip(gateway, req)` 化と、tool 名・principal
  のみの固定フィールド記録が必要。

- 親が対応: spec 変更に伴う evidence を実測再生成（144 tests / hash
  `90ef3ba0db8f…` / ファイル名・index 同期）。flaky な
  `webhook_spans_do_not_record_raw_payloads` は再実行で pass（無理な反復は停止）。

### Round 30 のレビュー／発掘結果

- [x] **migrations unit が通常デプロイ経路に配線されず cargo-sqlx も未配布** — 2026-11-19 対応済み: `build-selfhost.sh` の `JARVIS_SERVICE_UNITS` と配置ループの両方へ追加。sqlx-cli 導入を `cargo install sqlx-cli --no-default-features --features postgres --locked --root /usr/local` に統一し、両ドキュメントへ配置／有効化／検証手順を追記。`bash -n` / shellcheck / systemd verify OK。
  （confirmed high）— `build-selfhost.sh:60-75` の `JARVIS_SERVICE_UNITS` に
  `jarvis-migrations.service` が無く、`command -v cargo-sqlx` も開発環境に無い。
  policy/context は Wants で待つが unit 未配置なら無視され、003 未適用で
  reconcile が失敗する。unit を配置リストへ追加し sqlx-cli 導入を手順化する
  か、context の `migrate!()` を唯一の主体に統一して unit を廃止する。
- [x] **install-backup-timer.sh の rollback が migrations unit / verify-backup.sh** — 2026-11-19 対応済み: 退避と復元を単一の `restore_pairs` 配列（5対象）から生成し、migrations unit と verify スクリプトも復元対象化。バックアップ欠落時は新規配置を削除。最初の書き込み前に退避を移動。dry-run / root / post-install 検証は不変、`bash -n` / shellcheck OK。
  を復元しない**（confirmed medium）— 退避と復元を単一のペア配列から生成し、
  欠落時は新規配置を削除する。
- [x] **再接続時の ambiguous commit が AlreadyDecided に変換される** — 2026-11-19 対応済み: CAS 0行時／トランザクション失敗時に id で現在の decision を SELECT し、試行値と一致すれば `AlreadyCommittedByUs`（Ok）として扱う。二重 audit 行なし、CAS＋audit の同一 tx 構造とリトライ上限は不変。純粋ロジックの回帰4件追加（実 DB での lost-ack 完全再現は不可）。
  （likely high）— COMMIT 成功後にレスポンスが失われると再試行 CAS が0行となり、
  DB=approved / 待機側=Expired / operator=409 に分裂。再試行前に id で現在の
  decision を SELECT し、コミット済みなら publish して Ok を返す。
- [x] **起動リコンサイルの expired 決着が1件のエラーで全体中止** — 2026-11-19 対応済み: 行単位 best-effort 化（`AlreadyDecided` は成功扱い、他エラーは warn 可視化して継続）。未決着行が残った場合のみ起動失敗という fail-closed 契約を維持。回帰2件追加、I33-A の修正は不変。
  （likely medium）— 行ごとに結果を収集し AlreadyDecided は成功扱い、
  他エラーは warn＋メトリクスで可視化して残り行の決着を続行する。
- [x] **「単一 owner」コメントが実態と矛盾（unit と context の二重実行、**2026-11-19 対応済み: 方針を「`jarvis-context::builder.rs:862` の `migrate!()` が最終適用主体（埋め込みマイグレーションが権威）、unit は pre-flight 補助」と明文化。3 unit の「Single owner」表現を削除。build-selfhost.sh に同一コミット配布のマーカー照合ガードを追加し、配布ズレ時の checksum mismatch リスクを docs に記録。advisory lock で同時実行は直列化されるため二重適用自体は故障要因ではない** —
  範囲も 001-003 で 004 を含まない）**（confirmed medium）— 所有権を明確化し
  コメントを実ファイルと同期。
- [x] **発掘: 小さな残タスク5件** — **2026-11-19 全件対応済み: rollback 完全化、unit コメント 001-004、ui/README MSRV 1.100+、specs/test-backup.md に REQ-18/TC-B5（--retain-days 境界値）、specs/test-event.md に REQ-16/17（503 / RESOURCE_EXHAUSTED）を追加。evidence 2件を実測再生成（backup 23 checks / event 70 tests）し index 同期、check-evidence ok=6 errors=0**（当初: install-backup-timer rollback の
  migrations/verify 未復元（上記と同一）、unit コメントの 001-003→001-004、
  ui/README の Rust 1.85+→1.100+、specs/test-backup.md の `--retain-days`
  境界値拒否シナリオ、specs/test-event.md の 503/RESOURCE_EXHAUSTED 契約
  （後2件は evidence 再生成が必要）。

- 親が対応: clippy の collapsible_if、flaky なスパン捕捉アサーションの削除。

### Round 32 のレビュー／発掘結果

- [x] **migration unit が読む SQL ソースがどの配布経路からもコピーされない** — 2026-11-19 対応済み: build-selfhost.sh に `install -d /opt/jarvis/rust/migrations`＋`rust/migrations/*.sql` のコピー＋`.jarvis-source-commit` 生成を組み込み。SQL 0件／マーカー欠落・不一致は unit 生成前に fail-fast。`bash -n` / shellcheck OK。
  （confirmed high）— `jarvis-migrations.service:35` の `--source /opt/jarvis/rust/migrations`
  に対し、repo 全体に `rust/migrations/*.sql` を配置する処理が存在しない。
  build-selfhost.sh のガードはマーカー欠落時に警告のみで fail-open。
  → SQL の配置をデプロイ手順へ組み込み、欠落時 fail-fast にするか unit を廃止。
- [x] **build-selfhost.sh の Wants= 失敗伝播の記述が実態と逆**（confirmed medium）— 2026-11-19 対応済み: 「Wants= は失敗を伝播しない（fail-open）」へ訂正し、`systemctl is-active jarvis-migrations` による個別確認を案内。fail-closed 化の選択肢（Requires=+After / ExecStartPre CHECK）を docs に記録 —
  systemd の `Wants=` は弱い依存で unit が failed でも起動を続ける。
  「migration 失敗ならサービスも止まる」というコメント／docs を訂正するか、
  `Requires= + After=` または ExecStartPre の適用済みバージョン CHECK へ変更。
- [x] **lost-ack 対応が再接続失敗／最終試行のプローブ失敗時に機能しない** — 2026-11-19 対応済み: `read_back_after_error()` が MAX_DB_ATTEMPTS 世代ぶん再接続（非致命）＋read-back を反復し、OurCommit→Ok / CompetingDecision→AlreadyDecided / 不明→Persistence。approval.rs 側もタイムアウト時に CAS 勝者の公開決定を 100ms grace で拾い provisional Expired を訂正。回帰8件追加。
  （likely high）— `persistence.rs:380` の reconnect 失敗時にプローブへ到達せず、
  DB=approved / 待機側=Expired / operator=409(503) の分裂が残る。
- [x] **expired reconcile の `settled_expired` が試行件数を成功件数と誤報告** — 2026-11-19 対応済み: Ok / AlreadyDecided の行のみカウント。warning に `ApprovalError::category()` の固定語彙（NotFound / QueueFull / Persistence 等）を付与し、UUID・URL・SQL・本文は非出力。網羅テスト追加。
  （confirmed low）— 実際に Ok/AlreadyDecided だった件数へ変更し、warning に
  ApprovalError の固定語彙分類を付与。
- [x] **install-backup-timer の rollback が導入前から有効な timer まで停止** — 2026-11-19 対応済み: 最初の書き込み前に is-active / is-enabled を記録し、rollback は「この実行が新規有効化した場合のみ」stop/disable。既存有効だった場合は元の状態へ復元。`bash -n` / shellcheck / safety-test OK。
  （likely medium）— 適用前に is-active/is-enabled を記録し、この実行が新規に
  有効化した場合のみ stop/disable する。
- [x] **発掘: 仕様ギャップ5件** — **2026-11-19 対応済み: tool-matrix.md の egress allowlist を実装済み（DenyAll fail-closed）へ、test-ui.md を API/admin 2-scope（REQ-19/20、R-10）へ、test-context.md にスコア保持（REQ-12/13、R-8）と TC-P3 の Given を追加。evidence 2件を実測再生成（UI 36 / context 92）し index 同期、check-evidence ok=6 errors=0**（当初: tool-matrix.md の egress allowlist「未実装」
  記述、specs/test-mcp-gateway.md の egress/admin token 要件、
  specs/test-ui.md の 2-scope トークン前提、specs/test-context.md の
  スコア保持契約、同 TC-P3 の redact 期待値 `[X]` と既定 `[REDACTED]` の不一致。
- **P0 ガード有効化の設計材料**（i35-guard-design）: 8段階ロールアウト
  （kill switch token → state path → persistence → transport tokens →
  approval API → principal → webhook → P0-11 設計）と各段階の「値を見ない
  注入確認コマンド」を提示。実機 confirm で全ガード変数が ABSENT を確認。
  ユーザー確認事項: bws secret の実名、RPO/RTO、職務分離の要否、
  webhook 公開範囲と実送信者、kill switch ローテ周期、sudo 実行の可否。

- 親が対応: evidence 2件を実測再生成、flaky なスパン捕捉アサーションを削除。

### Round 33 のレビュー結果（未対応）

- [x] **コミット照合ガードがバイナリ上書き後に走る**（confirmed high）— 2026-11-19 対応済み: `cargo build` 直後に mktemp ステージングへバイナリ＋SQL をコピーし、両マーカー記録と照合を config/unit 生成・`/opt` 配置の前に移動。不一致・マーカー欠落・SQL 件数不整合は exit 1 で一切配置しない。`bash -n` / shellcheck OK。
  `build-selfhost.sh:36-38` で4バイナリを配置した後の `:44-56` で照合し、
  不一致なら exit 1 するため、新旧混在（新バイナリ＋旧 migration）が残る。
  照合とマーカー記録を `cargo build` 直後・バイナリ install 前に移動する。
- [x] **REQ-18/TC-B5 に対応する自動テストが無く evidence だけが pass を主張** — 2026-11-19 対応済み: `ci/scripts/test-backup-args.sh` を新規作成し6ケース（値欠落／空文字／非整数／0／負値／有効値）を dry-run で検証。既存 set の不変も確認。Makefile の `safety-test` / `ci-local` に組み込み、失敗時はエラーコードのみ出力。
  （confirmed medium）— `--retain-days` の境界値拒否は手動確認のみで回帰ゲートが無い。
  ci/scripts に回帰テストを追加し、TC 単位の実行記録を evidence に持たせる。
- [x] **flaky 修正でスパン捕捉アサーションを削除した結果、非漏洩テストが
  vacuous pass しうる**（confirmed medium）— 2026-11-19 対応済み: 両テストに `tracing::info!` イベントによる positive control（subscriber 健全性）を追加、`include_str!` ＋ `concat!` で `#[instrument(skip(...))]` 属性の静的ガードを追加。スパン捕捉自体はアサートせず flaky を再導入しない — subscriber がスパンを捕捉しなければ
  logs は空文字列で `!contains` はすべて真。テスト内で既知マーカの span を発行し
  捕捉を確認する deterministic な健全性アサーションを残す。
- [x] **BUILD_COMMIT の `unknown` フォールバックで fail-open**（likely medium）— 2026-11-19 対応済み: `git rev-parse` 失敗を明示 exit 1 とし、空文字・`unknown` も拒否。マーカー側も `unknown` でも照合を通過しない —
  git rev-parse 失敗時に記録される `unknown` とマーカー側が一致すると照合を通過する。
  rev-parse 失敗を明示 exit 1 に変更。
- [x] **backup エビデンスの passed=23 が runtime 宣言（2ゲート）と矛盾** — 2026-11-19 対応済み: runtime を実際に集計した `make test-verify-backup` へ限定し、notes の混在表記を削除。`check-evidence.sh` に notes の件数と results の整合チェック（NOTES_*_MISMATCH、値は非出力）を追加。6証跡すべて通过。
  （likely low）— safety-test の22件が反映されない。ゲートごとの内訳を記録する
  か runtime 宣言を実際の集計コマンドへ限定。

- 親が対応: clippy の collapsible_if（let chain 化）、evidence 2件の実測再生成。

### Round 34 のレビュー結果（未対応）

- [x] **expiry CAS の lost-ack で durable re-read が無く待機側が Expired を返す** — 2026-11-19 対応済み: `ApprovalPersistence::load_decision(id)` seam を追加（Noop は Ok(None)、Pg は committed 行のみ SELECT）。100ms grace 満了時に DB の権威決定でメモリ・待機側を上書きし tx.send。DB エラー／未決定は Expired フォールバック。lost-ack 回帰2件追加。
  （confirmed high）— `approval.rs:490-521`（特に 517-521 の warn + Expired）。
  並行 decide が CAS に勝っても watch::send が 100ms 以内に届かなければ、
  DB=approved / 待機側=Expired に分裂。`ApprovalPersistence` に id による
  decision 再読取り seam を追加し、grace 満了時に DB の権威決定で上書きする。
- [x] **read-back の生成リフレッシュが試行単位で掛け算され 503 が遅延** — 2026-11-19 対応済み: `read_back_after_error` を `read_back_once`（1 reconnect＋1 probe）へ置換し、各 attempt で1回のみ probe。接続世代は初期1＋最大3 reconnect に抑制（旧最大9回）。OurCommit / CompetingDecision 判定と上限は不変。
  （confirmed medium）— `persistence.rs:301-324` が1呼び出しで3回 reconnect＋probe し、
  呼び出し元がさらに3回トランザクション再試行（最大9回の TCP 接続）。
  単一の試行予算へ統合する。
- [x] **migration SQL 配布が上書きのみで古い *.sql を /opt に残す** — 2026-11-19 対応済み: 配置前に配布先とステージングのファイル名集合を照合し、余分な `*.sql` を `sudo rm -f` で削除してから新 SQL／マーカーを配置。失敗境界と順序は不変、`bash -n` / shellcheck OK。
  （likely medium）— `build-selfhost.sh:125` は上書きのみで削除しない。
  削除されたマイグレーションが残存すると unit の pre-flight が適用し、
  context の checksum 不一致で起動失敗しうる。配置前に余分なファイルを削除。
- [x] **timer rollback が next elapse を記録するだけで復元しない** — 2026-11-19 対応済み: 先頭コメントを「active/enabled を復元、next elapse は再計算で変わり得る」へ訂正。`pre_timer_next` は情報提供のみで rollback への復元を試みない旨を明記。ロジック変更なし。
  （confirmed low）— `install-backup-timer.sh:5-6` の「next elapse は restored」は
  誤記。active/enabled を復元し next elapse は再計算される旨へ訂正するか、
  OnCalendar 自体を退避・復元。
- [x] **UI の admin→API scope フォールバックが test-only の複製で検証され** — 2026-11-19 対応済み: `PolicyClient::approve` / `deny` の実 HTTP 送信経路を loopback スタブで検証するテスト5件を追加（admin 未設定時 API scope、admin 設定時 admin scope、両方未設定時ヘッダなし）。UI 41 tests pass。specs/evidence の「カバー済み」主張が実測裏付けを得た —
  証跡がカバー済みと主張**（likely medium）— `policy_client.rs:80-81` の
  test-only ヘルパのみを検証し、ランタイム送信パス（`:386-388`）は未検証。
  スタブサーバで実送信を検証し TC-U13 の Traceability に明示。

### Round 35 のレビュー結果（未対応）

- [x] **check-evidence の notes 件数照合が最初の一致しか検証しない** — 2026-11-19 対応済み: `re.finditer` ですべての件数一致を列挙し個別照合（値は非出力）。`test-legacy-guards.sh` に `notes_count_conflict` / `notes_passed_conflict` の tamper 2ケースを追加し、拒否を検証。6証跡すべて通过。
  （confirmed medium）— `check-evidence.sh:114,121` の `re.search` は1件目のみ。
  「23 checks ＋ 22 checks ＋ 23 passed」のような混在を通過させてしまう。
  `re.finditer` ですべての一致を照合し、`test-legacy-guards.sh` の tamper
  モードに「正しい件数＋矛盾する2つ目」を追加して回帰を担保。
- [x] **backup 証跡が REQ-18 カバレッジを主張しつつ集計ゲートがそれを実行して
  いない**（confirmed medium）— 2026-11-19 対応済み: runtime を `test-verify-backup + test-backup-args` へ拡大し `results.passed` を実測29（23+6）へ更新。notes は単一件数表記へ統一し、TC-B5 の6ケースを含む旨を明記 — runtime を `make test-verify-backup` に限定した
  結果、TC-B5 の6ケース（test-backup-args.sh）が23件に含まれない。
  requirements / notes から REQ-18 を外すか、6ケースを集計へ含めて実測再生成。
- [x] **コミット照合が dirty ツリーチェックを欠く**（likely medium）— 2026-11-19 対応済み: ビルド前に `git diff --quiet` / `git diff --cached --quiet` を必須ゲート化（dirty なら差分内容を出さず exit 1）。untracked は repo 全体では検査せず、`rust/migrations/*.sql` のみ `git ls-files --others` でペイロード化を遮断。`bash -n` / shellcheck OK —
  `build-selfhost.sh:47-54` は `git rev-parse HEAD` のみで作業ツリー差分を
  検査せず、両マーカーに同じ BUILD_COMMIT を書くため照合が常に真。
  ビルド前に `git diff --quiet && git diff --cached --quiet` を必須化し、
  dirty なら exit 1（必要なら untracked も検査）。

- 親が対応: rustfmt 差分（policy / ui）の自動修正。

### Round 36 のレビュー／発掘結果（未対応）

- [x] **lost-ack の ours-commit が最終試行の probe 失敗で 503 に後退** — 2026-11-19 対応済み: 最終 attempt の transport error に限り別 generation で probe を1回再試行（`probe_with_final_retry_resolution`）。決定的 Ok(None) は再試行せず、MAX_DB_ATTEMPTS=3 のトランザクション上限は維持。純粋回帰テスト追加。
  （confirmed high）— `persistence.rs:331-337,474-494` の `read_back_once` は1回の
  probe のみで、Err は Undecided→最終試行で Persistence(503)。試行予算統合の
  回帰。最終 attempt に限り probe を1回再試行するか、probe transport error 時に
  in-transaction read を再実行する。
- [x] **タイムアウト待機が 100ms grace 2回＋DB re-read で deadline 超過** — 2026-11-19 対応済み: AlreadyDecided 後に `load_decision_with_time` を即座に起動し、watch／durable／grace の3Way `tokio::select!` で先着を採用。待機は max(grace, DB RTT) に短縮。RECOVERY_GRACE / MAX_DECIDE_YIELDS を定数化。
  （confirmed medium）— `approval.rs:492-493,514-516,533`。2つの grace を統合
  するか load_decision を grace と並列開始して先着を採用。grace 長と再読取り
  回数を設定可能にして SLA を明示。
- [x] **durable re-read で復元した決定の decided_at がタイムアウト時刻で上書き** — 2026-11-19 対応済み: `LoadedDecision { decision, decided_at }` と trait 既定 `load_decision_with_time` を追加。DB の権威決定時刻で entry.decided_at を上書き（prune の保持判定が正確に）。既存モック／Noop は既定実装で吸収、http.rs の網羅 match 不変。
  （confirmed medium）— `approval.rs:559` の `entry.decided_at = Some(expired_at)`。
  DB の権威決定時刻とメモリが不一致で prune の保持判定が誤る。load_decision が
  decided_at も返すようにする。
- [x] **TC-U13 が本番起動配線（encoded bundle → new_with_token）を通っていない** — 2026-11-19 対応済み: `PolicyTokens::new(api, None/None, admin).encode()` → `new_with_token` → `decode_or_legacy` → `effective_admin_token` → POST の実送信テスト2件を追加（API-only / admin-only）。UI 43 tests pass。
  （likely medium）— API-only ケースが `new_with_tokens`（embedders 用）のみ。
  `PolicyTokens::new(Some(api), None).encode()` を本番経路へ渡すテストを追加。
- [x] **stale SQL 削除と新 SQL 配置が非原子的**（likely medium）— 2026-11-19 対応済み: unit の旧 active 状態記録→`systemctl stop`→stale rm＋新 SQL install→マーカー→旧 active なら start の順序を採用（初回は unit 存在確認でスキップ）。ディレクトリ swap は `--source` パス契約と renameat2 要件で不採用とコメント明記 —
  の rm→install の窓で migration unit が pre-flight しうる。入れ替え前に unit を
  stop するか staging ディレクトリを atomic rename で切り替える。
- [x] **発掘: 自動修正可能な残タスク5件** — **2026-11-19 うち4件対応済み: docs/architecture/AGENTS.md の参照先を実在ファイルへ、3 unit の 1Password コメントを bws run 実態へ、evidence-regeneration.md のゲート表・ファイル名検証・backup 再生成コマンドを現行化、ADR 3件＋テンプレートの date を RFC 3339 準拠へ。残る1件: specs/test-backup.md の TC-V2 Traceability 欠落＋:286 の件数表記（evidence 再生成が必要で別途対応）**（当初: docs/architecture/AGENTS.md の存在しない
  資料への参照、3 unit の旧 1Password コメント（ExecStart は bws run）、
  specs/test-backup.md の TC-V2 Traceability 欠落（evidence 再生成要）、
  docs/integration/evidence-regeneration.md が現行ゲートと矛盾、ADR 3件＋
  テンプレートの date が RFC 3339 違反（オフセット欠落）。

- 親が対応: なし（全ゲート初回通过）。

### Round 37 のレビュー結果（未対応）

- [x] **evidence/index.md の実測件数が6件中5件で古い**（confirmed medium）— 2026-11-20 対応済み: index の policy 53→144、context 17→92、backup 23→29、event 24→70、ui 14→36 へ同期（mcp 45/0/1 は既に一致）。backup 証跡も spec 変更に伴い hash `7e4e4b859e57…` へ実測再生成（23+6=29）、index の参照も更新。check-evidence ok=6 errors=0。
  index の 53/17/23/24/14 に対し実際は 144/92/29/70/36。`check-evidence.sh:237-250`
  はリンク存在のみ検査し数値主張を検証しない。索引の N passed と JSON results を
  照合するゲートを追加し、6件を実測値へ再生成。
- [x] **specs/test-backup.md:286 の件数表記が evidence 29件と矛盾** — 2026-11-20 対応済み: 件数表記を `test-verify-backup` 23 + `test-backup-args` 6 = 29 へ修正。あわせて TC-V2（REQ-4 / REQ-5）の Traceability 行2件を追加し、他5仕様とシナリオ一致。spec_hash 変更に伴う evidence 再生成も完了。
  （confirmed medium）— 23 checks + 22 checks と記載だが spec_hash は一致し
  ゲートが通る。29（23+6）へ更新して spec_hash／ファイル名／index を再生成。
- [x] **dirty gate が build-selfhost.sh 限定で deploy-mcp-gw-safe.sh が迂回** — 2026-11-20 対応済み: `deploy-mcp-gw-safe.sh` の `--apply` 配置前に rev-parse / clean-tree（unstaged＋staged）を必須化。マーカーは添付時のみ照合（現行ビルド経路との整合で未添付は best-effort）。dry-run / root / ロールバック / 段階別バックアップは不変、`bash -n` / shellcheck OK。
  （confirmed medium）— `deploy-mcp-gw-safe.sh:18,80,176` は
  `rust/target/release/jarvis-mcp-gw` をそのまま install し rev-parse／diff／
  マーカー照合が無い。同じゲートを追加するか mcp-gw 配置を build-selfhost 経由へ。
- [x] **untracked 検査が `--exclude-standard` で ignore 済み SQL を通す** — 2026-11-20 対応済み: 既存の untracked 検査は保持した上で、glob された各 `*.sql` を `git ls-files --error-unmatch` で追跡確認し、index に無いファイルは fail-closed（名前・差分は非出力）。`bash -n` / shellcheck OK。
  （likely low）— glob は全 `*.sql` を拾うのに git 側は ignore 済みを除外。
  ペイロード集合と照合対象を完全一致させる。
- [x] **試行予算の世代上限が契約と矛盾**（confirmed low）— 2026-11-20 対応済み: `GenerationBudget` 共有カウンタを導入し初期世代込み MAX_DB_ATTEMPTS(=3) 世代＝2 reconnect に制限。最終試行の probe／I40-A retry はスロットが残らず現行世代で実行。doc を実装へ同期し、生成世代数の pure 回帰4件追加 —
  `persistence.rs:39-43` は MAX_DB_ATTEMPTS 世代と明記するが最終試行でも
  reconnect するため gen0+3=4（I40-A 追加で最大5）。最終試行では reconnect
  をスキップするか共有カウンタで真の上限にし、世代数をテストで検証。

- 親が対応: clippy の manual map、型エラー（`rx.changed().is_ok()`）、rustfmt。

### Round 38 のレビュー結果（未対応）

- [x] **3Way select が durable 再読の Ok(None)/Err で早期 Expired を返し
  split brain が復活**（likely high）— 2026-11-20 対応済み: 即座採用は `Ok(Some(LoadedDecision))` のみに限定。`Ok(None)`/`Err` は durable_fallback に記録して RECOVERY_GRACE 満了まで watch 再確認ループを継続。「durable 即 Err ＋遅延 watch」回帰2件追加。 `approval.rs:593-608,655-659`。
  `Race::Durable` の Ok(None)/Err が grace 満了前に関数を `Ok(Expired)` で
  終了する。Ok(Some) のみ早期採用し、None/Err は grace 完了後の最終手段へ
  格下げ。「durable が即 Err + watch が遅れて届く」回帰を追加。
- [x] **Race::Grace の reread.await にタイムアウトが無く期限後待機が無界** — 2026-11-20 対応済み: 共有締切＋`tokio::time::timeout(RECOVERY_GRACE, &mut reread)` で最大1回だけ追加待機。タイムアウト時は request_id のみの固定 warn＋Expired 収束。stalled re-read の回帰テスト追加。
  （likely medium）— `approval.rs:586,609-630`。`tokio::time::timeout(残予算, reread)`
  で包み、タイムアウト時は固定語彙 warn＋Expired フォールバック。
- [x] **SQL 入れ替えの unit stop が Wants= で再起動され partial tree を読める** — 2026-11-20 対応済み: versioned ツリー（`migrations.d/<commit>-<ts>-<pid>`）を完成させてから固定 `--source` パスの symlink を `mv -Tf` の1回の rename で切替。入替え中は unit を mask（EXIT トラップで unmask）し Wants= 再起動も防止。`--source` パス契約は不変、`bash -n` / shellcheck OK。
  （likely medium）— `build-selfhost.sh:236-264` と policy/context の
  `Wants=jarvis-migrations.service` + `Restart=on-failure`。入れ替え中は
  mask するか policy/context も停止、あるいは symlink/atomic rename で
  reader 停止に依存しない切替へ。
- [x] **deploy-mcp-gw の provenance gate がマーカー未添付を許容** — 2026-11-20 対応済み: build-selfhost.sh が配置元 `rust/target/release/.jarvis-build-commit` にコミット＋4バイナリの SHA-256 を記録。deploy 側は欠落・空・HEAD 不一致・sha レコード欠落・不正 hex・バイナリ不一致をすべて fail-closed で拒否。
  （confirmed medium）— `deploy-mcp-gw-safe.sh:110-121`。build-selfhost.sh は
  `/opt/jarvis/bin/` にのみマーカーを置き `rust/target/release/` には置かない。
  配置対象と同ディレクトリへのマーカー生成を必須化し、欠落・空・HEAD 不一致を
  fail-closed で拒否（可能なら SHA-256 も照合）。
- [x] **最終試行の probe retry が枯渇した generation で再実行され lost-ack
  契約が死ぬ**（likely medium）— 2026-11-20 対応済み: `GenerationBudget::take_for_probe()` を追加し通常 probe は最後の1枠を温存。最終 retry が予約枠で別 generation を開き1回だけ再 probe。最大3世代の上限は維持、pure 回帰3件追加 — `persistence.rs:426-436`。budget 枯渇時は
  壊れた同一接続で2回目の probe を読むため OurCommit を検出できない。
  最終試行用に reconnect スロットを1つ予約するか、契約を実態へ合わせる。

- 親が対応: clippy の too_many_arguments、evidence/spec 実測再生成、index 件数同期。

### Round 39 のレビュー／発掘結果（未対応）

- [x] **migration 世代ツリーの prune が現在の symlink 実体を削除しうる** — 2026-11-20 対応済み: mtime 順ソート＋`readlink -f` の現役ツリー明示除外＋削除前 tripwire（現役と一致すれば fail-closed）。辞書順では現役が最古に来るケースを一時ディレクトリで再現確認。`bash -n` / shellcheck OK。
  （confirmed high）— `build-selfhost.sh:218,285,292-296` の prune が
  `sort`（辞書順）のため、commit SHA の辞書順で最新ツリーが古い側に入ると
  symlink 先が `rm -rf` される。mtime 順（`find -printf '%T@ %f\n' | sort -n`）
  にするか `readlink -f` の解決先を除外。
- [x] **タイムアウトが永続化前にメモリへ Expired を確定し prune が再試行しない** — 2026-11-20 対応済み: mark_expired を `deciding` でクレームし、コミット成功後にのみ decision/decided_at を確定。失敗時は Pending のまま残し次回 prune / reconcile が再試行。呼び出し側の Expired 返却契約は維持。DB 失敗注入回帰追加。
  （confirmed high）— `approval.rs:528-530,550,1010-1013`。mark_expired が
  Persistence(503) を返しても Expired が確定し、次回 prune は `is_expired`
  分支で mark_expired を呼ばず破棄。監査行が欠落。コミット成功後にのみ
  decision/decided_at を確定するか、未確認フラグで再試行。
- [x] **check-evidence が型不一致で整合チェック全体を黙ってスキップ** — 2026-11-20 対応済み: notes は str、passed/failed/skipped は厳密 int かつ >=0 を必須とし、違反を `BAD_NOTES_TYPE` / `BAD_RESULTS_TYPE`（値は非出力）で fail-closed。bool も type() is int で除外。6証跡すべて通过。
  （confirmed medium）— `check-evidence.sh:95-99,106-107,157-158`。skipped が
  str／notes が dict だと NOTES_* / skipped 検証が走らず、passed 捏造を通過。
  型違反を `BAD_NOTES_TYPE` / `BAD_RESULTS_TYPE` で fail-closed に。
- [x] **backup 仕様とエビデンスでリスク ID 集合が不一致（R-8 欠落）** — 2026-11-20 対応済み: `specs/test-backup.md` の Risk & tier に R-8（T3、unit >= 0.60、--retain-days 不正値による誤削除）を追加。TC-B5 の Traceability 行は既に Tier=T3 で整合。spec_hash 変更に伴い evidence を実測再生成（29件）し index 同期。
  （confirmed medium）— `specs/test-backup.md:59-67` は R-1〜R-7、evidence は
  R-8 を含む。Risk & tier に R-8（T3）を追加し、集合一致をゲート化。
- [x] **prune だけが AlreadyDecided を失敗扱いしゴーストを永久保持** — 2026-11-20 対応済み: prune Phase 2 で AlreadyDecided を成功扱いし、`load_decision_with_time` で権威決定をメモリへ反映＋watch 通知のうえ evict。re-read 不能でも CAS が decided を証明するため Expired として evict。reconcile / timeout と挙動統一、回帰テスト追加。
  （confirmed medium）— `approval.rs:1030-1046`。reconcile / timeout と挙動を
  揃え、AlreadyDecided を成功扱いにして evict する。
- [x] **発掘: 自動修正可能な残タスク5件** — **2026-11-20 全件対応済み: test-restore-local.sh を Makefile（.PHONY / 単独ターゲット / safety-test / ci-local）と ci.yml extra_gates へ配線、config テンプレート3件に実装済み変数のコメント追記、.PHONY の欠落4件（test は既存）追加、ui-build コメントを「UI 検証は CI 対象」へ訂正、dsh.md の ntfy を「実装見送り判断済み」／監査永続化を「実装済み」へ訂正。親が `printf '-- ...'` の5箇所を `printf '%s\n'` へ修正し警告解消**（当初: test-restore-local.sh が
  Makefile / ci-local / CI に未配線、config テンプレート3件に実装済み
  セキュリティ変数が未反映、Makefile の .PHONY が5ターゲット欠落、
  ui-build コメントが「CI 対象外」のまま、dsh.md の通知／監査永続化が
  「未実装・判断待ち」のまま（実際は判断済み／実装済み）。

- 親が対応: tokio::select! の構文（カンマ2箇所＋ガードの `, if`）、rustfmt。

### Round 41 のレビュー／発掘結果（R4 で修正済み）

- [x] index の実測件数ゲートが未実装（confirmed high）→ `INDEX_RESULT_MISMATCH`
  を実装し、tamper 4ケース（index/型2/Scenario）を追加。
- [x] assume-unchanged/skip-worktree が clean-tree と tracked-SQL 検査を迂回
  （confirmed high）→ `git cat-file blob HEAD:<path>` の内容ハッシュ照合と
  `ls-files -v` の index-bit 検査を追加。
- [x] クラッシュ残存 mask が管理者 mask と誤認され tee が /dev/null へ
  （likely high）→ 永続マーカー付与＋優先 unmask、tee 直前に symlink assert。
- [x] deciding クレームが await を跨ぎキャンセルで永久スタック（likely high）
  → `DecidingClaim(AtomicBool)` + `DecidingGuard(Drop)` の RAII 化。
- [x] jarvis-event の TOML パスだけ CWD 相対（confirmed medium）→
  `/etc/jarvis/event.toml` + `JARVIS_EVENT_CONFIG_PATH` へ統一。

### Round 41 の残り指摘（未対応）

- [ ] config 4テンプレートに実装済み変数が未反映（confirmed medium）—
  event の CONTEXT_MODE/CONTEXT_GRPC_ADDR/CONTEXT_GRPC_TOKEN、context の
  EMBEDDING_DIM、policy の REQUIRE_* ガード、mcp-gw の UPSTREAMS など。
  R4-E で一部追記済みだが、設定キーの網羅性を再確認する。
- [ ] check-evidence の tamper 期待値が増えたため、次回以降も
      `make safety-test` で全ケースの維持を確認する。

### Round 42 のレビュー結果（R4 で修正済み）

- [x] context gRPC が内部エラー文字列をあらゆる経路へエコー（confirmed high）
  → `INTERNAL_ERROR_MESSAGE` 固定＋固定語彙カテゴリのみログ、非開示回帰追加。
- [x] policy gRPC の kill switch Persistence detail／MCP tools/call の Forbidden
  が生エコー（confirmed medium）→ 両方固定語彙化、回帰追加。
- [x] prune が entries 書き込みロックを DB コミットの間保持（confirmed high）
  → 3フェーズ分離（ロック下分類／ロック外コミット／短いロック適用）、
  「prune 中に status が応答」回帰追加。
- [x] kill switch の check-then-act TOCTOU（confirmed high）→ `decide()` が
  `commit_gate` で権威状態をコミット境界とpublish前に再読、3件回帰追加。
- [x] 設定／ドキュメントの既定値ドリフト（egress unset／SANDBOX_TYPE／
  timeout_ms clamp／BASE_DIR）を訂正、CONFIG_PATH と OPENCODE_PASSWORD も確認済み。

### Round 42 の残り指摘（未対応）

- [x] 未使用依存の削除候補（confirmed）— tower-http（全4サービス＋workspace）、
  zeroize（policy）、prost-types（policy/mcp-gw）、figment json feature、
  uuid v7 feature、rmcp の transport-child-process / transport-sse-client。
  削除後に `cargo check --all-targets --locked` と `make ui-test` で検証。〔Round 43 で対応済み〕
- [x] 並行レビューの残り: select! の unbiased による期限境界 split brain、
  Connection::reconnect のキャンセル、symlink swap と bin マーカーの不一致窓。〔Round 43 で対応済み〕
- [x] jarvis-context の outbox/Qdrant 失敗ログが `{error:#}` で anyhow
  チェーン全体を OTLP logs へ送出（medium）— 固定語彙化が必要。〔Round 43 で対応済み〕
- 親が対応: fmt 差分、JoinHandle 型不一致（spawn 内で Result を unwrap）、
  tracing field 形式依存アサーションの削除。

### Round 43（R4 で修正済み）

- [x] 未使用依存を削除: tower-http（workspace＋4サービス）、zeroize、
  prost-types、figment json feature、uuid v7（workspace）と v4（UI）、
  rmcp の transport-child-process / transport-sse-client。`Cargo.lock` の
  orphan（process-wrap / windows-* 等）も整理。`--locked` で整合確認済み。
- [x] jarvis-context の outbox/Qdrant 失敗ログを固定語彙化（`error_kind`
  と id/attempts のみ、anyhow チェーン全体は非出力）。回帰テスト追加。
- [x] 期限境界 select! に `biased`＋Grace 分岐の watch 最終確認、
  `Connection::reconnect` をキャンセル安全化（完全 open → mem::replace → 旧 driver abort）。
- [x] bin マーカーを versioned ツリー経由の symlink で公開し migration swap と
  同一の原子境界で更新。

### Round 43 の残り指摘（未対応）

- [x] commit_gate のコミット後再読が「永続化済みだが未公開」を作る
  （confirmed medium）— DB=approved / HTTP=409 / メモリ=Pending の分裂。
  コミット前に1回に限定するか、コミット済みとして決定を返す。〔Round 44 で対応済み〕
- [x] DecidingGuard が共有 bool で世代/オーナーを持たず、prune と timeout が
  同じ Arc に第2ガードを作り得る（medium）— 世代付きトークン化＋
  apply_prune の `decided_at.is_some()` 温存。〔Round 44 で対応済み〕
- [x] mcp_allowlist.toml の timeout_ms 一律15000化は web.fetch 以外の
  shell.execute / github.create_pr / web.search に不要な機能後退（high）—
  30000 へ戻すかルートごとの実効値を明記。〔Round 44 で対応済み〕
- [x] egress テンプレートの「unset は deny-all」が unit の example.com
  フォールバックと矛盾（low）。〔Round 44 で対応済み〕
- [x] policy gRPC の kill switch エラーログだけ `{error:#}` でチェーン全体を
  OTLP へ送出（medium）— context 側と同じ固定語彙化が必要。〔Round 44 で対応済み〕

- 親が対応: builder.rs の未定義変数 `expected`、`embedding_dim` 修正、
  tracing field 形式依存アサーション削除、secret_scan 誤検出（PROBE 定数＋
  `concat!` 分割）。

### Round 44（R4 で修正済み）

- [x] commit_gate のコミット後再読による「durable 済み・未公開」分裂
  （confirmed medium）→ ゲート読みをコミット前の1回に限定し、コミット成功を
  権威として常に publish。TOCTOU 回帰の期待値を更新（回帰名は保持）。
- [x] DecidingGuard の共有 bool に世代／オーナーが無い競合（confirmed medium）
  → 奇数=有効／偶数=無効の単調世代トークンへ置換し、古いガードの Drop は
  no-op。`apply_prune` は `decided_at.is_some()` なら send/evict をスキップ。
- [x] mcp_allowlist.toml の timeout_ms 一律15000化が他ルートの機能後退
  （confirmed high）→ shell.execute / github.create_pr / web.search を 30000
  へ復元、web.fetch は clamp 上限 15000 を維持。注記で範囲を明示。
- [x] egress テンプレートの3層優先順位を明記、policy gRPC の kill switch
  エラーログを固定語彙 `error_kind` 化（OTLP への生テキスト送出を遮断）。

### Round 44 の残り指摘（未対応）

- [x] 承認永続化が生の tokio-postgres エラーを OTLP ログへ送出
  （confirmed medium）— `persistence.rs:316,720,778,840-847,902-903` の
  `error = %error` を固定語彙化（context 側と契約を揃える）。〔Round 45 で対応済み〕
- [x] OTLP endpoint／初期化エラーが原文でログされる（low）—
  `policy/main.rs:133-136` の endpoint URL（userinfo/query token の可能性）を
  固定語彙へ丸める。〔Round 45 で対応済み〕
- [x] Qdrant コレクション次元チェックの失敗がサイレント fail-open
  （medium）— `builder.rs:1048-1056` の Err(_) => warn + Ok(()) を起動失敗
  または初回 upsert での実次元検証へ変更。〔Round 45 で対応済み〕

- 親が対応: fmt 差分、Rust 1.100 の deprecated `fetch_update` → `try_update`（2箇所）。

### Round 45（R4 で修正済み）

- [x] 承認永続化が生 tokio-postgres エラーを OTLP へ送出（confirmed medium）
  → 固定語彙 `DbOperation` ＋ `db_error` / `db_attempt_warn` の2関数へ集約、
  生エラーを型レベルで排除。PROBE 非漏洩＋構造ガードの回帰2件追加。
- [x] OTLP endpoint／初期化エラーが原文でログされる（low）→ `endpoint_configured`
  と固定 `error_category` のみ、警告＋起動継続契約は維持。回帰1件。
- [x] Qdrant 次元 inspection 失敗がサイレント fail-open（medium）→ transport
  エラーは固定語彙 error! ＋ `bail!` で起動失敗、`Ok(None)` は既存どおり警告のみ。

### Round 45 の残り指摘（未対応）

- [x] **systemd `Environment=` の `${VAR:-default}` は展開されない**
  （confirmed high、Runtime Proof 付き）— `jarvis-mcp-gw.service:58,62` の
  BASE_DIR / EGRESS_ALLOWLIST がリテラル文字列として渡り、未上書き時は
  filesystem.* 全失敗／deny-all になる。systemd.exec(5) は変数展開を行わない。
  `Environment=` をリテラル既定値へ変更し EnvironmentFile を唯一の上書き経路と
  するか、shell ラッパで展開。併せて `${` 残存を fail-closed で検査。〔Round 46 で対応済み〕
- [x] kill switch 本体の `warn!` が自由入力 reason と `{error}` を OTLP へ送出
  （medium）— `kill_switch.rs:246-252,357-361`。`redacted()` と同じ固定語彙化。〔Round 46 で対応済み〕
- [ ] commit_gate 1回化の新契約が spec / UI / 回帰名に未反映（medium）—
  TC-K2 への追記、UI の「承認済み・実行は非常停止中」表示、回帰名改名。
- [x] `decide()` の publish が既存決定を無条件上書き（medium）—
  timeout / prune と同じ勝者温存ルールを適用（Noop バックエンドで競合）。〔Round 46 で対応済み〕
- [ ] 発掘: rustfmt ゲートが CI 未配線、Cedar スキーマ旧コメント、
  opencode.jsonc が build-selfhost 配布対象外、infra/README の 1Password 表記、
  dsh.md / tool-matrix.md の tools/list 非表示記述。

- 親が対応: fmt 差分、構造ガードの誤った切断位置（`#[cfg(test)]` → テストモジュール）。

### Round 46（R4 で修正済み）

- [x] **systemd `Environment=` の `${VAR:-default}` が展開されない**（confirmed high、
  Runtime Proof 付き）→ `jarvis-mcp-gw.service` の BASE_DIR / EGRESS_ALLOWLIST を
  リテラル既定値へ変更、EnvironmentFile を唯一の上書き経路と明記。
  `check-deploy-drift.sh` に未展開 `${` の fail-closed ガード追加。
- [x] kill switch 本体の `warn!` が自由入力 reason と `{error}` を OTLP へ送出
  （medium）→ `reason_kind()` 共通分類で固定語彙化、PROBE 回帰2件。
- [x] `decide()` の publish が既存決定を無条件上書き（medium）→ timeout / prune
  と同じ勝者温存ルールを適用、Noop 競合回帰2件。
- [x] commit_gate 1回化の新契約を spec TC-K5 / UI 状態表示へ反映（UI 46 tests）。

### Round 46 の残り指摘（未対応）

- [x] OTLP 固定語彙化が policy のみ（medium）— event / context / mcp-gw の
  `main.rs` は `.build()?` のまま。失敗時に ExporterBuildError が stderr/journal
  へ流出。4サービスで契約統一（固定 warn＋継続、または全サービス起動失敗）。〔Round 47 で対応済み〕
- [x] Qdrant の生エラー `e.to_string()` が collection_info 以外の5経路に残存
  （medium）— `ContextError::Qdrant(String)` を固定語彙化し、型レベルで排除。〔Round 47 で対応済み〕
- [x] `configured_vector_size` の複数 `?` が「応答未解析」を「多ベクトル」
  と誤分類して fail-closed を迂回（medium）— Ok(None) を2意味へ分離。〔Round 47 で対応済み〕
- [x] 新規コレクション作成時に実次元を読み戻さず `Ok(())`（low）—
  create_collection 後も collection_info で検証。〔Round 47 で対応済み〕
- [ ] persistence の構造ガードが `tracing::` 文字列計数（low）— AST ベース
  または型境界での強制へ変更。

- 親が対応: ureq 3 の `Error::StatusCode`（ボディ破棄）に対応するため
  プローブ専用 agent（`http_status_as_error(false)`）を追加。仕様変更に伴い
  evidence を実測再生成（policy 193件）。

### Round 47（R4 で修正済み）

- [x] OTLP 契約が policy のみ（medium）→ event / context / mcp-gw も固定語彙
  warn＋起動継続へ統一（定数名・値も policy と一致）、PROBE 回帰各1件。
- [x] Qdrant の生エラー `e.to_string()` が5経路に残存（medium）→
  `QdrantErrorKind` ＋ `discard_qdrant_error` で型レベル破棄、非漏洩回帰2件。
- [x] 次元レスポンス未解析の fail-open と新規コレクションの実次元未検証
  （medium/low）→ 未解析は Err 化、作成後も collection_info で検証、回帰2件。

### Round 47 の残り指摘（未対応）

- [x] **EnvironmentFile のパス不一致**（confirmed high）— unit は
  `/etc/jarvis/jarvis-mcp-gw.env` を読むが repo テンプレートは
  `infra/config/mcp_gateway.env` のままで配置経路が無い。未配置だと
  BASE_DIR が開発用リテラルのまま filesystem.* 全失敗、egress が
  example.com を許可する fail-open。パス統一＋配置スクリプト追加。〔Round 48 で対応済み〕
- [x] kill_switch.rs の restore() / persist() に state ファイル絶対パスと
  生エラーが残存（medium）— activation と同じ固定語彙化、PROBE 回帰拡張。〔Round 48 で対応済み〕
- [x] UI の「承認済み・実行は非常停止中」が2ポールで消える（medium）—
  kill switch 中表示用の独立フラグで status 上書きを抑制。〔Round 48 で対応済み〕
- [x] I48-C の 409/AlreadyDecided を UI が「失敗」と表示（medium）—
  競合時に勝者の決定を返すか UI で勝者 status を表示。〔Round 48 で対応済み〕
- [x] health probe 用 ureq agent にタイムアウトが無い（medium）—
  メイン agent と同じ timeout_global / timeout_connect を設定。〔Round 48 で対応済み〕
- [x] 発掘: code-server PASSWORD_FILE テンプレート残存、未使用 dev-deps
  （mockall / proptest）、specs/index.md の REQ 同期、dsh.md / tool-matrix.md
  の tools/list 非表示記述。〔Round 48 で対応済み〕

- 親が対応: fmt 差分のみ。

### Round 48（R4 で修正済み）

- [x] EnvironmentFile パス不一致（confirmed high）→ repo テンプレート
  `mcp_gateway.env` を配置元とし unit が読む `/etc/jarvis/jarvis-mcp-gw.env`
  へ deploy が原子的配置。drift 検査に存在／必須キーの fail-closed ガード。
- [x] kill_switch restore() / persist() のパス・生エラー残存（medium）→
  activation と同じ固定語彙化、PROBE 回帰4件追加。
- [x] UI の非常停止表示が2ポールで消える／409 を「失敗」と表示／probe
  タイムアウト無し（medium 3件）→ 独立ピンフラグ、409→winner 取得、
  probe agent へ timeout 設定。UI 52 tests pass。
- [x] 未使用 dev-deps（mockall / proptest）削除、code-server PASSWORD_FILE
  テンプレート整理、仕様索引／旧 tools/list 記述の同期。

### Round 48 の残り指摘（未対応）

- [x] **jarvis-context HTTP 境界が anyhow チェーン全体を OTLP ログへ送出**
  （confirmed high）— `http.rs:277-285` の `log_internal_error` が
  `format!("{err:#}")` を error フィールドへ。gRPC の固定語彙化と不整合。〔Round 49 で対応済み〕
- [x] 3サービスの SdkTracerProvider 未 shutdown（medium）— Round 47 で
  失敗 warn は統一したがライフサイクルは policy のみ TracerGuard 保有。〔Round 49 で対応済み〕
- [x] 多ベクトル collection の Ok(None) 警告継続が fail-open（medium）—
  実データは名前なしベクトル固定のため全 upsert/search が失敗しうる。〔Round 49 で対応済み〕
- [x] Qdrant バリアントが公開 String（medium）— 型レベル排除ではなく
  `pub(crate)` コンストラクタ＋Newtype／QdrantErrorKind への変更が必要。〔Round 49 で対応済み〕
- [ ] 新規コレクション作成直後の読み戻しが一時障害で起動拒否（low）。

- 親が対応: fmt 差分、テスト専用 API（approve / deny / into_decision）へ
  `#[cfg(test)]` 付与。

### Round 49（R4 で修正済み）

- [x] jarvis-context HTTP が anyhow チェーン全体を OTLP ログへ送出
  （confirmed high）→ `log_internal_error` を gRPC と同じ7語彙の variant トークン
  のみへ変更、PROBE 非漏洩＋語彙パリティ回帰2件。
- [x] 3サービスの SdkTracerProvider 未 shutdown（medium）→ 各 main に
  TracerGuard（Drop で shutdown）導入、固定語彙 warn、回帰2件×3。
- [x] 多ベクトル collection の warn 継続が fail-open（medium）→ 起動時
  fail-closed（名前なしベクトル固定のため全 upsert/search が失敗する経路を遮断）、回帰3件。
- [x] Qdrant バリアントの公開 String（medium）→ private-token newtype
  `QdrantErrorKind` へ変更、`pub(crate)` コンストラクタのみで生成。

### Round 49 の残り指摘（未対応）

- [x] **kill switch 発動の HTTP 409 を UI が AlreadyDecided と誤認**
  （confirmed high）— `http.rs:670-671` が KillSwitchActivated と AlreadyDecided
  の双方を409へマップし、UI が「既に決定済み: 保留中」と false-success 表示。
  固定本文で分岐するか 423 等へ分離。〔Round 50 で対応済み〕
- [x] 409 winner 取得が API スコープトークン固定（medium）— admin-only 構成で
  401 に後退。`winner_decision()` も `effective_admin_token()` を使う。〔Round 50 で対応済み〕
- [x] deploy/drift の BASE_DIR 必須キー検査が空値を OK（medium）—
  非空・絶対パス・実在ディレクトリを必須化（egress の空値は正当として維持）。〔Round 50 で対応済み〕
- [x] build-selfhost.sh が mcp_gateway.env を配置せず新規導入で fail-open
  （high）— config 配置ブロックへ追加するか EnvironmentFile の `-` を外す。〔Round 50 で対応済み〕
- [x] probe timeout 回帰が vacuous（medium）— 600ms 応答で3秒未満アサート
  を満たしてしまう。5秒以上応答にして timeout 発動を直接観測。〔Round 50 で対応済み〕
- [x] 3サービスのシグナル停止（SIGTERM）で Drop が走らずスパン未フラッシュ
  （low）— policy 同様の shutdown_signal + select! が必要。〔Round 50 で対応済み〕

- 親が対応: fmt 差分、tracer shutdown の `&raw_err`（3箇所）、
  `CollectionInfo` 完全修飾、http.rs の旧 Qdrant String API（2箇所）、
  `anyhow::Error::from` 明示。

### Round 50（R4 で修正済み）

- [x] **kill switch 発動の HTTP 409 を UI が AlreadyDecided と誤認**（high）→
  409 を維持したまま固定本文で分岐、UI は KillSwitchBlocked として非常停止を
  ピン留め（winner GET を呼ばない）。
- [x] 409 winner 取得が API トークン固定（medium）→ status エンドポイントを
  ApiOrAdmin スコープ化（list は API 専用を維持）、UI も admin フォールバック。
- [x] probe timeout 回帰が vacuous（medium）→ 5秒応答＋`ureq::Error::Timeout`
  直接観測へ変更。UI 58 tests pass。
- [x] build-selfhost.sh が mcp_gateway.env を配置せず新規導入で fail-open
  （high）→ config 配置ブロックへ root:root 0600 で追加。BASE_DIR 検査を
  非空・絶対パス必須化（egress 空値は deny-all として正当）。
- [x] 3サービスのシグナル停止でスパン未フラッシュ（low）→ policy 同等の
  shutdown_signal（ctrl_c + SIGTERM）と select! によるリスナー停止、回帰3件。

### Round 50 の残り指摘（未対応）

- [x] 単一 named vector collection が fail-open（medium）— `named >= 1` を
  拒否すべき（`Config::ParamsMap` は要素数に関わらず named モード）。
  既存テスト `single_vector_collections_are_not_rejected_by_multi_vector_guard`
  が fail-open を期待値として固定している。〔Round 51 で対応済み〕
- [x] HTTP の store_memory / search_memory が log_internal_error を通らず
  内部エラーを無ログで破棄（medium）— 全6 RPC を同じヘルパへ。〔Round 51 で対応済み〕
- [x] context の不正 JSON / UUID が `unwrap_or_default` でサイレントに成功
  （medium）— HTTP 400 / gRPC InvalidArgument へ変更。〔Round 51 で対応済み〕
- [x] TracerGuard の Drop が shutdown 失敗を握り潰す（low）。〔Round 51 で対応済み〕
- [x] HTTP/gRPC 語彙パリティテストが gRPC 側を検証していない（low）。〔Round 51 で対応済み〕
- [ ] 発掘: local-restore-design.md / remaining-tasks.md の古い記述、
  jarvis-core の未参照共有型、backup.service の開発用絶対パス、
  docs/architecture の Structure ツリー。

- 親が対応: fmt 差分、ReturnConv `atomic.store(true, Ordering)`（3箇所）。

### Round 51（R4 で修正済み）

- [x] 単一 named vector collection が fail-open（medium）→ `named >= 1` を
  拒否（`Config::ParamsMap` は件数に関わらず named モード）、既存テスト更新
  と fail-closed 回帰追加。
- [x] HTTP の store/search が log_internal_error を通らず無ログ（medium）→
  全6 RPC を固定語彙ヘルパへ統一、横断テスト追加。
- [x] context の不正 JSON / UUID が unwrap_or_default でサイレント成功
  （medium）→ HTTP 400 / gRPC InvalidArgument、9件の回帰追加（additive
  field の degrade 契約は維持）。
- [x] TracerGuard の Drop が shutdown 失敗を握り潰す／語彙パリティが
  self-confirmation（low 2件）→ AlreadyShutdown 以外を warn、
  include_str! で gRPC 実ソースの語彙を照合。

### Round 51 の残り指摘（未対応）

- [x] **kill switch 409 の非常停止ピンが probe の fail-open で1ポール後に
  解除される**（confirmed high）— 409 本文で KillSwitchBlocked を確定した
  直後に /health probe が false を返すと pin が落ちる。`kill_switch_active ||
  kill_switch_blocked` で維持するか、probe を fail-closed 化。〔Round 52 で対応済み〕
- [x] **BASE_DIR 絶対パス検査が重複キーで迂回**（confirmed high）— grep の
  any-match セマンティクス。最後の割り当て行のみを検査し、同一キーの重複を
  DANGER 化、`/` と `..` 成分も拒否。〔Round 52 で対応済み〕
- [x] **mcp-gw env の配置が非アトミック**（confirmed high）— mv 成功後に
  install 失敗すると env 欠落で unit リテラル既定が実効化。一時パス install
  ＋ mv -f、trap で .bak 復元、退避ファイルも 0600。〔Round 52 で対応済み〕
- [x] SIGTERM が in-flight の承認コミット／ツール実行をドロップ（medium）—
  drain 待ちを実装するか「即時停止」とログ／systemd TimeoutStopSec を一致。〔Round 52 で対応済み〕
- [x] 409 固定本文が UI と policy で複製（medium）— jarvis-core の共有定数
  または cross-crate 契約ゲートへ。〔Round 52 で対応済み〕

- 親が対応: fmt 差分、anyhow `context()` の move（qdrant）、未使用 import、
  SDK 自身の DEBUG ログを捕捉したテスト（自アプリ WARN 行のみへ限定）。

### Round 52（R4 で修正済み）

- [x] **kill switch 409 の非常停止 pin が probe 一時障害で解除**（high）→
  probe を fail-closed 化（エラー時は停止中）＋409 由来の latch を sticky に
  OR。UI 61 tests pass。
- [x] **BASE_DIR 検査が重複キーで迂回**（high）→ 最終割り当て行のみ検査、
  同一キー重複を DANGER、`/` と `..` 成分を拒否。deploy 側にも同じロジック。
- [x] **mcp-gw env 配置が非アトミック**（high）→ 一時パス install ＋
  `mv -f`、.bak 0600＋trap 復元、配置直後に `--env-only` drift 検査。
- [x] 409 固定本文を jarvis-core 共有定数へ一本化、UI 契約テストを
  独立 fixture との一致 pin へ置換（self-confirmation 解消）。
- [x] SIGTERM を「即時停止（in-flight 保証なし）」と明記、旧 "exiting
  cleanly" ログを削除、TimeoutStopSec 30→10。

### Round 52 の残り指摘（未対応）

- [x] **gRPC search_memory が符号付き limit を `as usize` で受け流す**
  （confirmed high）— `limit=-1` が `usize::MAX` になり Qdrant へ実質上限なし。
  `usize::try_from` で InvalidArgument 化＋共通上限、回帰追加。〔Round 53 で対応済み〕
- [x] jarvis-policy の TracerGuard::Drop だけ Round 51 修正から漏れ、
  早期リターンでスパン消失が無記録＋生 err を eprintln（medium）。〔Round 53 で対応済み〕
- [x] 語彙パリティ／6RPC 回帰がソース文字列スキャン依存（medium）—
  `error_kind` を pub(crate) 化して実戻り値比較、gRPC 側にも振る舞いテスト。〔Round 53 で対応済み〕
- [x] store_memory が embedding 次元を検証せず書き込み時 fail-open
  （medium）— `embedding.len() == embedding_dim` を 400/InvalidArgument で拒否。〔Round 53 で対応済み〕
- [x] metadata が object 以外でも parse 成功、expires_at 範囲外が黙って
  None 降格（medium）— 共通ヘルパで拒否（additive degrade とは区別）。〔Round 53 で対応済み〕

- 親が対応: UI fmt 差分のみ。

### Round 53（R4 で修正済み）

- [x] **gRPC search_memory が符号付き limit を `as usize` で受け流す**
  （confirmed high）→ `validate_search_limit(i64)`（try_from＋上限1000）を
  HTTP/gRPC 共通ヘルパへ集約、負値・上限超過を 400/InvalidArgument 化。
- [x] store_memory の embedding 次元・metadata object・expires_at 範囲を
  共有 validation モジュールで検証（additive degrade 契約は維持）。
- [x] jarvis-policy の TracerGuard::Drop を固定語彙 warn 化（他3サービスと
  統一）、早期リターン経路の回帰追加。
- [x] 語彙パリティを実戻り値比較へ変更、`GrpcBackend` 注入で gRPC 3 RPC ×
  7語彙の実呼び出し検証を追加（文字列スキャンは補助へ格下げ）。

### Round 53 の残り指摘（未対応）

- [x] **egress allowlist の重複キー検査が未実装**（confirmed high、Runtime
  Proof 付き）— 先頭空値・末尾 example.com で `--env-only` が OK を返す。
  BASE_DIR と同じ重複拒否＋最終行検査を egress にも適用。〔Round 54 で対応済み〕
- [x] 409 本文の読み取り失敗が空文字→AlreadyDecided に倒れる（medium）—
  `read_to_string().unwrap_or_default()` を Result 保持へ。〔Round 54 で対応済み〕
- [x] kill switch latch が 409 より前に取得した probe で解除される TOCTOU
  （medium）— generation／時刻を持たせ 409 確定以降の probe のみ解除。〔Round 54 で対応済み〕
- [x] デプロイロールバックが env を削除し example.com fail-open を情報表示
  のみで放置（high）— テンプレートを書き戻すか unit を fail-closed 化。〔Round 54 で対応済み〕
- [x] I54-D 即時停止が event/context に未適用（"exiting cleanly" 残存、
  TimeoutStopSec=30）— policy と同じ固定語彙・10秒へ揃える。〔Round 54 で対応済み〕
- [ ] 発掘: UI/mcp-gw の仕様・証跡が古い（36/45 件のまま）、check-evidence
  の requirements 集合未検証、Runbook ゲート表の古さ、mcp_allowlist 旧コメント。

- 親が対応: fmt 差分、`tracing_subscriber::fmt()` 修正（3箇所）、
  grpc.rs テストの UnifiedEvent / MemoryEntry import。

### Round 54（R4 で修正済み）

- [x] **egress allowlist の重複キー検査が未実装**（confirmed high、Runtime
  Proof 付き）→ BASE_DIR と同じ重複拒否＋最終割り当て行検査を egress にも
  適用。空値は deny-all として正当。
- [x] **デプロイロールバックが env を削除し fail-open**（high）→ .bak が
  無ければ repo の deny-all テンプレートを root:root 0600 で書き戻し、
  ロールバック後 drift DANGER を exit 3 の強制ゲート化。
- [x] 409 本文読み取り失敗の fail-open→fail-closed、latch に generation を
  付与し古い probe が 409 を解除する TOCTOU を解消（UI 64 tests）。
- [x] event/context も即時停止方針へ統一（旧 "exiting cleanly" 削除、
  TimeoutStopSec 30→10）。
- [x] UI / mcp-gw の仕様・証跡を実測へ同期（UI 64、mcp-gw 124/1 ignored）。

### Round 54 の残り指摘（未対応）

- [x] gRPC embedding に NaN/±Inf を送ると有限性チェック無しで Qdrant へ
  （medium）— `validate_embedding` に `is_finite()` を追加。〔Round 55 で対応済み〕
- [x] HTTP の axum JsonRejection が bare-400／no-leak 契約を迂回（medium）—
  型不正で詳細本文が返る。カスタム extractor または Value 受け。〔Round 55 で対応済み〕
- [x] 語彙パリティ実呼び出しテストが RPC ごとの割当てを検証していない
  （low）— operation とカテゴリの組を1回ずつアサート。〔Round 55 で対応済み〕
- [x] OTel SDK 自身の DEBUG 行が RUST_LOG=debug で生エラーを残す（low）—
  EnvFilter に `opentelemetry_sdk=warn` 等を追加。〔Round 55 で対応済み〕
- [x] 共有 validation が content/source/metadata のサイズ境界を未定義のまま
  （low）— MAX_*_BYTES と深さ上限を追加。〔Round 55 で対応済み〕
- [x] check-evidence が requirements 集合と Traceability の REQ 値を照合
  せず（medium）、Runbook ゲート表が現行エラーコードと乖離（low）。〔Round 55 で対応済み〕

- 親が対応: UI fmt 差分、spec 変更に伴う evidence 実測再生成
  （UI 64 / mcp-gw 124）、notes 件数同期。

### Round 55（R4 で修正済み）

- [x] embedding の NaN/±Inf 無検証（medium）→ `validate_embedding` に
  `is_finite()` 追加、両 ingress＋core recheck で 400/InvalidArgument。
- [x] axum JsonRejection が bare-400/no-leak 契約を迂回（medium）→
  `BareJson<T>` extractor で型不正も空本文400＋固定語彙ログへ集約。
- [x] 語彙パリティを RPC 別検証へ（low）→ 各(case,RPC)に独立バッファ、
  (operation,error_kind) の組 21×2 を個別アサート。
- [x] OTel SDK DEBUG の生エラー（low）→ 4サービスに
  `opentelemetry_sdk=warn` / `opentelemetry_otlp=warn` のガード追加。
- [x] content/source/metadata のサイズ・深さ境界（low）→ 共有 validation
  に MAX_* 定数と深さ上限、両 ingress で拒否。
- [x] check-evidence に REQ 集合一致ゲート（`REQ_SET_MISMATCH`）追加、
  tamper 回帰と Runbook 同期。仕様追加に伴う evidence の requirements
  同期（UI 1..28 / mcp 1..21）と UI Traceability へ REQ-1 行追加。

### Round 55 の残り指摘（未対応）

- [x] 409 本文が非空の別本文なら winner に倒れる残存 fail-open（medium）—
  policy/UI のビルド世代ずれや中継改変で非常停止が隠れる。既知の
  already-decided 本文に完全一致した場合のみ winner へ進めるか 423 分離。〔Round 56 で対応済み〕
- [x] Round 54 の shell fail-closed 修正（egress 重複・env 復元・drift
  exit 3）に自動回帰が無い（low）— `test-legacy-guards.sh` に3ケース追加。〔Round 56 で対応済み〕

- 親が対応: fmt 差分、`BareJson` extractor の不要な `?`（3箇所）、
  REQ 集合中不一致（evidence requirements 同期＋UI spec の REQ-1 行）。

### Round 56（R4 で修正済み）

- [x] 409 本文が非空の別本文なら winner に倒れる fail-open（medium）→
  `ALREADY_DECIDED_409_BODY` 完全一致時のみ winner、それ以外の全409を
  停止側へ fail-closed。共有定数＋UI/core/policy の独立 pin。UI 67 tests。
- [x] Round 54 の shell fail-closed 修正に自動回帰が無い（low）→
  egress 重複拒否・env 復元構造ガード・drift DANGER→exit3 の3ケース追加。
- [x] specs/test-ui.md の旧分類記述を新契約（fail-closed）へ更新し、
  evidence 実測再生成（67件）。

### Round 56 の残り指摘（未対応）

- [x] シナリオ充足ゲートが skipped を「実行済み」と計上（medium）—
  `check-evidence.sh:184-186` の executed から skipped を除外し、
  near-total skip を通さない。〔Round 57 で対応済み〕
- [x] 有限性チェックが store ingress のみ（medium）— query embedding /
  reconcile / f64→f32 キャスト経路へ `validate_embedding` を適用。〔Round 57 で対応済み〕
- [ ] REQ 集合ゲートが spec ごとに最後の evidence しか検証しない（low）。
- [ ] OTel SDK ガードがより詳細な RUST_LOG ターゲット指定で上書き可能（low）。
- [ ] サイズ境界が content/source/metadata のみ（low）— payload / query /
  event_type / correlation_id にも上限を追加。
- [ ] 発掘: policy/mcp-gw/event の JsonRejection 未対応、event の生 anyhow
  ログ、UI の expires_at 境界不一致、backup-preflight の BK-4 表記、
  docs/architecture/AGENTS.md の verify.rs 欠落。

- 親が対応: fmt 差分、shellcheck の SC2329（eval 間接呼び出し）、
  spec 変更に伴う evidence 実測再生成（UI 67）。

### Round 57（R4 で修正済み）

- [x] シナリオ充足ゲートが skipped を実行済みと計上（medium）→ executed を
  passed+failed のみとし tamper 2件（skipped 水増し・同一 spec 複製証跡）追加。
- [x] 有限性チェックが store ingress のみ（medium）→ parse 後の f32 検証、
  query/pending/reconcile/search の全5経路へ validate_embedding 適用。
- [x] policy/mcp-gw/event の JsonRejection 迂回（medium）→ jarvis-core へ
  BareJson を集約し全 JSON ハンドラを置換。event の生 anyhow ログも固定語彙化。
- [x] jarvis-core/Cargo.toml に axum/tracing 追加（共有 extractor の必須依存、
  Cargo.lock も更新）。

### Round 57 の残り指摘（未対応）

- [x] kill switch probe がリダイレクトを追従（medium）— `max_redirects(0)`
  が probe agent に無く、3xx→200 で停止表示が落ちる。〔Round 58 で対応済み〕
- [x] probe が 503 以外のエラー status を「停止でない」と判定（medium）—
  502/500/504 で fail-open。200/503 以外は Err 化して unwrap_or(true) へ。〔Round 58 で対応済み〕
- [x] 期限切れ AlreadyDecided が実決定なしで「既に決定済み」表示（medium）—
  410/Expired 分離、または winner が Pending/Expired なら失敗表示。〔Round 58 で対応済み〕
- [x] skipped 正当化が "ignored" 部分文字列一致（low）、index 件数検証が
  reference-style リンクのみ（low）。〔Round 58 で対応済み〕
- [ ] mcp-gw の http_error() が `{error:#}` で anyhow チェーンを OTLP へ。
- [ ] サイズ境界が payload/query/event_type/correlation_id に未適用（low）。

- 親が対応: fmt 差分、Cargo.lock 更新（offline）、tracing field 形式依存
  アサーション削除、shellcheck SC2181（heredoc の $? 判定）。

### Round 58（R4 で修正済み）

- [x] kill switch probe の fail-open（confirmed medium 2件）→ probe agent に
  `max_redirects(0)`、200/503 以外を Err 化（502/504/3xx は停止側）、想定外
  200 本文も Err。UI 73 tests。
- [x] 期限切れ AlreadyDecided の誤表示（confirmed medium）→ `ApprovalError::Expired`
  を新設し HTTP 410 固定本文へ分離、kill switch 事前チェックを deadline より
  前に移動、UI は winner が Pending/Expired なら実状態を表示。
- [x] 証跡ゲートの部分文字列一致（low 2件）→ `N ignored` の数値照合、
  inline link 対応と passed/failed 必須化（INDEX_RESULT_MISSING）、tamper3件。

### Round 58 の残り指摘（未対応）

- [x] BareJson が axum の 413/415/422 を一律400へ丸め（medium）—
  種別ごとの空本文 status、または bare-400 統一を仕様と契約テストで明記。〔Round 59 で対応済み〕
- [x] 非有限ベクトルの poison row が上限なしで永続 defer（medium）—
  dead-letter／削除分岐と attempts 上限の導入、reconcile バッチ保護。〔Round 59 で対応済み〕
- [x] jarvis-core の BareJson が context に未適用（medium）— context も
  共有型へ切り替え、cross-crate 契約テストで実 warn レコードを比較。〔Round 59 で対応済み〕
- [x] BareJson の rejection warn が不正 JSON 高レートでログ増幅（low）。〔Round 59 で対応済み〕
- [x] jarvis-core への axum/tracing 必須依存が GPUI UI に不要な推移依存
  （low）— feature gate または薄い crate への分離、ui/Cargo.toml コメント更新。〔Round 59 で対応済み〕

- 親が対応: fmt 差分のみ。

### Round 59（R4 で修正済み）

- [x] BareJson が 413/415/422 を一律400へ丸め（medium）→ 種別別の空本文
  status（Content-Type 欠落415／2MiB超過413／型不正400）、warn→debug で
  ログ増幅も抑制。context も `jarvis_core::BareJson` へ一本化。
- [x] 非有限 poison row が上限なしで永続 defer（medium）→ 即 dead-letter
  ＋ attempts 上限10＋バッチ除外、100 poison + 1正常行の回帰追加。
  既存 `reconcile_defers_pending_row_with_non_finite_vector` は方針反転に
  伴い dead-letter 期待へ更新。
- [x] jarvis-core への axum/tracing 必須依存（low）→ opt-in `http` feature
  で optional 化、UI は既定 feature で axum 非推移、lockfile 整理。
  4サービスは workspace 依存の `features=["http"]` で継承。

### Round 59 の残り指摘（未対応）

- [x] 証跡 index の件数照合が `re.search` の最初の一致のみ（medium）—
  `finditer` へ変更し矛盾する2つ目を拒否、REF_USAGE も finditer 化。〔Round 60 で対応済み〕
- [x] HTTP 410 が本文未検証・共有 pin なし（medium）→ 410 固定本文を
  jarvis-core 共有定数化し、trim 後完全一致＋不一致時 Err（fail-closed）。〔Round 60 で対応済み〕
- [x] health probe の本文分類が非対称（medium）— 503 も `status=degraded`
  かつ reason∈{kill_switch,no_policy} 完全一致、200 は reason 不在を必須化、
  語彙を共有定数化。〔Round 60 で対応済み〕
- [x] 410／health 語彙の cross-crate pin テスト追加。〔Round 60 で対応済み〕

- 親が対応: fmt 差分、`features=["http"]` の workspace 配線（blocker）。

### Round 60（R4 で修正済み）

- [x] 証跡 index の件数照合が最初の一致のみ（medium）→ `finditer` で全一致
  を個別照合、複数参照リンクも全件解決。矛盾値の tamper 2件追加。
- [x] HTTP 410 本文が未検証・ローカル定数（medium）→ jarvis-core 共有定数
  `APPROVAL_EXPIRED_BODY`、trim 後完全一致、不一致時 Err fail-closed。
  core/policy/UI の3箇所 pin。
- [x] health probe の本文分類が非対称（medium）→ 200 は status=ok かつ
  reason 不在、503 は status=degraded かつ reason 完全一致、他は Err。
  status/reason 語彙を共有定数化し cross-crate pin。UI 79 tests。
- [x] context/event の jarvis-core 依存へ `features=["http"]` を配線
  （単体 `-p` ビルドの blocker を解消）。

### Round 60 の残り指摘（未対応）

- [x] dead-letter／attempts 上限がログのみで永続状態・観測が無い（medium）—
  `dead_lettered_at`／`vector_state` カラムとメトリクス、migration 004 の
  選択クエリ説明を実状へ同期。〔Round 61 で対応済み〕
- [x] poison 判定が再埋め込みより先行し、回復可能な旧次元行を削除
  （medium）— dimension 不一致は retry 経路に残す。〔Round 61 で対応済み〕
- [x] BareJson の 413/415 再分類が仕様・証跡に未反映（medium）—
  4仕様の該当 TC へ契約明記＋evidence 再生成。〔Round 61 で対応済み〕
- [x] BytesRejection を無条件413へ丸め（low）— `LengthLimitError` のみ413、
  他は400（axum 準拠）。〔Round 62 で対応済み〕

- 親が対応: fmt 差分、context/event の feature 配線（Runtime Proof で発見）。

### Round 61（R4 で修正済み）

- [x] dead-letter／attempts 上限がログのみ（medium）→ 永続カラム
  （migration 005）＋メトリクス＋選択クエリの3ゲート。
- [x] poison 判定が再埋め込みより先行（medium）→ 旧次元は1回再埋め込みで
  回復、非有限かつ次元一致のみ dead-letter。回帰3件。
- [x] BareJson の 413/415 が仕様・証跡に未反映（medium）→ 4仕様へ明記、
  実測で証跡再生成（context 156 / event 76 / mcp 125 / policy 205）。
- [x] migration 004 のチェックサム変更リスクを回避: 列追加を新規 005 へ
  分離し 004 は元の内容へ復元。

### Round 61 の残り指摘（未対応）

- [x] **証跡 index の参照リンクを外すだけで件数照合がスキップ**
  （confirmed high、Runtime Proof 付き）— evidence_entry 判定を条目の必須
  属性とし、JSON 解決不能な条目を fail-closed 化。双方向照合も追加。〔Round 62 で対応済み〕
- [x] REQ 集合ゲートが glob 外の spec_path を無検査で通す（medium）—
  未一致 entry を `REQ_SPEC_UNRESOLVED` で拒否、specs/ 内へ正規化。〔Round 62 で対応済み〕
- [x] health 応答の重複 JSON キーで kill_switch→no_policy に collapse
  （medium）— 重複キー検出または固定3形状との生本文完全一致。〔Round 62 で対応済み〕
- [x] index 条目の spec リンクと JSON の spec_path が無相関（medium）—
  .md リンクの basename 一致を `INDEX_SPEC_MISMATCH` で検証。〔Round 62 で対応済み〕
- [x] BytesRejection を無条件413へ丸め、health の exact body match 契約
  記述と実装の不一致（low 2件）。〔Round 62/64 で対応済み〕

- 親が対応: fmt 差分、migration 005 分離、spec 変更に伴う証跡再生成、
  secret_scan の未ステージファイル対象エラー。

### Round 62（R4 で修正済み）

- [x] **証跡 index の参照リンク除去で件数照合がスキップ**（confirmed high、
  Runtime Proof 付き）→ 条目を構造的に evidence として扱い、JSON 解決不能を
  `INDEX_UNRESOLVED_ENTRY` で fail-closed。glob 外 spec_path の
  `REQ_SPEC_UNRESOLVED`、index の .md リンクと JSON の spec_path 照合
  `INDEX_SPEC_MISMATCH` も追加、tamper7件。
- [x] health 応答の重複キー／余分キー（confirmed medium/low）→ policy の
  3形状を jarvis-core 固定文字列定数化、UI は trim 後完全一致のみ分類、
  他は Err→停止側。cross-crate pin 更新。UI 82 tests。
- [x] BytesRejection の無条件413（low）→ `LengthLimitError` のみ413、
  他は axum 準拠の400。

### Round 62 の残り指摘（未対応）

- [x] migration 005 の死信保持に振る舞いテストが無く、既存テストは逆の
  「削除」を期待（confirmed high）— Pg の dead_letter が行を保持し後続 list
  が返さないことを実 DB／永続モックで検証、文字列 contains を置換。〔Round 63 で対応済み〕
- [x] dead-letter カウンタが DB 書き込み成功前に increment（medium）—
  成功後に移動、失敗カウンタを分離。〔Round 63 で対応済み〕
- [x] attempts 上限到達行が終端マーカーもメトリクスも無くサイレント除外
  （medium）— vector_state='exhausted' 永続化＋専用メトリクス。〔Round 63 で対応済み〕
- [x] シナリオ充足判定が下限のみ（medium）— 契約 step 追記時に新規 Scenario
  を要求、またはシナリオ別 pass 数を記録。〔Round 63 で対応済み〕
- [x] migration pre-flight unit コメントが「through 004」のまま（low）。〔Round 63 で対応済み〕
- [ ] `overflow_warning_records_event_metadata_without_payload` が並列実行時
  にフレーキ（tracing ログキャプチャ競合）— 再実行で pass、構造修正は未実施。

- 親が対応: fmt 差分、I64-A pristine 一時ツリーへの evidence/index.md
  コピー漏れ（safety-test fail）、event ログキャプチャのフレーキ再実行。

### Round 63（R4 で修正済み）

- [x] migration 005 の死信保持に振る舞いテストが無い（confirmed high）→
  永続化ミラーのモックで「行が残り list が除外」を検証、既存の削除期待を
  保持期待へ更新、Pg UPDATE SQL の静的ガード追加。
- [x] dead-letter カウンタが永続化前に increment（medium）→ 成功後に移動、
  失敗専用カウンタを分離。
- [x] attempts 上限到達行がサイレント除外（medium）→ `vector_state='exhausted'`
  永続化、ゲージを pending/exhausted に分割、backfill 追加。
- [x] シナリオ step カバレッジ（`SCENARIO_STEP_UNCOVERED`）と migration unit
  コメント through 005 を追加。

### Round 63 の残り指摘（未対応）

- [ ] Round 61 の4証跡の notes に BareJson／415／413／no-leak キーワードが
  未同期（medium）— 現在 `BASELINE_CONTRACT_STEPS` で grandfather 中。
  notes へ実測キーワードを追記し baseline を削除して strict 化する。
- [ ] 4サービスの `status_label` が 413/415 を "other" に丸め（medium）—
  Round 62 の 400/413 分離がメトリクスで観測不能。ラベル追加＋cross-crate
  回帰。
- [ ] UnknownBodyError→400 が4仕様・証跡に未反映（low）— 3種→4種へ修正。
- [ ] /health の `.trim()` が「完全バイト一致」契約と矛盾（low）— trim を
  外すか doc／仕様を trim 許容へ揃える。

- 親が対応: fmt 差分、clippy `collapsible_if`（let-chain 化）、shellcheck
  SC2016（SQL プレースホルダのリテラル一致）。

### Round 64（R4 で修正済み）

- [x] 4証跡の notes に契約キーワードを同期し `BASELINE_CONTRACT_STEPS` を
  削除して strict 化。tamper を notes キーワード除去方式へ更新。
- [x] 4サービスの status_label に 413/415 を追加し "other" 丸めを解消、
  行動テスト4件追加。
- [x] BareJson 契約を「3種」→「4種」（UnknownBodyError→400）へ訂正、
  /health の `.trim()` を外し完全バイト一致へ fail-closed 強化。
  前後空白の回帰追加、UI 83 tests。
- [x] 5仕様の変更に伴い証跡を実測再生成（context 162 / event 77 /
  mcp 126 / policy 207 / UI 83）、index 同期。

### Round 64 の残り指摘（未対応）

- [ ] todo.md:1858 付近の旧 Round 63 残指摘（grandfather 記述、413/415
      メトリクス、UnknownBodyError、trim 契約）はすべて本ラウンドで解消済み。
      次回レビューで重複排除すること。
- 親が対応: spec_hash drift の再生成、tamper が古い証跡ファイル名を参照
  していた問題、notes/index の件数表記同期。

## アーカイブ判定（2026-11-20 / I67-A）

- 判定方針: 各 `- [ ]` について後続ラウンドの「R4 で修正済み」節および
  本文の「対応済み」記述を照合し、明示的に解消が記録された項目のみを
  `- [x]` へ変更した。行自体は履歴として保持し、行末に解消ラウンドを
  追記（削除なし・見出し／OKF は未変更）。
- アーカイブ判定: 91件（Round 42〜62 の「残り指摘」のうち Round 43〜64
  で解消が明記された重複エントリ）。残り `- [ ]` は 44件。
- 保持区分: ユーザー判断（P0-1/3/6/7、P0-10/11、P1-4/6/12、P2-2、
  P3-4/5、Grafana／Webhook／bws.env／pprof・zpages／バックアップ外部化）、
  実機操作（P1-5、S9、timer unit 再適用・実復元演習）、最新指摘
  （Round 63/64 の残り）。
- 一部解消のため `[ ]` を維持（誤判定防止）: commit_gate 新契約の
  回帰名未反映、rustfmt ゲート CI 未配線を含む発掘5件、persistence 構造
  ガード、新規コレクション読み戻し一時障害、local-restore 等ドキュメント
  ドリフト、UI/mcp-gw 証跡の一部、REQ 集合ゲートの per-spec 検証、
  OTel SDK ガードの上書き可能性、サイズ境界の未対象項目、mcp-gw の
  anyhow エコー、フレーキテストの構造修正 — いずれも後続ラウンドで
  「全項目解消」が明記されていない。

### Round 65（R4 で修正済み）

- [x] todo.md の解消済み旧指摘91件を `[x]` 化（履歴は保持、open 135→44）。
- [x] pre-commit に `make secret-scan` を追加（CI を最終ゲートとする二段構成）。
- [x] UI clippy に `-D clippy::undocumented_unsafe_blocks` を付与、
  ci.yml の clippy_args へ `-D warnings`。
- [x] 契約 step の Counter を両側で比較し、重複 step も fail-closed 化。
- [x] Grafana を service/status 集計へ（413/415 を可視化）、README／
  emit doc／Runbook 件数／Cedar スキーマの古い記述を現行実装へ同期。

### Round 65 の残り指摘（未対応）

- [ ] **restore-local の config allowlist 検証が本番 DB/Qdrant 上書き後**
  （confirmed high）— `configs.sh:62` の検査を `verify_set()` の必須ゲート
  （stop_services より前）へ移動。`--skip-configs` 時も通过させる。
- [x] verify-backup.sh の legacy 経路がペイロード0件を受理（confirmed medium / I69-B）—
  2026-11-20 対応済み: 必須ペイロード実在検査を COMPLETE marker の有無に
  かかわらず全セットへ適用し、空／ヘッダのみの manifest も
  `manifest has no file entries` で拒否。既存 legacy 合格フィクスチャは
  postgres/qdrant/config-* の完全ペイロード付きへ変更し期待値名を実態に
  合わせ、新規2ケース（ヘッダ無し・ペイロード無し／自明ファイルのみ）で
  exit 1 を追加。値・パス非出力、実バックアップ先は非接触。検証は親が実施
  （回帰は 23→25 件、証跡の passed 29→31 は親が再生成）。
- [ ] 隔離 DB `jarvis_restore_test` が後始末されず2回目の復元が不能
  （medium）— Qdrant と対称に DROP、失敗経路も trap で片付け。
- [ ] migration 範囲コメントが jarvis-policy/context service で through 004
  のまま（low）、build-selfhost.sh のガードコメントも未更新。
- [ ] pending_embeddings ゲージが dead_letter / exhausted / remove 後に
  更新されず未処理件数を過大報告（low）。
- [ ] 証跡の passed 件数が自己申告の手入力（medium）— `cargo test -- --list`
  または JUnit 出力から機械導出する生成器の導入。

- 親が対応: fmt 差分、JSON/YAML 構文確認、全ゲート再検証。

### Round 66（R4 で修正済み）

- [x] **restore-local の config allowlist 検証が本番 DB/Qdrant 上書き後**
  （confirmed high）→ 検査を `verify_set()` の必須ゲート（stop_services より前）
  へ移動、`--skip-configs` も通过不可。`configs.sh` は共有helper に統一。
- [x] verify-backup.sh の legacy fail-open（medium）→ 必須ペイロード検査の
  COMPLETE ゲート撤廃、空 manifest 拒否、回帰2件追加（23→25）。
- [x] 隔離 DB の後始末（medium）→ 検証後の DROP＋EXIT trap、
  `--keep-isolated-db` オプション、設計書同期。
- [x] 証跡 generator（`ci/scripts/generate-evidence.sh`）追加、
  outbox gauge を terminal 遷移後に再集計（回帰4件）。
- [x] 証跡の実測件数を同期（context 162→166、backup 29→31）。

### Round 66 の残り指摘（未対応）

- [ ] restore-local の非 allowlist config 回帰が `test-restore-local.sh` に
  未追加（子の担当外）— `verify_set()` ゲートで拒否され DB が触られない
  ケースを `--apply --confirm` と `--skip-configs` の両方で検証。
- [ ] migration 範囲コメントが jarvis-policy/context service で through 004
  のまま（low）、build-selfhost.sh のガードコメントも未更新。
- 親が対応: test-restore-local.sh の DROP アサートを本番 DB 宛に限定、
  証跡の実測件数同期。

### Round 67（R4 で修正済み）

- [x] restore-local の非 allowlist config 回帰を `test-restore-local.sh` へ
  追加（`--apply --confirm` と `--skip-configs` の両方で DB/Qdrant/サービス
  未接触を検証、48→60件）。
- [x] migration 範囲コメントを policy/context unit・build-selfhost.sh・
  Runbook の4箇所へ through 005 反映、同一コミット更新ルール明記。

### Round 67 の残り指摘（未対応）

- [ ] **restore-local が jarvis-backup.timer を停止せず本番 DB 上書きと
  日次バックアップが競合**（confirmed high）— SERVICE_INVENTORY へ timer
  追加＋apply 前 stop／on_exit 復元、runbook へ必須ステップ追加。
- [ ] **setup-selfhost.sh の `chown -R` が root:root 配置のバックアップ
  スクリプトを jarvis 所有へ劣化**（confirmed high）— chown 対象をデータ系
  ディレクトリへ限定、bin/scripts は root 所有を冪等維持。
- [ ] **smoke-mcp-gw.sh の inputSchema 検証が旧契約を固定**（high）—
  filesystem.read は path が optional（既定 '.'）、required 無しを検証する
  形へ更新。現行のままでは deploy-mcp-gw-safe.sh のスモークが失敗し
  デプロイが中止される。
- [ ] generate-evidence.sh が failed=0 を固定出力（medium）— 実測 run から
  failed を導出するモード追加、または --list 断片を「予測値」と明示。
- [ ] restore_configs が owner/mode を明示せず root:root 0600 方針から
  漂移（medium）— allowlist6対象の期待 owner/mode を単一表で定義し
  install で是正。
- [ ] P1-6 暗号化導入時に verify/restore のペイロード名・形式ハードコード
  がゲートを停止（medium）— manifest に format/encryption/signature 記述子。
- [ ] dsh.md / remaining-tasks.md の firejail・Cedar informational・
  build-selfhost.sh:47 参照などの古い記述（low 4件）。

- 親が対応: 全ゲート再検証（restore 60件 pass）。

### Round 68（R4 で修正済み）

- [x] **backup timer が復元中の本番 DB と競合**（confirmed high）→
  SERVICE_INVENTORY へ jarvis-backup.timer 追加、apply 前 stop／on_exit
  復元、runbook へ必須ステップ追加。
- [x] **setup-selfhost.sh の chown -R が root 所有を劣化**（confirmed high）
  → データ系ディレクトリへ限定、bin/scripts は毎回 root:root を再適用。
  併せて build-selfhost.sh:402 の install も sudo root 所有化。
- [x] **smoke-mcp-gw.sh が旧 schema 契約を固定**（high）→ filesystem.read は
  optional path（既定 '.'）＋required 無しを検証。デプロイ中止を解消。
- [x] generate-evidence.sh の failed=0 を予測値と明示し --check で実 run を
  要求。dsh.md / remaining-tasks.md の古い記述4件を現行実装へ同期。

### Round 68 の残り指摘（未対応）

- [ ] P1-6 暗号化導入時に verify/restore のペイロード名・形式ハードコードが
  ゲートを停止（medium）— manifest に format/encryption/signature 記述子。
  過去のラウンドから継続、ユーザー判断（暗号化鍵の注入先）を含む。
- [ ] restore_configs の owner/mode 方針漂移（medium）— allowlist6対象の
  期待 owner/mode を単一表で定義し install で是正。
- 親が対応: build-selfhost.sh:402 の sudo install root 所有化（chown 修正の
  残余リスク）、全ゲート再検証。

### Round 69（R4 で修正済み）

- [x] restore_configs の owner/mode 漂移（medium）→ 6設定の期待 owner/mode
  単一表＋install 経路へ統一、build-selfhost.sh も同表を source。
- [x] バックアップ manifest に format/encryption/signature/role 記述子を
  追加（既定 none/none で後方互換）、verify/restore を記述子駆動の単一
  経路へ統一。未知記述子は fail-closed。spec にシナリオ追加。
- [x] spec_hash 変更に伴う backup 証跡再生成（31件）。

### Round 69 の残り指摘（未対応）

- [ ] **timer が active だが disabled だと停止されない**（confirmed high）—
  `inventory_services()` の is-enabled && is-active 前提。timer は is-active
  のみで停止対象にし、停止後 inactive を確認。
- [ ] **復元実行ユーザー前提が postgres(0700 backup_dir) と systemctl(root)
  で両立しない**（confirmed high）— 実行ユーザーを固定し precondition
  チェック、runbook の3コマンドを権限付き形へ統一。
- [ ] Persistent=true の timer を失敗経路で無条件 restart すると、壊れた DB の
  COMPLETE バックアップが作られる（medium）— 失敗時は timer を停止維持。
- [ ] jarvis-backup.service の ExecStartPost が repo 作業コピー（711 koppe）
  を参照し postgres から読めない（medium）— /opt の root:root コピーへ変更。
- [ ] restore-local.d/{pg,qdrant,git,configs}.sh が依然固定名（medium）—
  記述子から入力名を導出（P1-6 実施時）。
- [ ] drift の casbin_policy.csv が JARVIS_DRIFT_POLICY_DIR を無視（low）、
  web.fetch の requires_approval 検査が grep -A8 依存（low）。
- [ ] dsh.md の「firejail 全面撤去」は過大主張（low）— 実行 fallback のみ撤去、
  設定値とパッケージは互換のため残存。

- 親が対応: install の sudo 不在フォールバック、`declare -gA`（source 先の
  関数スコープ回避）、shellcheck SC1091、tamper の notes キーワード保持、
  backup 証跡再生成。

### Round 70（R4 で修正済み）

- [x] **timer が active だが disabled だと停止されない**（confirmed high）→
  timer は is-active のみで停止対象、stop 後 inactive を確認し dispatch 前に
  fail-closed。状態付き systemctl モックと2ケース追加（restore 80件）。
- [x] **復元の実行ユーザー前提が両立しない**（confirmed high）→ root 固定、
  3種 precondition（backup_dir/psql/systemctl）fail-closed、psql を
  `sudo -u postgres` ラッパへ集約、runbook の3コマンドを `sudo bash` 化。
- [x] Persistent timer の失敗時再開を停止維持＋固定 warn、ExecStartPost を
  /opt の root:root verifier へ変更、配置を手動起動より前に前倒し。

### Round 70 の残り指摘（未対応）

- [ ] Round 69 の新規仕様シナリオ4件（TC-B6 / TC-V5 / TC-R1 / TC-R2）に
  自動テストが無く証跡がカバレッジを虚偽主張（medium）— 7列 manifest／
  未知記述子／role 導出の回帰追加＋シナリオ別 pass のゲート化。
- [ ] 必須ペイロード検査が manifest 自己申告 role へ後退（medium）—
  Runtime Proof: postgres 欠落／truncated が format=raw role=postgres で
  exit 0。role と basename の整合強制＋footer 検査を role に紐付け。
- [ ] owner/mode 表が deploy-mcp-gw-safe.sh 経路を覆っていない（medium）—
  3経路すべてが共有表を参照する形へ。
- [ ] install_expected_config の sudo 不在フォールバックが root:root 不変
  条件を黙って破る（medium）— fail-closed 化＋owner も検証。
- [ ] 記述子ロジックが verify-backup.sh と restore-local.sh に重複し既に
  分岐（low）— 共有 lib へ抽出し両スクリプトで source。

- 親が対応: 3ワークストリームの restore-local.sh 並行編集を統合、
  全ゲート再検証（restore 80件 pass）。

### Round 71（R4 で修正済み）

- [x] Round 69 の新規シナリオ4件に自動テストを追加（verify 25→37、
  restore 80→105）。7列記述子受理・未知記述子 fail-closed・role 駆動判定を実測。
- [x] 必須ペイロード検査を manifest 自己申告から実 basename 由来へ復元、
  role/basename 整合と pg_dump footer 常時検査（truncated dump 拒否）。
- [x] owner/mode 表を3経路（build / restore / deploy）へ統一、
  sudo 不在時は opt-in なければ fail-closed。静的ガード8件追加。
- [x] 記述子ロジックを `infra/scripts/lib/payload-descriptors.sh` へ抽出、
  verify/restore 両方が source。installer／Makefile も lib を配布。
- [x] 証跡を実測再生成（backup 31→43）。

### Round 71 の残り指摘（未対応）

- [ ] 共有 lib の配布先（`install-backup-timer.sh` と `Makefile`）は追加したが、
      既に configured 済みの実機へ反映するには再インストールが必要（ユーザー/
      実機運用）。
- 親が対応: TC-R1（role 権威）と I74-B（basename 権威）の矛盾を `*.sql.gz`
  一般受理で解消、shellcheck SC1091／SC2016／SC2123 の抑止、
  証跡の実測件数同期。

### Round 72（R4 で修正済み）

- [x] drift の casbin パスが JARVIS_DRIFT_POLICY_DIR を無視（low）→ 環境変数
  経路へ統一。web.fetch の requires_approval 検査を tomllib 厳密パースへ
  変更（grep -A8 の行距離依存を解消）、回帰2件。
- [x] restore-local.d/*.sh の固定名参照を共有 `resolve_restore_input()`
  による記述子導出へ変更（legacy は後方互換）。改名ペイロード復元の
  回帰追加（restore 105→119件）。
- [x] UTF-8 バイト境界テスト3件、AllowList の空名／空 principal 起動時拒否。
- [x] dsh.md の firejail「全面撤去」を範囲限定、backup-redesign-notes.md
  の legacy 許容、remaining-tasks.md の解消済み記述を現行へ同期。

### Round 72 の残り指摘（未対応）

- [ ] restore-local の module 内 fail-closed は dispatch 後（stop_services 後）
  に発動。verify_set 前に解決ゲートを組み込んで stop 前に拒否したい場合は
  `resolve_restore_input()` を verify_set() から呼ぶ必要がある（low）。
- [ ] 共有 lib の実機配布（再インストール）と、ユーザー判断項目:
  P1-6 暗号化鍵注入先、P0 ガード有効化、Grafana、Webhook、S9、
  バックアップ外部化、push。

- 親が対応: fmt 差分、AllowList の `#[derive(Debug)]`（expect_err 要件）、
  全ゲート再検証。

### Round 73（R4 で修正済み）

- [x] verify_set() に stop_services 前の `resolve_restore_input()` 事前解決
  ゲートを追加（未知 role／複数一致／解決先不在を stop 前に拒否）。
  回帰3ケース追加、restore 119→155件。
- 親が対応: 全ゲート再検証。

### Round 73 の残り指摘（未対応）

- [ ] 共有 lib の role 解決と basename 判定が不一致（medium）— role=other の
  config-* が検証は通るが restore_configs が黙ってスキップ。role 解決を
  共有関数へ集約し、矛盾行を検証時に fail-closed。
- [ ] payload_format_check がグローバル hook 経由のみで既定 no-op（low）—
  上書き忘れで破損ペイロードが無視される fail-open。hook 装填 assert または
  戻り値での判定伝播。
- [ ] root 固定の restore が git アーカイブを root:root で展開し koppe 所有
  repo を編集不能にする（medium）— --no-same-owner／対象所有者での展開。
- [ ] curl/psql/jq モックが常に成功し fail-closed 分岐が未検証（medium）—
  失敗注入モックを追加。
- [ ] MOCK_LOG が本番の postgres precondition と psql ラッパを迂回（low）—
  専用名 `JARVIS_RESTORE_FIXTURE_MODE` へ変更し本番で拒否。
- [ ] 4 crate の env キー→フィールド写像テストが Serialized 上書きで
  形骸化（medium）— 本番 ENV_PREFIX 定数を参照する contract テストへ。
- [ ] infra/README.md の MCP Gateway 節が旧挙動（承認ツール非表示、
  upstream メモリのみ）のまま（medium）。
- [ ] check-evidence のファイル名ハッシュ照合が空サフィックスでスキップ
  （low）— fail-closed 化＋tamper ケース。
- [ ] mcp_allowlist.toml の shell.execute timeout 参照先コメントが誤り
  （low）。
- ユーザー判断: P1-6、P0 ガード、Grafana、Webhook、S9、バックアップ外部化、
  push。共有 lib の実機配布（再インストール）。

### Round 74（R4 で修正済み）

- [x] role 解決を共有 `resolved_role_for()` へ集約（verify_set と resolver が
  同じ role を使用）、role と basename の矛盾／未知 role／absent を fail-closed。
- [x] 共有 lib の hook を必須登録化、判定を戻り値で伝播（rc 0/1/2）、
  未登録 hook は rc=2 で拒否。
- [x] git 展開を `--no-same-owner`＋repo 既存 owner への chown に変更。
- [x] curl/psql/jq の失敗注入モックを追加し fail-closed 分岐を実測
  （restore 119→681件、safety 93→655件）。
- 親が対応: 全ゲート再検証。

### Round 74 の残り指摘（未対応）

- [ ] verify-backup.sh が `resolved_role_for()` をまだ使っていない
  （担当外だった）— 共有 lib のロール解決を3スクリプトすべてで統一。
- [ ] git.sh:33-36 の resolver rc=2 が警告付き成功スキップ（pre-stop
  ゲートが先に失敗するため通常フローでは到達不能だが防御的改善の余地）。
- ユーザー判断項目は従前どおり（P1-6／P0 ガード／Grafana／Webhook／S9／
  バックアップ外部化／push／共有 lib の実機配布）。

### Round 75（R4 で修正済み）

- [x] verify-backup.sh の role 判定を共有 `resolved_role_for()` へ統一、
  未知 role／basename 矛盾を fail-closed。回帰10件（verify 37→47）。
- [x] git.sh の resolver rc=1/2 を fail-closed 化（rc=0 のみ続行）、
  隔離プローブで module 内の防御的拒否を実測（restore 681→690件）。

### Round 75 の残り指摘（未対応）

- [ ] 共有 lib が「戻り値で verdict を強制できる」と主張しつつ3呼び出し側が
  rc=1 を無視（medium）— `format_rc -ge 1` で fail 加算、hook は記録専用に。
- [ ] 設計書が必須とする flock 同時実行防止が restore-local.sh に未実装
  （medium）— 実装するか設計書から要件を削除。
- [ ] 設計書の「スキップ指定は事前検証から除外しない」が実装と正反対
  （low）— 設計書を CLI help のセマンティクスへ更新。
- [ ] 681件中約270件が同一の本番未接触アサーションで、ログ不在時に
  無条件 pass（low）— 代表ケースへ集約し前提ログを必須化。
- [ ] jq モックの empty／prod_empty モードが未使用（low）— case '' 分岐の
  回帰を追加。
- [ ] secret_scan.py が追跡ファイル0件で fail-open（medium）— 空集合を
  exit 1＋回帰追加。
- [ ] remaining-tasks.md の PASSWORD_FILE 節（行数ズレ＋「未修正」主張）、
  local-restore-design.md の「自動スクリプト未実装」現在形（low 2件）。
- [ ] test-review-run.sh の「default backend」テストが既定経路を通っていない
  （low）— opencode スタブで既定分岐を実検証。

- 親が対応: I78-A で verifier も role 矛盾を拒否するようになり、I77-A の
  「verifier は受理」期待値を更新、全ゲート再検証。

### Round 76（R4 で修正済み）

- [x] 共有 lib の verdict 契約を実装へ整合: 両呼び出し側で `format_rc -ge 1`
  を失敗加算、hook はログ専用（二重カウントを構造的に排除）。
- [x] restore-local.sh に `flock -n` 排他ロックを追加（同時実行を fail-closed
  で拒否）、設計書のスキップセマンティクスを CLI help と一致。
- [x] secret_scan.py が追跡ファイル0件で fail-open → exit 1、空 repo 回帰追加。
- [x] ドキュメント3箇所（PASSWORD_FILE 参照／自動スクリプト未実装）を現行
  実装へ同期、test-review-run.sh に既定 opencode 分岐の実検証を追加。

### Round 76 の残り指摘（未対応）

- [ ] 681件中約270件が同一の本番未接触アサーションで、ログ不在時に
  無条件 pass（low）— 代表ケースへ集約し前提ログを必須化。
- [ ] jq モックの empty／prod_empty モードが未使用（low）— qdrant.sh の
  `case ''` 分岐の回帰を追加。
- ユーザー判断: P1-6 暗号化鍵、P0 ガード、Grafana、Webhook、S9、
  バックアップ外部化、push、共有 lib の実機配布（再インストール）。

- 親が対応: 全ゲート再検証（restore 690件、safety 664件）。

### Round 77（R4 で修正済み）

- [x] テストの形骸化を解消: `assert_absent` の空ログ pass を前提ログ必須化
  で排除、同一アサーションの反復を代表ケースへ集約（restore 690→354件、
  失敗モードごとの fail-closed は維持）。
- [x] jq モックの empty／prod_empty 回帰を追加（qdrant.sh の `case ''` 分岐）。
- [x] `dead_letter_warns_once_with_fixed_vocabulary` の3回連続フレーキを
  `capture_warns` の静的 Mutex 直列化で解消。
- 親が対応: 全ゲート再検証。

### Round 77 の残り指摘（未対応）

- [ ] flock のコメントが「backup timer とのインリーブを防ぐ」と主張するが
  バックアップ側はロックを取得しない（medium）— コメント訂正または両側取得。
- [ ] flock fd がサブプロセスへ継承され子が残ると stale ロックが残る
  （medium）— dispatch 前に fd を閉じる／PID 記録。
- [ ] /tmp の予測可能ロックファイルの inode 変更で相互排除が破られる
  （medium）— /run/jarvis 等の安定パス＋stat 照合。
- [ ] カウンタ単一化でサマリの意味が「イベント数」→「呼び出し数」へ変化
  （low）— runbook 明記＋サマリ値の回帰。
- [ ] secret_scan が部分走査（サブディレクトリ実行）を検出できず OK になる
  （medium）— repo root から git -C で走査対象を固定。
- [ ] egress allowlist のテストが本番と逆の「空=無制限」を固定（medium）—
  from_config 経由に統一。
- [ ] pg_url doc が「Slice 1 のみ・接続なし」と古い（low）。
- [ ] event の ingress トークン OnceLock テストが順序依存（low）。

- ユーザー判断: P1-6、P0 ガード、Grafana、Webhook、S9、バックアップ外部化、
  push、共有 lib の実機配布。

### Round 78（R4 で修正済み）

- [x] flock を堅牢化: スコープ記述を訂正（timer 除外は stop/verify が担任）、
  dispatch 前に fd を閉じる、PID／取得時刻を記録、/run 安定パス＋inode 照合。
- [x] secret_scan.py を repo root 固定の git ls-files へ変更、CWD 非依存と
  index 件数クロスチェック、サブディレクトリ CWD 回帰3件。
- [x] egress allowlist テストを本番 from_config() 経路へ統一、空入力が
  DenyAll になる fail-closed を期待へ反転。
- [x] pg_url doc・event OnceLock 順序依存・verify サマリ単位の Runbook
  明記を同期（event 77→78 テスト、証跡更新）。

### Round 78 の残り指摘（未対応）

- [ ] flock を dispatch 前に閉じるため、dispatch 中の他インスタンス排除は
  このロックでは担保されない（トレードオフを子が明記）。厳密な相互排除が
  必要なら別 fd で再取得する別タスク。
- [ ] stat 照合は TOCTOU を完全に排除するものではない（一次防御は /run 権限、
  設計書に明記済み）。
- ユーザー判断: P1-6、P0 ガード、Grafana、Webhook、S9、バックアップ外部化、
  push、共有 lib の実機配布。

- 親が対応: shellcheck SC2174／SC2261、event 証跡の件数同期、全ゲート検証。

### Round 79（R4 で修正済み）

- [x] UI cargo コマンドへ `--locked`、workspace の jarvis-core http feature を
  利用側4クレートへ明示化（新規 crate への推移を防止）。
- [x] `--backup-dir` 値欠落ガード（verify 48件／backup-args 7件）、smoke の
  EXIT trap pid 初期化と mode 検証、op:// 記述を現行へ訂正。
- [x] gRPC の serde_json エラー本文を固定語彙へ、apply_minimal_env の実効
  テストと spawn 静的ガード、UI の同一 id 二重決定を防止（UI 84件）。

### Round 79 の残り指摘（未対応）

- [ ] CI の外部アクション／再利用ワークフローが可変タグ参照（low）—
  checkout を SHA 固定、rust-gates.yml@v1 の解決 SHA を記録。
- [ ] qdrant-client の ^1.10 が reqwest 0.12/0.13 の major 重複を招く
  （low）— exact pin か共存理由の記録。
- [ ] rust-script の依存が semver 下限のみでロック無し（medium）—
  exact pin または Makefile + rust-script 例外の明記。
- [ ] verify サマリ「ok=43」の単位誤記（medium）— 証跡のテスト件数と
  verify の判定呼び出し数を区別して表記。
- [ ] テスト集約で失敗時の本番非接触検証が代表1ケースのみに（medium）—
  失敗クラスごとに1件ずつ復活。
- [ ] test-restore-local が既定ロックパスを使用し /run 経路が CI で未検証
  （low）— LOCK_FILE を一時パスへ。
- [ ] secret_scan の index 件数クロスチェックが自己和集（low）—
  主張を限定または独立検証へ置換。
- [ ] todo.md:591 の egress「未設定時は無制限」が現行と逆（low）。
- ユーザー判断: P1-6、P0 ガード、Grafana、Webhook、S9、バックアップ外部化、
  push、共有 lib の実機配布。

- 親が対応: fmt 差分、全ゲート再検証。

### Round 80（R4 で修正済み）

- [x] CI の checkout をフル SHA 固定、rust-gates.yml@v1 の解決 SHA を記録。
- [x] qdrant-client を =1.19.0 exact pin、reqwest major 重複の共存理由を
  Cargo.toml に記録。rust-script の exact pin 方針を Makefile へ記録。
- [x] verify サマリ ok=43 の単位誤記を訂正（証跡のテスト関数数と区別）、
  todo.md:591 の egress 記述に「現行は DenyAll」訂正追記。
- [x] 失敗注入ケースに失敗クラス別の本番境界チェックを復活（restore
  354→437件）。test-restore-local の LOCK_FILE 隔離と両経路回帰。
- [x] secret_scan の index クロスチェック主張を「実装変更への回帰ガード」
  へ限定し、一次防御は repo_root + -C root と明記。

### Round 80 の残り指摘（未対応）

- [ ] `ci/review-gate.yml`（未配線の reference）に actions/checkout@v4 が
      4箇所残存 — 未配線を理由に明示除外するか別タスクで SHA 固定。
- [ ] reqwest 0.13 統一は cargo check/test を伴う別タスク（現状は exact pin
      で解決版を固定、共存理由を記録済み）。
- [ ] rust-script 各スクリプトのメタブロックを exact pin 化し、rust-script
      自体のバージョンも固定（Makefile には方針記録済み）。
- [ ] prod_db scope の同一モジュール内後続 psql は未検出（low・任意）。
- ユーザー判断: P1-6、P0 ガード、Grafana、Webhook、S9、バックアップ外部化、
  push、共有 lib の実機配布。

- 親が対応: restore コピードライバへの module 複製、shellcheck SC2016、
  全ゲート再検証。

### Round 81（R4 で修正済み）

- [x] `ci/review-gate.yml` の checkout@v4 4箇所をフル SHA 固定（v4 併記）、
  未配線 reference である旨を先頭コメントへ明記。
- [x] reqwest 0.13 統一は見送り（rmcp/testcontainers が 0.12 を固定しており
  ワークスペース変更では消えない）。qdrant-client exact pin と共存記録を維持し、
  otel 由来も含む重複発生源を記録へ補正。
- [x] rust-script 4スクリプトのメタ依存9件を rust/Cargo.lock 根拠で exact pin。
  clap/hostname はリポジトリ内に根拠が無いため保留理由を記録。

### Round 81 の残り指摘（未対応）

- [ ] **PII パターン設定が PiiConfig スキーマと不一致**（confirmed high）—
  `pii_patterns.toml` がトップレベルキーで14パターンを定義するが
  `patterns: HashMap` を要求するパースに失敗し既定6パターンのみが有効。
  `[patterns]` 配下へ移動＋実設定のパース回帰を追加。
- [ ] 証跡 generator／UI README／Makefile:test に --locked が無い（medium）。
- [ ] policy/event の起動検証テストが env 存在で早期 return し vacuous pass
  （medium）— Serialized で明示値をマージして常にアサート。
- [ ] egress ホスト長 253/254・ラベル長 63/64 の境界回帰が無い（low）。
- [ ] ContextConfig の Debug が pg_url を redact せず userinfo がダンプされる
  （medium）。
- [ ] UI の二重 POST 防止テストが実 decide() を通らず空振り（medium）—
  in-flight 解放の全経路を回帰で担保。
- [ ] root の /run ロック成功枝に回帰が無い（low）、psql モックが sudo 経路を
  通らない（medium）、Qdrant コレクション名がハードコード（low）。
- [ ] CI の記録 SHA がコメント止まりで自動照合が無い／上流 v1 が未署名（low）。
- ユーザー判断: P1-6、P0 ガード、Grafana、Webhook、S9、バックアップ外部化、
  push、共有 lib の実機配布。

### Round 82（R4 で修正済み）

- [x] **PII スキーマ不一致**（confirmed high）→ `pii_patterns.toml` を
  `[patterns]` 配下へ移動し14パターンを有効化。実設定パース／AWS・GitHub・
  JWT redact／warn 不出の回帰2件と仕様 TC-P5 追加、証跡再生成（context 166→172）。
- [x] 証跡 generator／UI README／Makefile:test へ --locked、ContextConfig の
  Debug で pg_url を redact（実接続用 getter は不変）。
- [x] policy/event の起動検証テストを env 非依存化（vacuous pass 解消）、
  egress ホスト長 253/254・ラベル長 63/64 の境界回帰4件。
- [x] UI 二重 POST テストを実 decide() 駆動へ（seam 隔離）、root /run 枝・
  no-/run 枝・sudo -u postgres psql 経路・カスタム Qdrant コレクション名の
  回帰を追加（restore 437→461件）。
- 親が対応: fmt 差分（型エイリアス／import）、UI テストの refresh 前提、
  clippy type_complexity、証跡ハッシュ／REQ 同期、tamper の参照更新。

### Round 82 の残り指摘（未対応）

- [x] CI の記録 SHA がコメント止まりで自動照合が無い（low）— Round 83 で
  `ci/scripts/check-workflow-pins.sh` を追加し GitHub API で現行 SHA を照合。
  ci-local と CI extra_gates に配線、オフライン時は UNVERIFIED 警告で通過。
- ユーザー判断: P1-6、P0 ガード、Grafana、Webhook、S9、バックアップ外部化、
  push、共有 lib の実機配布。

### Round 83（R4 で修正済み）

- [x] workflow pin 照合ゲートを新規作成し、ci-local / CI extra_gates / Runbook
  へ配線。オンライン照合 OK、オフライン UNVERIFIED 警告の両方を実測。
- 親が対応: 全ゲート再検証（restore 461件、ci-local ok=435）。

### Round 83 の残り指摘（未対応／次ラウンド候補）

- [x] --locked 取りこぼし（medium）→ build-selfhost.sh:200 と
      `docs/integration/*.md` / `AGENTS.md` の25行へ付与。cargo fmt は非対応のため
      据え置き。
- [x] PiiConfig に deny_unknown_fields（low）→ 未知トップレベルキーで parse
      失敗→warn＋既定パターンの回帰を追加。※ `[patterns]` 内のキーは HashMap
      のため今回の対象外（将来の列挙型化タスク）。
- ユーザー判断: P1-6、P0 ガード、Grafana、Webhook、S9、バックアップ外部化、
  push、共有 lib の実機配布。

### Round 84 の残り指摘（未対応）

- [x] workflow pin ゲートの `api_unreachable` fail-open（medium）→ per-entry
  リセット方式へ変更。`PIN_RECORD_MISSING` / `PIN_RECORD_MALFORMED` は常に
  fail-closed、到達性失敗は当該エントリのみ UNVERIFIED。
- [x] workflow pin ゲートの自動回帰 `ci/scripts/test-workflow-pins.sh` を
  追加（13分岐・16件、safety-test / ci-local へ配線）。
- [x] 起動検証テストを TOML 権威と env 上書きに分離（vacuous pass 解消、
  policy / event 各+1テスト）。
- 親が対応: 全ゲート再検証（pin 16件、safety 435件）。

### Round 85 の残り指摘（未対応）

- 自動修正可能な既知指摘はすべて解消済み。残るのはユーザー判断:
  P1-6 暗号化鍵の注入先、8段階の P0 ガード有効化、Grafana Cloud 認証、
  Webhook 公開範囲、S9 実機復元演習、バックアップ外部化、push（446+ commits）。

### Round 86（R4 で修正済み）

- [x] PII の `redact_replacement` が regex キャプチャ展開（`$0`）で PII を
  復元する fail-open（medium）→ `regex::NoExpand` によるリテラル置換化、
  回帰追加。
- [x] CI `extra_gates` の2コマンドと README の cargo 例へ --locked。
- [x] workflow pin ゲートの適用範囲を「完全 SHA 以外の全 ref」へ拡大、
  `@main` の PIN_RECORD_MISSING 回帰（pin 17件）。
- [x] ci.yml の §5 自己矛盾・Makefile 行番号、Runbook の checkout 方針矛盾、
  UI clippy フラグ欠落、otel journald の migrations 欠落を訂正。
- 親が対応: fmt 差分、secret_scan のテストリテラル誤検出（`concat!` 分割・
  汎用名化）、全ゲート再検証。

### Round 86 の残り指摘（未対応）

- [x] pin ゲートのブランチ ref（low）→ タグ優先・404 で heads フォールバック、
  curl スタブと回帰を拡張（pin 17→21件）。
- ユーザー判断: P1-6、P0 ガード、Grafana、Webhook、S9、バックアップ外部化、
  push、共有 lib の実機配布。

### Round 87 の残り指摘（未対応・次ラウンド優先）

- [ ] **RustSec 既知脆弱版38件**（confirmed high）— `cargo audit --no-fetch`
  実測。patch 版あり: tokio 1.53.3 / rustls 0.23.46 / rustls-webpki 0.103.16 /
  webpki-roots 1.0.10 / rustix 1.1.6 / pulldown-cmark 0.13.6 / ctutils 0.4.4 /
  ui heapless 0.9.4。major 更新要: hickory 0.26 / rsa 0.10 / rmcp 3.9。
- [ ] CI / Makefile に cargo audit ゲートが無い（medium）— 両 lock を対象に
  extra_gates / ci-local へ追加、advisory DB 更新方針と ignore 運用を決定。
- [ ] **OTel traces filter が全 Rust スパンをドロップ**（confirmed high）—
  `otelcol-config.yaml:99-104` の include（`jarvis.*` 等）が実スパン名
  （`check` / `process_event` / `approve_approval`）と不一致。include 拡張または
  スパン名変更＋E2E 検証。
- [ ] Grafana Dependencies パネルが未スクレイプの4 job を参照（medium）、
  dead-letter/outbox 4メトリクスにパネルが無い（low）。
- [ ] status_label が4サービスで個別定義で 503 バケットが不一致（low）—
  jarvis-core へ共有化。
- [ ] ui/Cargo.toml に rust-version が無く README の MSRV 主張が裏付けなし
  （medium）。
- [ ] aarch64 クロスゲートで cmake 未宣言（medium）、libssl-dev の根拠消滅
  （low）。

- 親が対応: 全ゲート再検証（pin 21件、ci-local ok=435）。

### Round 88（R4 で修正済み）

- [x] **RustSec 既知脆弱版38件**（confirmed high）→ patch 版8件を更新
      （tokio 1.53.3 / rustls 0.23.46 / rustls-webpki 0.103.16 /
      webpki-roots 1.0.10 / rustix 1.1.6 / pulldown-cmark 0.13.6 /
      ctutils 0.4.4 / ui heapless 0.9.4）。残り29件は major 更新待ちを
      実測 ID 付きで `audit.toml` に一時 ignore（再評価 2026-12-20）。
- [x] cargo audit ゲートを新規作成し ci-local / safety-test / CI extra_gates
      へ配線。cargo-audit 0.23 が audit.toml を auto-discover しないため
      `--ignore` を明示渡しするよう修正。
- [x] **OTel traces filter が全 Rust スパンをドロップ**（confirmed high）→
      include を実スパン名へ拡張。Grafana Dependencies パネルを実 scrape job
      5件へ、dead-letter/outbox 4メトリクスのパネルを追加。
- [x] status_label を jarvis-core へ共有化（503 を専用バケットへ）、
      aarch64 に cmake 追加・libssl-dev 削除、ui rust-version 1.100 追加。
- 親が対応: Cargo.lock patch 更新、audit.toml の ID 実測転記、`--ignore`
  配線、全ゲート再検証。

### Round 88 の残り指摘（未対応）

- [ ] major 更新待ち29件の解消（hickory 0.26 / rsa 0.10 / rmcp 3.9 /
      旧 rustls 0.22 系統 / libsqlite3-sys）— 再評価 2026-12-20。
- [x] CI runner への cargo-audit 0.23.0 固定導入（冪等チェック付き、呼び出し側
      extra_gates）。共有 workflow は別リポジトリのため呼び出し側を選択。

### Round 89（R4 で修正済み）

- [x] MCP discovery を timeout＋ページ(64)／ツール(4096)上限で fail-closed、
      git 子プロセスを kill_on_drop＋明示 kill/reap、出力をストリーミング計測。
- [x] deploy のバイナリ install を fail-closed、build 経路の9 unit を共有
      owner/mode 表へ編入、setup の libssl-dev 削除。
- [x] check-deploy-drift に stat ベース owner/mode 検査（固定語彙 E_BAD_OWNER_MODE）
      と tamper 回帰。
- [x] check-evidence に RISK_SET_MISMATCH ゲート、6証跡の risk_items 同期、
      generator のパス厳格化・成功断片漏出排除・自動回帰。
- 親が対応: テストの json import／pid unwrap／spawn env ガード、docs ガードの
      ファイル名形式（日付＋ハッシュ）、全ゲート再検証。

### Round 89 の残り指摘（未対応）

- [ ] major 更新待ち29件の解消（hickory 0.26 / rsa 0.10 / rmcp 3.9 /
      旧 rustls 0.22 系統 / libsqlite3-sys）— 再評価 2026-12-20。
- ユーザー判断: P1-6、P0 ガード、Grafana、Webhook、S9、バックアップ外部化、
  push、共有 lib の実機配布。

### Round 90（R4 で修正済み）

- [x] **retain_days オーバーフロー**（confirmed high）→ 乗算前上限
      `106751991167300` 検証＋未来 cutoff ガード、境界回帰（backup-args 9件）。
- [x] SSRF 判定を IANA/IETF 分類へ（is_global は toolchain で不安定のため
      明示実装）、非 UTF-8 パスも機密判定対象へ。
- [x] MCP 子プロセスの kill+reap、ツール上限を extend 前に事前検査。
- [x] status_label に 410、safe_client_error_text 単一化、policy gRPC の
      serde エラー固定文言化。
- [x] drift の egress 形式を Rust と同一化、JARVIS_DRIFT_BINARY、JSON 安全
      構築、監査ドキュメントの古い主張と古い固定件数を同期。
- 親が対応: is_global 自前実装、format! エスケープ、shellcheck 抑止、
      secret_scan のテストリテラル、全ゲート再検証。

### Round 90 の残り指摘（未対応）

- [ ] major 更新待ち29件（再評価 2026-12-20）。
- ユーザー判断: P1-6、P0 ガード、Grafana、Webhook、S9、バックアップ外部化、
  push、共有 lib の実機配布。

### Round 91（R4 で修正済み）

- [x] **admin_token 空白 fail-open**（confirmed high）→ event/context に
      mcp-gw と同じ validate_admin_token を追加、未設定は互換維持。
- [x] systemd 依存順序（migrations→policy→{mcp-gw,context}→opencode→event）
      と migration unit の両 DB 適用、build-selfhost の順序表示を同期。
- [x] 3サービスの /health を HEALTH_* 契約へ統一し下流状態を反映、
      deploy のヘルスゲートで degraded(503) を検出。
- [x] テスト決定性: 証跡ファイル名の動的選択、固定 /tmp を pid+AtomicU64
      一意化、承認ポーリングへ 5s デッドライン。
- [x] 未使用生成 proto／PolicyError 削除、internal gateway error 文言を
      jarvis-core 集約、UI env 定数化、ドキュメント行番号ドリフト解消。
- 親が対応: fmt 差分、空ファイル git rm、systemd-analyze verify、全ゲート再検証。

### Round 91 の残り指摘（未対応／次ラウンド候補）

- [ ] mcp-gw testing.rs の固定 /tmp パス（pid+AtomicU64 未対応、
      medium）— 残存する並走競合経路。
- [x] approval.rs / http.rs / policy/http.rs のデッドラインなし承認ポーリング
      4箇所＋testing.rs の固定 /tmp → Round 92 で解消。
- [x] event の OpenCode URL を config 側で解決し公開 getter 経由で probe
      → Round 92 で解消。
- ユーザー判断: P1-6、P0 ガード、Grafana、Webhook、S9、バックアップ外部化、
  push、共有 lib の実機配布。

### Round 92（R4 で修正済み）

- [x] testing.rs の固定 /tmp を pid+AtomicU64 一意化、承認ポーリング4箇所へ
      5s デッドライン。
- [x] EventConfig に OpenCode URL getter／load 時スナップショットを追加、
      http.rs の直接 env 参照を廃止。

### Round 92 の残り指摘（未対応・次ラウンド優先）

- [ ] policy 仕様に 410 Gone（期限切れ承認）の HTTP 契約が未反映（中）—
      REQ-10＋Scenario＋Traceability 追加、証跡再生成、両 index 同期。
- [ ] 3仕様（event/context/mcp-gw）に下流連動 /health の degraded 契約が
      未反映（中）— REQ＋Scenario＋Traceability 追加、証跡再生成。
- [ ] mcp-gw /health が allowlist 経由で upstream 登録を判定し、登録済みだが
      allowlist 未掲載だと永続 503 誤判定（中）— Gateway の実際の登録数を
      直接公開して照合。
- [ ] deploy-mcp-gw-safe.sh が 503 degraded を消费せず劣化サービスを
      DONE と報告（中）— スクリプト側にも degraded 検出と rollback。
- [ ] deploy.rs のヘルスゲートが固定5秒＋1回プローブで低速起動を誤判定
      （中）— 上限付きリトライ（20回×0.5s 等）へ。
- [ ] drift が casbin_model.conf の内容・owner/mode を未検査（中）。
- [x] ui/README.md:39 と remaining-tasks.md:288 の clippy 例へ
      undocumented_unsafe_blocks を追加。
- [x] policy 仕様へ 410 Gone、3仕様へ /health degraded 契約を反映、
      4証跡を実測再生成（policy 211 / event 91 / context 181 / mcp 151）。
- [x] mcp-gw /health を allowlist 推論から登録済みサーバ照合へ、
      deploy-mcp-gw-safe.sh の degraded rollback、deploy.rs の上限付き
      リトライ、drift の /health 200 検証。
- [x] drift が casbin_model.conf の内容・owner/mode を未検査 →
      追加＋tamper 回帰。

### Round 93 の残り指摘（未対応）

- [ ] mcp-gw approval.rs:712 の approver task が timeout 未適用（低）—
      成功経路ではハングしないが一貫性のため。
- [ ] major 更新待ち29件（再評価 2026-12-20）。
- ユーザー判断: P1-6、P0 ガード、Grafana、Webhook、S9、バックアップ外部化、
  push、共有 lib の実機配布。

- 親が対応: tokio::spawn 所有値化、証跡再生成（4件）、index／Runbook の
  参照同期、全ゲート再検証。

### Round 94（R4 で修正済み）

- [x] approval.rs:712 の approver task へ5sデッドライン＋タイムアウト回帰。
- [x] mcp-gw の期限切れ理由を共有定数 APPROVAL_EXPIRED_BODY へ、UI 仕様へ
      410 契約、policy 仕様へ /health 503 degraded 契約を追加。
- [x] 第3のデプロイ経路 install-mcp-gateway.sh へ上限付きリトライ＋degraded
      検出＋drift 強制ゲート、静的ガード追加。
- [x] deploy.rs の curl へ --max-time 2、degraded 判定を whitespace-tolerant
      化、pretty-printed 回帰を Rust/shell 双方へ追加。
- [x] check-evidence に新契約キーワード、event の未閉鎖 gherkin 修正、
      specs/index／local-operations-runbook を現行 unit へ同期。
- [x] 5証跡を実測再生成（policy 211 / event 91 / context 181 / mcp 153 /
      ui 85）、REQ と notes キーワードを同期。

### Round 94 の残り指摘（未対応）

- [ ] major 更新待ち29件（再評価 2026-12-20）。
- ユーザー判断: P1-6、P0 ガード、Grafana、Webhook、S9、バックアップ外部化、
  push、共有 lib の実機配布。

- 親が対応: 5証跡再生成、Runbook 参照同期、全ゲート再検証。

### Round 95（R4 で修正済み）

- [x] 4サービス設定構造体へ `deny_unknown_fields`、mcp-gw の直接 env キーを
      Figment `.ignore()` へ。タイポしたガードキーが起動拒否される回帰を各クレートへ追加。
- [x] check-evidence に policy 固有の HEALTH_KILL_SWITCH_BODY /
      HEALTH_NO_POLICY_BODY キーワードを追加。

### Round 95 の残り指摘（未対応・次ラウンド優先）

- [ ] **source が VARCHAR(100) で Rust 検証上限 4096 と不一致**（high、
      子エージェントが途中終了したため未実装）— migration 006 で TEXT 化、
      ON CONFLICT へ source/expires_at 追加、境界回帰。
- [ ] **両 DB バックアップギャップ**（high、子の実装が既存10回帰と矛盾した
      ため差し戻し）— JARVIS_PG_EXTRA_DATABASES を segment 単位で設計し直し、
      manifest role と verify/restore の解決を整合させたうえで再実装。
      DB_NAME 上書きが pg.sh:29 で無視される low も同時に解消する。
- [ ] smoke-mcp-gw.sh:90-94 の /health が 200 degraded 本文を合格させる
      （medium、子が途中終了）— deploy と同じ契約へ。
- [ ] reconcile_pending の同時実行制御なし（low）— advisory lock または
      条件付き UPDATE。
- [ ] mcp_allowlist.toml:9-13 の行番号陳腐化、CONTEXT_TOKEN_HEADER 重複、
      hickory/rsa 等 major 更新待ち29件（再評価 2026-12-20）。
- ユーザー判断: P1-6、P0 ガード、Grafana、Webhook、S9、バックアップ外部化、
  push、共有 lib の実機配布。

- 親が対応: 2体の途中終了を確認、矛盾する backup-multidb 差分を差し戻し、
  全ゲート再検証。

### Round 96（/swarm-limited・親が直接対応）

- [x] source のスキーマ不一致（high）→ migration 006 で `TEXT` 化、
      ON CONFLICT へ `source` / `expires_at` を追加。
- [x] DB_NAME が pg.sh で無視される（low）→ `${DB_NAME:-jarvis}` へ。
- [x] smoke の /health が 200 degraded を合格させる（medium）→
      固定本文 `status=degraded` を検出して FAIL。
- 親が対応: 全ゲート再検証（restore 461件）。

### Round 96 の残り指摘（未対応）

- [ ] 両 DB バックアップギャップ（high）— 子実装は既存回帰と矛盾したため
      差し戻し済み。manifest role と verify/restore を整合させた再設計が必要。
- [x] reconcile_pending の同時実行制御（low）→ mark_failed を条件付き UPDATE
      （`attempts <= $2`）へ、古いスナップショットの上書きを防止。
- [x] mcp_allowlist.toml の行番号陳腐化 → 関数名参照へ変更。
- [x] CONTEXT_TOKEN_HEADER 重複 → jarvis-core 定数の alias へ集約。
- [ ] major 更新待ち29件（再評価 2026-12-20）。
- ユーザー判断: P1-6、P0 ガード、Grafana、Webhook、S9、バックアップ外部化、
  push、共有 lib の実機配布。

### Round 97（/swarm-limited・親が直接対応）

- 新規サブエージェント起動数 0（セッション合計上限40を超過中）。
- 親が上記3件を直接修正、全ゲート再検証。

### Round 97 の残り指摘（未対応）

- [ ] 両 DB バックアップギャップ（high）— manifest role と verify/restore を
      整合させた再設計が必要（前回の子実装は既存回帰と矛盾し差し戻し済み）。
- [ ] major 更新待ち29件（再評価 2026-12-20）。
- ユーザー判断: P1-6、P0 ガード、Grafana、Webhook、S9、バックアップ外部化、
  push、共有 lib の実機配布。

### Round 98（/swarm-limited・親が直接対応）

- 新規サブエージェント起動数 0（セッション合計上限40を超過中）。
- [x] ADR-0003 の「影響」節に履歴の注記を追加（web.search の 400
      `no native handler` は決定時点の状態で、現行は upstream MCP 経由）。
- 自動修正可能な既知タスクは枯渇。残るのは設計判断が必要な両 DB バックアップ
  （前回の子実装は既存回帰と矛盾し差し戻し済み）とユーザー判断項目のみ。

### Round 98 の残り指摘（未対応）

- [ ] 両 DB バックアップギャップ（high）— 設計判断が必要のため一旦保留。
- [ ] major 更新待ち29件（再評価 2026-12-20）。
- ユーザー判断: P1-6、P0 ガード、Grafana、Webhook、S9、バックアップ外部化、
  push、共有 lib の実機配布。

### Round 99（/swarm-limited・親が直接対応）

- 新規サブエージェント起動数 0（セッション合計上限40を超過中）。
- [x] 両 DB バックアップギャップの判断材料を `local-restore-design.md` の
      「未決定事項」へ追記（現状、却下した実装の理由、3つの判断論点、
      S9 での検証方針）。設計判断が要るため実装は保留。
- 親が yomiyasu で本文を検査（変更不要、ゲートは技術用語として残す）。

### Round 99 の残り指摘（未対応）

- [ ] 両 DB バックアップギャップ（high）— 設計判断待ち。判断材料は
      `local-restore-design.md` に整備済み。
- [ ] major 更新待ち29件（再評価 2026-12-20）。
- ユーザー判断: P1-6、P0 ガード、Grafana、Webhook、S9、バックアップ外部化、
  push、共有 lib の実機配布。
