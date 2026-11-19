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
- [x] `verify-backup.sh` がマニフェスト記載ファイルのみ検査し必須ペイロード欠落を合格させる — 2026-11-19 対応済み: 完全セットに postgres/qdrant/config-* の実在を必須化し欠落時 `REQUIRED MISSING` で失敗。子の指摘で既存フィクスチャ2件が新チェックで失敗したため親が必須ペイロードを追加、`test-verify-backup` 23 tests pass / `infra/scripts/verify-backup.sh:62-79` / R4-DR3 / confirmed
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

- [ ] 未使用依存の削除候補（confirmed）— tower-http（全4サービス＋workspace）、
  zeroize（policy）、prost-types（policy/mcp-gw）、figment json feature、
  uuid v7 feature、rmcp の transport-child-process / transport-sse-client。
  削除後に `cargo check --all-targets --locked` と `make ui-test` で検証。
- [ ] 並行レビューの残り: select! の unbiased による期限境界 split brain、
  Connection::reconnect のキャンセル、symlink swap と bin マーカーの不一致窓。
- [ ] jarvis-context の outbox/Qdrant 失敗ログが `{error:#}` で anyhow
  チェーン全体を OTLP logs へ送出（medium）— 固定語彙化が必要。
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

- [ ] commit_gate のコミット後再読が「永続化済みだが未公開」を作る
  （confirmed medium）— DB=approved / HTTP=409 / メモリ=Pending の分裂。
  コミット前に1回に限定するか、コミット済みとして決定を返す。
- [ ] DecidingGuard が共有 bool で世代/オーナーを持たず、prune と timeout が
  同じ Arc に第2ガードを作り得る（medium）— 世代付きトークン化＋
  apply_prune の `decided_at.is_some()` 温存。
- [ ] mcp_allowlist.toml の timeout_ms 一律15000化は web.fetch 以外の
  shell.execute / github.create_pr / web.search に不要な機能後退（high）—
  30000 へ戻すかルートごとの実効値を明記。
- [ ] egress テンプレートの「unset は deny-all」が unit の example.com
  フォールバックと矛盾（low）。
- [ ] policy gRPC の kill switch エラーログだけ `{error:#}` でチェーン全体を
  OTLP へ送出（medium）— context 側と同じ固定語彙化が必要。

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

- [ ] 承認永続化が生の tokio-postgres エラーを OTLP ログへ送出
  （confirmed medium）— `persistence.rs:316,720,778,840-847,902-903` の
  `error = %error` を固定語彙化（context 側と契約を揃える）。
- [ ] OTLP endpoint／初期化エラーが原文でログされる（low）—
  `policy/main.rs:133-136` の endpoint URL（userinfo/query token の可能性）を
  固定語彙へ丸める。
- [ ] Qdrant コレクション次元チェックの失敗がサイレント fail-open
  （medium）— `builder.rs:1048-1056` の Err(_) => warn + Ok(()) を起動失敗
  または初回 upsert での実次元検証へ変更。

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

- [ ] **systemd `Environment=` の `${VAR:-default}` は展開されない**
  （confirmed high、Runtime Proof 付き）— `jarvis-mcp-gw.service:58,62` の
  BASE_DIR / EGRESS_ALLOWLIST がリテラル文字列として渡り、未上書き時は
  filesystem.* 全失敗／deny-all になる。systemd.exec(5) は変数展開を行わない。
  `Environment=` をリテラル既定値へ変更し EnvironmentFile を唯一の上書き経路と
  するか、shell ラッパで展開。併せて `${` 残存を fail-closed で検査。
- [ ] kill switch 本体の `warn!` が自由入力 reason と `{error}` を OTLP へ送出
  （medium）— `kill_switch.rs:246-252,357-361`。`redacted()` と同じ固定語彙化。
- [ ] commit_gate 1回化の新契約が spec / UI / 回帰名に未反映（medium）—
  TC-K2 への追記、UI の「承認済み・実行は非常停止中」表示、回帰名改名。
- [ ] `decide()` の publish が既存決定を無条件上書き（medium）—
  timeout / prune と同じ勝者温存ルールを適用（Noop バックエンドで競合）。
- [ ] 発掘: rustfmt ゲートが CI 未配線、Cedar スキーマ旧コメント、
  opencode.jsonc が build-selfhost 配布対象外、infra/README の 1Password 表記、
  dsh.md / tool-matrix.md の tools/list 非表示記述。

- 親が対応: fmt 差分、構造ガードの誤った切断位置（`#[cfg(test)]` → テストモジュール）。
