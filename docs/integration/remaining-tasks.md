---
type: Reference
title: "JARVIS 残タスク一覧"
description: "残タスク・判断待ち事項の洗い出し"
tags:
  - jarvis
  - tasks
  - integration
generated:
  by: "DeepSeek Harness (lead agent)"
  at: "2026-11-17T23:33:40+09:00"
---

本ドキュメントは、リポジトリ内容・設定・実装・ローカル systemd 実態・ドキュメント記載を点検して整理した「現時点で残っている/判断待ちのタスク」の一覧です。
**安全第一**の観点で、破壊的操作や未確認の前提に依存する作業は「要判断」として分離しています。

## 最新状態（2026-11-19 時点・下記の確認前提は 2026-11-17 のスナップショット）

- 作業ツリー: クリーン、`origin/master` と同期（unpushed 0）。
- コア4サービス: すべて `active`、health 200（8081〜8084）。
- GitHub Issues / PR: オープン 0 件。
- **CI**: 全 run が約3秒で failure。原因は Checks API の annotation で
  「account payments have failed / spending limit」= **GitHub Billing 問題**。
  コード・workflow 設定の不具合ではない（[ci.yml](../../.github/workflows/ci.yml) は
  fmt/check/test/clippy + 追加 gate をローカル `make ci-local` と同等に実行）。
  ローカルゲート `make ci-local` は全成功。
- **バックアップ**: ローカル実装が完了し実バックアップ検証済み
  （[backup-redesign-notes.md](backup-redesign-notes.md)）。
  2026-11-19 にユーザーが `sudo -n bash infra/scripts/install-backup-timer.sh --apply`
  を実行し、timer は**実機で active / enabled**、手動バックアップの整合性検証は
  ok=30 fail=0。ただし repo 側で `--repo-dir` 追加と `ProtectHome=read-only` 変更を
  行ったため、**修正済み unit の実機再適用は未**（root 必須・ユーザー実行待ち）。
- **復元（S9）**: `infra/scripts/test-restore-local.sh` を追加し、
  合成フィクスチャで dry-run / apply 経路を自動検証（**47 tests pass**、
  本番リソース非接触を多重ガード）。**実機の隔離先復元演習は承認待ち**。
- **承認 API**: オプションのトークン認証を追加（既定は無効で後方互換）。
  4コンポーネント横断の契約テスト済み。
- **承認 UI のトークン名・scope 整合（R4-UI）**: UI が
  `JARVIS_POLICY_APPROVAL_API_TOKEN`（旧 `JARVIS_POLICY_APPROVAL_TOKEN` は
  UI 側の一時フォールバックで、policy 側の実変数ではない）と
  `JARVIS_POLICY_APPROVAL_ADMIN_TOKEN`
  （未設定時は API トークンへフォールバック）を読み、一覧取得に API
  トークン、承認・却下に admin トークンを `X-Jarvis-Approval-Token` で
  送信するよう修正。env 名→scope・ヘッダの契約テストを追加。
  秘密情報の値は記載しない。
- **読み取りツールの機微ファイル拒否**: `sandbox.rs` のパス解決で `.env`,
  `bws.env`, `*token*`, `*secret*`, `*.pem`, `*.key` を拒否する defensive
  check を追加（MCP-4 の残リスク低減）。回帰テスト・smoke 12/12 成功。
- **手動 review gate の健全性**: クリーンな clone（`/tmp` に HEAD を clone）で
  `make ci-local` が成功（legacy guards 22 + backup 23 checks）。GitHub Actions の
  ubuntu-latest でも同じく Rust Toolchain + Python3 で動作する根拠。
- **デプロイ経路の整合性確認**: `deploy-mcp-gw-safe.sh` は unit / allowlist を
  同期し、`check-deploy-drift.sh` は新しい `tools/list`（承認ツールも表示）を
  部分集合で検証するよう更新済み。トークン関連 env はテンプレートで
  コメントアウトされており、デプロイ時に誤って有効化されることはない。
  `policy_http_addr` は config 既定値が実機の `:8081` と一致。
- **migration unit のデプロイ配線**: `jarvis-migrations.service` を
  [build-selfhost.sh](../../infra/scripts/build-selfhost.sh) の
  `JARVIS_SERVICE_UNITS` と配置ループへ追加（`User=jarvis` を保持したまま
  通常デプロイで配置）。2026-11-19 に migration ソース
  （`rust/migrations/*.sql`）の `/opt/jarvis/rust/migrations/` への配置と
  `.jarvis-source-commit` 生成も同スクリプトへ統合し、ソース／マーカー
  欠落時は fail-fast 化（I36-A）。sqlx-cli の導入と、配置・有効化・検証手順は
  後述「承認待ち実機タスクの実行手順」の MIG 節に集約。
- **opencode.jsonc の配布は未配線（2026-11-20 確認・記録のみ）**: repo 側の
  秘密ファイル read deny は
  [infra/config/opencode/opencode.jsonc](../../infra/config/opencode/opencode.jsonc)
  に定義されているが、
  [build-selfhost.sh](../../infra/scripts/build-selfhost.sh) の配布対象に
  含まれていない（旧 cloud-init レガシー
  [cloud-init.yaml](../../infra/opentofu/cloud-init.yaml) にのみ記載）。
  したがって repo 側の deny は実機に届かない。**未配線**として記録し、
  配線自体は別タスクの担当とする（本タスクではスクリプトを変更しない）。

## 確認前提（2026-11-17 の洗い出し時点・履歴として保持）

- ブランチ/作業ツリー: `master...origin/master`、未コミット変更なし
- GitHub Issues: オープン 0 件（`gh issue list --state open`）
- ローカル稼働サービス: `jarvis-policy` / `jarvis-mcp-gw` / `jarvis-event` / `jarvis-context` は `active` / `enabled`
- バックアップ（2026-11-17 時点）: 両 unit は repo に存在したが未導入（not-installed）。
  現在の状態は「最新状態」節を参照。
- 方針: OCI は使用しない（[adr/0004-local-only-no-oci.md](../../adr/0004-local-only-no-oci.md)）

## 1. MCP Gateway / ツール機能（優先度: 高）

| ID | タスク | 現状・根拠 | 次のアクション（案） |
|---|---|---|---|
| MCP-1 | 承認フローの設計・実装・E2E 配線 | **2026-11-15 実装済み（repo側）**: gateway が `requires_approval=true` のツール呼び出しを jarvis-policy の HTTP 承認フロー（`POST /v1/approval`）に転送し、オペレータが `POST /v1/approval/{id}/approve|deny` で決定するまで待機する（`rust/jarvis-mcp-gw/src/approval.rs`, `lib.rs`）。失敗・期限切れは fail-closed で拒否。承認必須ツールも `tools/list` に表示されるよう変更（旧「非表示」ワークアラウンド廃止）。E2E テスト追加（`http::tests::execute_endpoint_blocks_until_operator_approval` 等）。 | 実機適用は次回 `deploy-mcp-gw-safe.sh --apply` 時（policy HTTP addr 既定 `127.0.0.1:8081` は unit 既存設定と整合）。**オペレーター承認UI は GPUI コンソール（`ui/`, ADR-0005）で実装済み**（2026-11-16、実機ビルド・起動検証済み、`make ui-run`）。監査永続化（`PgApprovalPersistence`）は 2026-11-19 に実装済み（opt-in、`pg_url` 設定時に注入）。 |
| MCP-2 | 未実装ツールのネイティブ実装または上流 MCP 登録 | `web.search` / `homeassistant.get_state` / `android.get_device_info` 等はネイティブ未実装（2026-11-15 時点で `requires_approval = true` 化）。`filesystem.write` / `shell.execute` / `git.commit` / `github.create_pr` / `homeassistant.call_service` / `android.send_notification` も承認必須（[tool-matrix.md](tool-matrix.md), [dsh.md](dsh.md)）。※ 2026-11-16 以降は MCP-1 実装により承認必須ツールも `tools/list` に表示される。 | **2026-11-16 現状**: MCP-1 実装で未実装ツールも `tools/list` に表示されるようになり、呼び出しは承認フローを通過した後 `no native handler ... register an upstream MCP server` で fail-closed に失敗する（安全だが UX 上のデッドエンド）。ネイティブ実装には外部サービス接続（HA/Android/Web 検索）と資格情報、書き込み系には systemd `ReadWritePaths` の拡張が必要であり、ツール単位の利用要件判断が先。当面は現状の fail-closed 挙動を維持する。 |
| MCP-3 | allowlist `sandbox_type`（ツール単位）の整理 | **2026-11-15 解消（repo側）**: ADR-0002 の `opencode` CLI sandbox fallback 撤去に伴い、option A（dead config 削除）を採用。[infra/config/mcp_allowlist.toml](../../infra/config/mcp_allowlist.toml) から per-tool `sandbox_type` キーを削除（TOML parse 検証済み）。gateway 全体設定 `JARVIS_MCP_GW_SANDBOX_TYPE` は既存 unit/env 互換のため解析のみ残存し、実行経路には影響しない（[adr/0002-runtime-agnostic.md](../../adr/0002-runtime-agnostic.md) 更新済み）。 | `/etc/jarvis/mcp_allowlist.toml` への同期は次回 `deploy-mcp-gw-safe.sh --apply` 時（適用まで `check-deploy-drift.sh` は PENDING を表示する）。将来ツール単位の隔離が必要になった場合は、上流 MCP サーバ側の sandbox 設定（`register_mcp_server` のコマンド）で実装する。 |
| MCP-4 | 読み取り基準ディレクトリの絞り込み | **2026-11-15 解消（repo側）**: [infra/systemd/jarvis-mcp-gw.service](../../infra/systemd/jarvis-mcp-gw.service) の `JARVIS_MCP_GW_BASE_DIR` を `/home/koppe` から repo ルート（`/home/koppe/ドキュメント/作業用/jarvis/jarvis`）へ絞り込み。`ProtectHome=read-only` は `/home` 読取に必要なため維持。smoke にも `JARVIS_MCP_GW_BASE_DIR="$repo"` を追加し、狭い基準での `filesystem.list`/`git.status` を検証する。 | `/etc` への同期は次回 `deploy-mcp-gw-safe.sh --apply` 時。適用後は repo 外の読み取りが拒否されることを代表的 `tools/call` で確認する（実機変更は別判断）。 |

### A-M16 監査永続化の要件分離・判断材料（2026-11-18）

以下は A-M16 の判断材料（2026-11-18 時点）。**結論: 要判断**だったが、
(1) の永続監査は 2026-11-19 に `PgApprovalPersistence` として実装済み
（opt-in）。3要件を混同せず、目的・保存場所・保持期間・機微情報の扱い・
コストで比較する。

#### 現状（Runtime Proof / A-M16）

- [approval.rs](../../rust/jarvis-policy/src/approval.rs):1-18 は、決定を
  メモリ保持しつつ opt-in の PostgreSQL バックエンド
  （[persistence.rs](../../rust/jarvis-policy/src/persistence.rs) の
  `PgApprovalPersistence`）で決定と監査行を同一トランザクションでコミットする
  設計を説明する。2026-11-18 時点では未実装だったが、2026-11-19 に実装済み。
- [001_initial.sql](../../rust/migrations/001_initial.sql):20-36 に `audit_log`
  テーブル（event_id / correlation_id / principal / action / resource /
  decision / context / policy_version / latency_ms）と3インデックスが
  定義済み。ただし参照は migration 内のみで、書き込みコードはない。
- [approvals.rs](../../ui/src/approvals.rs):19-29 は `pending` キューと
  `status` のみを保持。決定済み履歴や CSV / JSON export はない。
- [ui/README.md](../../ui/README.md):80-90 は `approved_by` 固定値の送信と、
  P0-1 / P0-3 対応で認証 Identity へ紐付ける方針を記録（値は記載しない）。
- [dsh.md](dsh.md) の「現状更新」は、承認 UI と監査永続化を実装済み、
  通知を実装見送りと整理。[todo.md](../../todo.md) の P3-9 は
  「実装見送り」判断（公開 ntfy への機微 `context` 送信リスク）。

#### 3要件の比較（A-M16）

- **(1) 永続監査（MCP-1 / AUD-01）**: 目的=セキュリティ/コンプライアンスの
  決定記録。保存場所=PostgreSQL の既存 `audit_log` テーブル。
  保持期間=未決定（方針次第）。機微情報=`context` JSONB に生の引数を
  残さない設計が必要（id / resource / decision 程度に限定する案）。
  コスト=中（policy 側の記録処理と既存スキーマの配線が必要）。
- **(2) ローカル UI 決定履歴・CSV / JSON export（A-M16）**: 目的=運用者の
  直近決定の参照と書き出し。保存場所=UI のメモリまたはローカルファイル
  （DB とは別系統）。保持期間=未決定（例: 直近 N 件）。
  機微情報=保存対象をメタデータに限定し、引数本文は残さない。
  コスト=低〜中（UI 状態とファイル I/O のみで DB 変更不要）。
- **(3) 通知（P3-9）**: 目的=保留承認への気付き。保存場所=保存せず
  ntfy 等の外部先へ送信するのみ。保持期間=通知先側の設定に依存し
  repo 側は未決定。機微情報=公開トピックへ `context` を送らない契約が
  必須（現状は見送り判断）。コスト=低（薄い橋渡し）だが契約設計が先。

#### 判断（A-M16）

- 3つは目的が異なるため一本の実装に統合しない。永続監査は DB、
  UI 履歴はローカル、通知は外部送信と別系統に扱う。
- 保持期間・機微情報の許容範囲・通知契約が未確定のため、
  **判断は確定させず要判断とする**。決定後に最小スキーマと
  エクスポート形式を設計する。

### P0-3 principal 自己申告の現状確認（2026-11-18）

[todo.md](../../todo.md) の P0-3 について、実コードを読み取り専用で追った
結果を記録する。**結論: 一部実装済み（`/mcp` のみ）。全体としては要対応**。

#### 経路ごとの事実（Runtime Proof）

- `/mcp` エンドポイントはサーバ側固定。
  [http.rs](../../rust/jarvis-mcp-gw/src/http.rs):42-45 が
  `gateway.mcp_principal()` を `JarvisMcpServer::new` へ渡し、
  [mcp_server.rs](../../rust/jarvis-mcp-gw/src/mcp_server.rs):28-40 / 137 が
  固定 principal でツール一覧と `execute_tool` を実行する。
- `/v1/execute` は自己申告のまま。
  [http.rs](../../rust/jarvis-mcp-gw/src/http.rs):28-33 の
  `ExecuteToolRequest.principal` を JSON ボディから受け取り、:105-113 で
  `gateway.execute_tool(..., &req.principal)` へ渡す。
- gRPC も自己申告のまま。[grpc.rs](../../rust/jarvis-mcp-gw/src/grpc.rs):19-32 が
  [proto](../../rust/jarvis-mcp-gw/proto/mcp_gateway.proto):10-14 の
  `principal`（field 3）をそのまま `execute_tool` へ渡す。
- 渡された principal は [lib.rs](../../rust/jarvis-mcp-gw/src/lib.rs):63-104 で
  policy の `is_allowed` と allowlist の principal チェックに使われる。
  呼び出し側が許可済み principal を名乗る余地がある。
- 承認決定側も自己申告。[ui/README.md](../../ui/README.md):80-90 が
  `approved_by` の固定値送信を P0-1 / P0-3 関連の未移行事項として明記
  （[approvals.rs](../../ui/src/approvals.rs):127、値は記録しない）。

#### 判断（P0-3）

- 既定状態では P0-3 を「実装済み」とはしない（`/v1/execute` と gRPC が未対応）。
  後述の opt-in フラグ ON 時はこれら2経路もサーバ側固定値へ統一される。
- `todo.md` の P0-3 に証跡を追記し、対応範囲を残る2経路へ絞って管理する。
  認証済み Identity からの導出等、設計判断は確定させない。

### P0-3 安全な修正方針の実施状況（2026-11-19 更新）

前節の現状確認を踏まえた `require_server_side_principal`（既定 `false`）に
よる後方互換な修正は **repo 側に実装済み**。残る判断は本番有効化と
監査粒度の2点のみ。実機操作は行っていない。秘密情報の値は記載しない。

#### principal の流れ（3経路の比較）

- 共通の落とし所: 3経路とも最終的に
  `McpGateway::execute_tool(tool, args, principal)`
  （[lib.rs](../../rust/jarvis-mcp-gw/src/lib.rs):63-188）へ収束する。
  ここで principal は policy の `is_allowed`（:73-76, :173-177）、
  allowlist の `is_principal_allowed` / tool 単位
  `allowed_principals`（:93-104）、承認リクエスト（:128）に使われる。
  実処理の `gateway.execute`（:187）は principal を見ない。
- `/mcp`（サーバ側固定）: [http.rs](../../rust/jarvis-mcp-gw/src/http.rs):42-46
  が `gateway.mcp_principal().to_string()` を
  `JarvisMcpServer::new` へ渡し、
  [mcp_server.rs](../../rust/jarvis-mcp-gw/src/mcp_server.rs):26-35 / 137
  が保持値で `tools/list` と `call_tool` を実行する。値は
  [config.rs](../../rust/jarvis-mcp-gw/src/config.rs):27-30 / 60 の
  `mcp_principal`（既定 `opencode`、
  `JARVIS_MCP_GW_MCP_PRINCIPAL` で上書き可）。クライアントは選べない。
- `/v1/execute`（リクエスト由来）:
  [http.rs](../../rust/jarvis-mcp-gw/src/http.rs):28-33 の
  `ExecuteToolRequest.principal` を JSON ボディから受け取り、
  :105-113 でそのまま `execute_tool` へ渡す。
- gRPC（リクエスト由来）:
  [grpc.rs](../../rust/jarvis-mcp-gw/src/grpc.rs):19-32 が
  [proto](../../rust/jarvis-mcp-gw/proto/mcp_gateway.proto):10-14 の
  `principal`（field 3）をそのまま渡す。
- policy 側は principal を文字列として受け取るだけ
  （[client.rs](../../rust/jarvis-policy/src/client.rs):48-95 →
  [casbin_policy.csv](../../infra/config/policy/casbin_policy.csv)）。
  「誰が送ったか」は認証せず、名乗りを検証しない。
  よって `/v1/execute` / gRPC は許可済み principal を名乗れる。

#### /mcp と同じ固定値を他2経路へ適用できるか

- 技術的には可能。`execute_tool` が単一の choke point なので、
  `principal` 引数の解決をここへ集約すれば3経路を同時に覆える。
  `/mcp` は既に同じ値を渡すためフラグ ON でも挙動は不変（冪等）。
  wire 形式（JSON の `principal` / proto field 3）は維持できる。
- ただし「principal の認証」は解決しない。ON 時は全呼び出しが
  同一 identity になり、監査ログの呼び出し者粒度が失われる。
  loopback + Tailscale の信頼境界を前提とした defense-in-depth であり、
  認証 Identity からの導出（P0-1 と同じ設計判断）は別途必要。
- 承認側の `approved_by` は依然としてボディ由来
  （[ui/README.md](../../ui/README.md):80-90）で、本フラグの範囲外。

#### opt-in フラグの実装（require_server_side_principal / 既定 false）

実装箇所（実装済み・既定 false）:

- [config.rs](../../rust/jarvis-mcp-gw/src/config.rs):76-80 に
  `#[serde(default)] pub require_server_side_principal: bool` が定義済み。
  `Default`（:259）で `false`。Figment の
  `Env::prefixed("JARVIS_MCP_GW_")` により
  `JARVIS_MCP_GW_REQUIRE_SERVER_SIDE_PRINCIPAL=true` で有効化できる。
  既存 config にキーが無くても parse 成功する（後方互換）。
- [lib.rs](../../rust/jarvis-mcp-gw/src/lib.rs):95-103 の
  `execute_tool` 先頭で caller 由来の `principal` を実効 principal
  （フラグ ON 時は `mcp_principal`）でシャドーイングする。
  以降の policy / allowlist / 承認リクエスト / ログの判定はすべて
  同じ実効 principal を使う。`/mcp` は同一値を渡すので
  フラグ ON でも挙動は不変（冪等）。
- `http.rs` / `grpc.rs` / `mcp_gateway.proto` は変更しない。
  リクエストの `principal` は OFF で従来どおり有効、
  ON では無視される（wire 互換を維持するための選択）。
- 本番有効化時に必要な作業: 設定テンプレート
  [mcp_gateway.env](../../infra/config/mcp_gateway.env) への
  切替手順の注記、および `bws run` + figment env 経由の
  unit 更新（値は記載しない）。
- ON 時は全経路が `mcp_principal` に統一され、名乗りは無視される。
  `mcp_principal` が allowlist の `principals` と各 tool の
  `allowed_principals` に無い場合は全ツールが fail-closed になる。

採用しない案（代替として検討・記録のみ）:

- 各 entry point（[http.rs](../../rust/jarvis-mcp-gw/src/http.rs):109-112 /
  [grpc.rs](../../rust/jarvis-mcp-gw/src/grpc.rs):24-30）で
  `req.principal` を `gateway.mcp_principal()` へ差し替える方式は、
  変更量は小さいが入口が増えるたびに修正が必要で漏れやすい。
  集約側（`execute_tool`）を採用する。
- proto の `principal` field を削除する方式は wire 破壊のため、
  opt-in 段階では採用しない（将来の段階移行で判断）。

#### テストの実装状況

- [config.rs](../../rust/jarvis-mcp-gw/src/config.rs):455-487 に
  `server_side_principal_guard_defaults_to_false` と
  `explicit_server_side_principal_guard_is_honoured` を追加済み
  （キー無し TOML は `false`、明示 `true` は保持）。
- [lib.rs](../../rust/jarvis-mcp-gw/src/lib.rs):601-616 に
  `server_side_principal_guard_overrides_caller_principal` を追加済み。
  `mcp_principal = "opencode"` の gateway へ `execute_tool(..., "jarvis")`
  を渡し、ON では成功することを確認する。
  :620 の `mcp_principal_stays_fixed_when_guard_is_enabled` で
  `/mcp` が固定 principal を維持することも担保する。
- 検証コマンド: `cd rust && cargo fmt --all --check`,
  `cargo test -p jarvis-mcp-gw -p jarvis-policy`,
  `cargo clippy --workspace -- -D warnings`, `make ci-local`。
  ドキュメント更新のみの本タスクでは実行していない（親が検証）。

#### 残る判断（要判断）

- **本番有効化**: `JARVIS_MCP_GW_REQUIRE_SERVER_SIDE_PRINCIPAL=true` の
  適用可否とタイミング。実機への env 注入（unit 更新・`bws run` 経路）
  と `deploy-mcp-gw-safe.sh --apply` を伴う。既定 `false` のままなら
  現行の caller principal 動作が継続する。
- **監査粒度**: ON 時は全呼び出しが同一 identity になり呼び出し者粒度が
  失われるため、申告値を `claimed_principal` として別途記録するかを
  決定する（認証 Identity 導出は P0-1 と同じ設計判断として別途）。
- **結論**: フラグの実装は完了。判断は上記2点に限定する。

## 2. バックアップ / DR・OCI レガシー整理（優先度: 高）

| ID | タスク | 現状・根拠 | 次のアクション（案） |
|---|---|---|---|
| BK-1 | バックアップ保存先の再決定と実装更新 | **2026-11-17 実装済み**: 保存先を `/var/lib/postgresql/jarvis-backups`（ローカル、postgres:postgres 0700）と決定。[backup-local.sh](../../infra/scripts/backup-local.sh) が PostgreSQL dump + Qdrant snapshot + 非 secret 設定 + SHA-256 manifest を生成。実バックアップ1回成功、[verify-backup.sh](../../infra/scripts/verify-backup.sh) で整合性・完全性を検証（合成テスト6件）。旧実装は OCI/rclone/op 前提のレガシーで、ランチャーは fail-closed 化済み。 | 保存先の外部化（NAS/別ホスト）・暗号化は要件が出た時点で別途対応。現在はローカル保存で ADR-0004 に準拠。 |
| BK-2 | バックアップ timer/service のローカル運用化 | **2026-11-17 repo 側完了**: unit を現行化（`User=postgres`、OCI/rclone/op 依存撤去、`ProtectSystem=strict` + `ReadWritePaths`）。`install-backup-timer.sh`（dry-run 既定・rollback 付き・適用後に手動実行＋整合性検証まで実施）を追加。`systemd-analyze verify` 成功。**実機は active / enabled（2026-11-19 ユーザー適用済み）**。ただし repo 側 unit 変更（`--repo-dir` 追加）の再適用は未。 | 「承認待ち実機タスクの実行手順」節の BK-2 に従い、確認 → 修正 → 再確認を実施。 |
| BK-3 | OpenTofu/OCI 後始末資産の扱い決定 | [infra/opentofu/](../../infra/opentofu/) は未使用レガシー（[infra/opentofu/README.md](../../infra/opentofu/README.md)）。後始末用に [infra/scripts/decommission-oci.sh](../../infra/scripts/decommission-oci.sh) あり（既定は dry-run、`--apply` で destroy 実行。`tofu`/OCI CLI 設定/`infra/opentofu/environments/prod` の init/state 等が前提。実行時は `tofu init`・`state list`・最終 state バックアップ（`$HOME/jarvis-oci-state-<ts>.json`）・`destroy` plan 作成を行う）。2026-11-15 確認: `infra/opentofu/environments/prod/.terraform/terraform.tfstate` がローカルに存在（内容は未確認）。 | 「保持」「削除」「OCI リソース削除実行」の方針決定。**実行判断/実行前の確認例（read-only・dry-run優先）**: (1) `tofu`/OCI CLI/認証情報・対象 tenancy/region・`terraform.tfvars` 等の前提を確認し、state の所在・鮮度・バックアップ（別保管含む）を確保する。(2) OCI コンソール/CLI で実リソース有無・保護すべきデータ（Object Storage/Block Volume 等）の残存を確認し、未追跡リソースや state 不整合がないか点検する（A1 Free 枠再取得不可等の影響も考慮）。(3) まず dry-run（既定）で `state list` と destroy plan の対象を確認し、出力を保存/レビューしてから `--apply` の要否を判断する。(4) 実削除時のロールバック困難性・Tailscale 管理画面の後処理等を手順化し、必要な承認/権限のもとで実施する（実削除/OCI 操作は別判断・手順）。 |
| BK-4 | レガシー設定テンプレートの整理 | 全6テンプレートを棚卸し（2026-11-17）。`policy/context/event/otel.env` は既に legacy 注記済み。`code_server.env` / `mcp_gateway.env` に残っていた「Populated by 1Password CLI at runtime」注記を、実機の `bws run` + `/etc/jarvis/bws.env` 前提へ訂正。値の変更・削除は行わず、secret scan もクリーン。 | ファイル単位の廃止/削除は実機設定の確定後に別判断（現状は参照テンプレートとして維持）。 |

**BK 関連の 2026-11-19 更新**: BK-1/BK-2 のローカル実装に加え、
`restore-local.sh` と `test-restore-local.sh`（47 tests pass）を追加した。
timer は実機で active / enabled だが、repo 側 unit 変更の再適用は未実施。
S9 の実機復元演習とバックアップ外部化（P1-4）は承認待ち。

### レガシー op:// テンプレートの判断記録（P3-5 一部、2026-11-18）

[infra/config/context.env](../../infra/config/context.env) /
[infra/config/event.env](../../infra/config/event.env) /
[infra/config/policy.env](../../infra/config/policy.env) /
[infra/config/otel.env](../../infra/config/otel.env) の4件について、
**値なし参照テンプレートとして tracked を維持する**と判断した記録。
秘密情報の値は記載せず、実機変更・テンプレート変更は行っていない。

#### 確認した事実（Runtime Proof）

- `git ls-files` で上記4件がすべて tracked。`op://` を含む env テンプレートも
  この4件のみ（他の `infra/config/*.env` は `op://` 参照を含まない）。
- 各ファイルの `op://` 行は参照プレースホルダであり、秘密情報の値そのものは
  含まない（参照先の実値は未確認）。
- [jarvis-context.service](../../infra/systemd/jarvis-context.service) /
  [jarvis-event.service](../../infra/systemd/jarvis-event.service) /
  [jarvis-policy.service](../../infra/systemd/jarvis-policy.service) /
  [jarvis-mcp-gw.service](../../infra/systemd/jarvis-mcp-gw.service) は
  `/etc/jarvis/bws.env` を読み込み `bws run` で起動。repo 内の
  `infra/config/*.env` は `EnvironmentFile` として参照していない。
- [otelcol.service](../../infra/systemd/otelcol.service) は `bws run` /
  `op run` を使わず設定ファイルを直接指定（SEC-1）。したがって
  `otel.env` の参照テンプレートも実機の起動経路では使われていない。
- 実機の secret 内容・供給元の詳細はこの記録では確認していない。

#### 判断

- **tracked 維持**: 値を含まない参照テンプレートを削除・ignore すると
  設定項目の更新履歴が失われる（P2-10 の判断と同一方針）。
- 実機の秘密情報は root 管理の `bws.env` に集約され、テンプレートは
  参照用として残す。テンプレート更新時はその都度レビューする。
- 破壊的な `rust/scripts/restore.rs` は
  [restore.sh](../../infra/scripts/restore.sh) が fail-closed（exit 2）で
  委譲を止め、`make safety-test` が回帰を担保（既存の BK/SEC 進捗記録と整合）。

### code-server の PASSWORD_FILE 扱い（2026-11-20 確認・記録のみ）

- [build-selfhost.sh](../../infra/scripts/build-selfhost.sh):459 は生成する
  unit から `Environment=PASSWORD_FILE=` 行を `sed` で削除する。
- 一方 [code-server.service](../../infra/systemd/code-server.service):33 と
  [code_server.env](../../infra/config/code_server.env):6 には `PASSWORD_FILE`
  が残り、テンプレート記述と実機の起動経路が食い違う。
- **訂正方針**: パスワードは `config.yaml` に集約し、デプロイ時に
  `PASSWORD_FILE` は除去する。unit / env テンプレートの記述もこの方針へ
  合わせる（テンプレート自身の編集は本タスクの担当外のため記録のみ）。

### BK/SEC 進捗（2026-11-16 追記）

### BK-2 進捗（2026-11-17）

- **BK-1/BK-2 のローカル実装完了**: `backup-local.sh` + 現行 unit + `install-backup-timer.sh`
  を追加。実バックアップ1回成功（PG dump/Qdrant snapshot/非 secret 設定/
  SHA-256 manifest、secret 不在・0700 を確認）。`systemd-analyze verify` 成功。
- **復旧 runbook を追加**: [backup-restore-runbook.md](backup-restore-runbook.md)（隔離先での非破壊検証手順）。
- **timer の実機有効化は 2026-11-19 完了**: ユーザーが
  `sudo -n bash infra/scripts/install-backup-timer.sh --apply` を実行し
  exit 0、active / enabled、手動バックアップ整合性検証 ok=30 fail=0
  （[backup-redesign-notes.md](backup-redesign-notes.md) 参照）。
  ただし repo 側 unit 変更の実機再適用は未実施（後述の実行手順を参照）。

- **レガシー backup.sh / restore.sh は fail-closed 化済み**: `--dry-run` でも
  ローカル削除が走る backup.rs / 破壊的な restore.rs への委譲を止め、
  実行時に理由と `backup-preflight.sh` への導線を表示（exit 2）。
  実装自体は `rust-script ... --help` で参照可能、`make lint` も成功。
- **レガシー deploy.sh も fail-closed 化済み**: 旧 `jarvis-host` 宛リモート
  デプロイ（OCI 時代の資産）への委譲を停止し、`deploy-mcp-gw-safe.sh`
  （dry-run 既定）へ誘導（exit 2）。`make deploy-dry` は直接実装を呼ぶため
  従来どおり動作。

- **BK-1/BK-2/SEC-2 の安全ゲート実装済み**: [backup-preflight.sh](../../infra/scripts/backup-preflight.sh)
  （`make backup-preflight`）で、サービス状態・ツール有無・設定ファイル列挙（secret 値は読まない）・
  `op://` テンプレート数・timer 状態を読み取り専用で検査。実機 run は fail=0 / warn=2。
- **しかし保存先・認証/暗号化方式・復旧検証手順は未決定**のため、バックアップ本体の
  再実装と timer 有効化は行っていない（[backup-redesign-notes.md](backup-redesign-notes.md) の
  「仍未解決（ブロッカー）」を維持）。OCI は ADR-0004 で除外。
- **BK-3**: OCI リソース削除は「ociは使わない」方針のもと実行しない（後始末スクリプトは参照用に保留）。
- **BK-4 / SEC-2**: `op://` 参照4ファイルと `bws.env` の扱いは、現行の秘密管理方式を
  確定した上で判断する（実機 secret の内容は確認していない）。

## 3. シークレット管理・構成ドリフト（優先度: 中〜高）

| ID | タスク | 現状・根拠 | 次のアクション（案） |
|---|---|---|---|
| SEC-1 | otelcol unit の repo/実機差分の整合 | **2026-11-16 解消（repo側）**: [infra/systemd/otelcol.service](../../infra/systemd/otelcol.service) を実機の稼働 unit と機能一致させた（`User/Group=koppe`、`op run`/`EnvironmentFile` 撤去、直接 `otelcol-contrib --config`）。functional diff で一致を確認。資格情報の供給元は未特定（secret 値は未確認）。 | 実機適用は不要（repo が実機に追従）。Grafana 認証の供給経路が判明した場合に unit 注記を更新する。 **0.0.0.0 バインドの監査記録は後述「SEC-1 関連: otelcol の 0.0.0.0 バインド監査」を参照。** |
| SEC-2 | バックアップ系の 1Password 依存解消 | **2026-11-17 実質解消**: 現行バックアップは [backup-local.sh](../../infra/scripts/backup-local.sh) で OCI/rclone/op 非依存、実バックアップ検証済み。レガシー [backup.rs](../../rust/scripts/backup.rs) / [restore.rs](../../rust/scripts/restore.rs) の `op inject` は残存するが、[backup.sh](../../infra/scripts/backup.sh) / [restore.sh](../../infra/scripts/restore.sh) は fail-closed 化され実機では呼ばれない（`make safety-test` が担保）。unit も `op.env` 参照を撤去済み。 | レガシー実装の `op inject` コード自体は参照用として残置。実機 secret 変更は不要（新バックアップは secret を読まない）。 |

| P2-10 | `.gitignore` 拡張の判断 | **採用しない**（詳細は後述）。 | 判断記録を追加。 |

### SEC-1 関連: otelcol の 0.0.0.0 バインド監査（P0-6、2026-11-18）

[infra/config/otelcol-config.yaml](../../infra/config/otelcol-config.yaml) と
[infra/systemd/otelcol.service](../../infra/systemd/otelcol.service) を読み取り専用で
監査した結果を記録する。実機の設定・リスニング状態は変更・確認していない。
秘密情報の値は記載せず、repo 側の config / unit も変更しない。

#### 現状（repo 側の設定）

- extensions: `health_check` が 13133、`pprof` が 1777、`zpages` が 55679 を
  いずれも 0.0.0.0 で待ち受け、3つとも `service.extensions` で有効。
- OTLP receiver: gRPC が 4317、HTTP が 4318 を 0.0.0.0 で待ち受ける。
  `service.extensions` に auth 拡張はなく、受信側の認証設定は見当たらない。
- 内部テレメトリ: `service.telemetry.metrics` の pull exporter が
  host 0.0.0.0 / port 8888 を使用（記載の4系統に加えて発見）。
- systemd unit: `User/Group=koppe`、`UMask=0077`、`NoNewPrivileges`、
  `ProtectSystem=strict`、`ProtectHome=yes`、`PrivateTmp=yes`、
  `ReadWritePaths=/etc/jarvis /var/log/jarvis` などのハードニングはある。
  一方 `IPAddressDeny=` / `IPAddressAllow=` / `RestrictAddressFamilies=` /
  `PrivateNetwork=` のようなネットワーク露出の制限指定はない。
- ExecStart は repo 内 config ではなく実機側の設定ファイルを参照するため、
  実機の実際のリスニング状態はこの記録からは確定できない。

#### 方針との矛盾

- [AGENTS.md](../../AGENTS.md) の Security Notes は、全サービスを `127.0.0.1`
  にバインドし外部アクセスは Tailscale 経由とする方針を規定している。
  上記の 0.0.0.0 バインドはこの方針と矛盾する。
- 同ファイルの Service Ports 表には OTLP 4317 / 4318 が載る一方、
  health / pprof / zpages / 内部テレメトリ 8888 は掲載がない。
  health 欄も「—」で、実際の公開範囲との乖離がある。

#### 対応方針（未実施・要判断）

- config のバインド先を loopback へ変更するか、unit 側でネットワーク制限を
  追加するかは判断が先。pprof / zpages を常時有効にするかも要判断。
- Tailscale 経由を前提に 0.0.0.0 を維持するなら、外部からの到達性を
  どこで制限するか（firewall / Tailscale 側）を明示する必要がある。
- 現時点では repo 側 config・unit とも変更せず、記録のみとする。

#### 対応結果（2026-11-18 追記・repo 側のみ）

上記は監査時点の記録。その後、repo 側テンプレートのみ次の対応を行った。

- [infra/config/otelcol-config.yaml](../../infra/config/otelcol-config.yaml) の
  待ち受け6箇所を `127.0.0.1` へ変更した: health_check 13133、pprof 1777、
  zpages 55679、OTLP gRPC 4317、OTLP HTTP 4318、内部テレメトリ
  `service.telemetry.metrics` の prometheus host 8888。
  [AGENTS.md](../../AGENTS.md) の loopback 方針に整合する。
- `prometheusremotewrite` の Grafana Cloud 向け endpoint と認証参照は
  **変更していない**（送信先であり待ち受けではない）。秘密情報の値は
  記録していない。
- **実機は未変更**: 稼働 unit の ExecStart は
  `/etc/jarvis/otelcol-config.yaml`（別パス）を参照するため、この repo 側
  変更は動作中の collector に直接影響しない。実機反映は別途、設定同期と
  再起動・リスニング確認が必要（要判断・要操作）。
- unit 側のネットワーク制限（`IPAddressDeny` 等）や pprof / zpages の
  常時有効化の要否は、上記「対応方針」のとおり引き続き要判断。

### P0-7 Grafana 認証情報の供給経路（判断材料、2026-11-18）

[todo.md](../../todo.md) の P0-7 について、秘密情報の値を一切見ずに
repo 側の参照関係を調査した結果を記録する。**結論: 要判断**。
実装・実機変更・再起動は行っていない。

#### 現状（repo 側の参照関係 / P0-7）

- [otelcol-config.yaml](../../infra/config/otelcol-config.yaml):130 の
  `prometheusremotewrite` が Basic 認証ヘッダで
  `${env:GRAFANA_CLOUD_AUTH}` を参照（送信先 URL と値は記録しない）。
- [otelcol.service](../../infra/systemd/otelcol.service):12-18 は
  `EnvironmentFile` も `bws run` も `/usr/bin/op run` もなし。
  `ExecStart` は `otelcol-contrib --config` の直接起動のみ。
- 他の4サービス（policy / mcp-gw / event / context）は root 管理の
  `bws.env` を `EnvironmentFile` で読み込み `bws run` 経由で起動。
  otelcol だけがこの経路に参加していない。
- [otel.env](../../infra/config/otel.env) は値なしの `op://` テンプレートで、
  参照名は `GRAFANA_CLOUD_TOKEN` / `GRAFANA_CLOUD_INSTANCE`。
  設定が要求する `GRAFANA_CLOUD_AUTH` とは名前が一致しない。
- [build-selfhost.sh](../../infra/scripts/build-selfhost.sh):683 は旧
  `op run --env-file=/etc/jarvis/otel.env` 起動接頭辞を除去して unit を生成する。
  `op run` 経路は既に意図的に外されている。
- [AGENTS.md](../../AGENTS.md) の Configuration にも、otelcol は直接起動で
  Grafana 認証の供給経路は未特定、との記載がある。

#### 3案の比較（P0-7）

- **A. bws run 経由で注入**: 値を見ない確認は bws 側に secret を置き、
  環境変数の存在のみを終了コード等で検証する（値は出力しない）。
  再起動は unit 編集 → daemon-reload → restart → 送信成功をログ確認。
  リスク: bws トークン依存が1プロセス増えるが、他4サービスと同じ
  既知経路。secret 名と Basic 形式の整合を先に決める必要がある。
- **B. root 管理の EnvironmentFile**: 0600 の root 所有ファイルを unit の
  `EnvironmentFile` で読み込む。値は repo へ置かず、バックアップ対象外に
  する。再起動は同様。リスク: 平文がディスクに残り、
  `systemctl show` 等で値が露出しうる（確認時は値を出さない操作が必要）。
- **C. ファイルベース secret**: confmap の file provider で参照先ファイルを
  指定し、config / env のどちらにも値を置かない。再起動には config と
  起動オプションの変更が必要。リスク: 対応バージョンと有効化手順の確認、
  ローテーション時の再読込挙動、otelcol 実行ユーザーへの読取権限付与。

#### 判断（P0-7）

- 現状は `GRAFANA_CLOUD_AUTH` がどの経路からも注入されておらず、
  exporter が参照する env が未設定（実機の値・ログは未確認）。
- A / B / C の採否は、既存の bws 運用を踏襲するか、平文ファイルを
  許容するか、collector 側の機能に依存するかという方針次第。
  **判断は確定させず要判断とする**。決定後に unit 注記と手順を更新する。

| P1-12 | Webhook 4ルートの認証設計 | **未対応・要判断**。 | 詳細は後述の設計記録。 |

### P2-10 .gitignore と infra/config/*.env の判断記録（2026-11-18）

[todo.md](../../todo.md) の P2-10（R1-SEC-005）「`.gitignore` を
`infra/config/*.env` へ拡張」について、**採用しない**と判断した記録。
[.gitignore](../../.gitignore) およびテンプレートファイルは変更していない。

#### 確認した事実

- [.gitignore](../../.gitignore):10-11 の `config/*.env` / `config/*.env.*` は
  リポジトリ直下の `config/` にしか一致せず、`infra/config/*.env` は対象外。
  `git ls-files` で6個の `infra/config/*.env` が tracked であることを確認。
- 同ファイル:48 は `infra/config/bws.env` を明示的に ignore。
  `git check-ignore -v` で一致を確認（実機ファイルの値は未確認）。
- [infra/config/bws.env.example](../../infra/config/bws.env.example) は ignore
  されず tracked（テンプレートとして残す）。
- 6個のテンプレートは接続先・パス・`op://` 参照プレースホルダ等のみで、
  秘密情報の値そのものは含まない。
- 実機の秘密情報は repo 外の root 管理ファイルに置かれる
  （[AGENTS.md](../../AGENTS.md)）。

#### 判断と理由

- **採用しない**: ignore すると参照テンプレートの更新が git で追えなくなり、
  構成ドリフト（BK-4 参照）を招く。値を含まないテンプレートを ignore する
  利点はない。
- 実機の秘密情報は別経路で既に ignore 対象にあり、repo 内テンプレートを
  ignore する必要性はない。
- 将来テンプレートへ秘密値が混入した場合は、そのファイルを個別に
  ignore 対象へ移す等の対応を判断する（全体への blanket ignore は避ける）。

### P1-12 Webhook 認証の設計記録（2026-11-18）

[todo.md](../../todo.md) の P1-12（A-M3）について、現状確認と方針を記録する。
実装・設定変更は行っていない。秘密情報の値は記載しない。

#### 現状（repo 側の読み取り確認）

- [normalizer.rs](../../rust/jarvis-event/src/normalizer.rs):141-147 で
  `/webhook/ha` / `/webhook/android` / `/webhook/gmail` /
  `/webhook/calendar` の4ルートを登録。各ハンドラは JSON ペイロードを
  `UnifiedEvent` へ変換して `emit` するのみで、認証ミドルウェアや
  署名検証はない。
- [config.rs](../../rust/jarvis-event/src/config.rs):50 の webhook 待ち受け先は
  `127.0.0.1:8085`（既定）。[AGENTS.md](../../AGENTS.md) の方針どおり
  loopback bind + Tailscale 経由の信頼境界を前提としている。
- `ha_token` は Home Assistant 側への WebSocket 接続の認証に使われ、
  webhook 受信側の認証には使われていない
  （[sources.rs](../../rust/jarvis-event/src/sources.rs):41）。

#### 設計方針（未実装・要判断）

- 現状の信頼境界（loopback bind + Tailscale）を前提に、当面は
  共有トークン / HMAC 署名の導入は行わない。
- 導入の要否は**実機の公開範囲が決定した後**に判断する。Tailscale 外へ
  公開する、リバースプロキシ経由で外部サービスから受け取る等の変更が
  発生した場合は、認証・署名検証とレート制限を再評価する。
- 実機のリスニング状態・公開範囲はこの記録では確認していない
  （repo 側の既定値とコードからの確認）。

### Cedar schema の整備状況（2026-11-16、ユーザー指定項目）

[infra/config/policy/cedar_schema.cedarschema](../../infra/config/policy/cedar_schema.cedarschema)
の :4-8 は起動時 Strict 検証を明記しており、以下を実施済み:

- namespace を braces 形式へ修正し schema 自体が parse 可能に。
- entity 属性を optional 化し、`ApprovalRequest` entity を追加。
- `shipped_cedar_schema_parses` と `shipped_policies_validate_against_shipped_schema`
  の2テストを追加（後者は `cedar_policy::Validator` の strict mode で
  `policies.cedar` が schema に適合することを検証）。

**実施済み**: 起動時に `cedar_policy::Validator` の strict mode で
`policies.cedar` が schema に適合することを検証する
（[policy.rs](../../rust/jarvis-policy/src/policy.rs):27-35, 140）。

**現状（2026-11-20 更新）**: schema は informational ではない。直上のとおり
起動時に Strict 検証が強制され、エラー時は fail-closed で起動失敗となる。
さらに gateway は caller の tool 引数を直接渡さず、
[lib.rs](../../rust/jarvis-mcp-gw/src/lib.rs):20-41 の `policy_context` が
server 管理の `tool` / `principal` / `resource` / `requires_approval` のみを
渡すため、caller が ABAC 属性を偽装できない。引数値自体の runtime schema
validation は未実装だが、それは schema が補助情報であることを意味しない。

### 承認フロー堅牢化（2026-11-17 追加）

- `jarvis-policy` の承認キューに保留数上限（`JARVIS_POLICY_APPROVAL_MAX_PENDING`、
  既定100・後方互換）と重複 ID 拒否を追加。loopback の無認証エンドポイント
  に対するリソース枯渇・保留ハイジャックを防ぐ（テスト追加）。
- `jarvis-mcp-gw` は承認後に policy を再チェック（kill switch の承認中発動で
  fail-closed）、allowlist の `max_args_size` を実効化、ツール入力スキーマを
  `tools/list` で公開。
- 旧 backup/restore/deploy ランチャーは fail-closed 化し、`make safety-test`
  で回帰検証（ci-local に組込）。

### MCP-1 承認フローのセキュリティ境界（2026-11-16 レビュー、2026-11-19 更新）

- jarvis-policy の承認 API（`/v1/approval`, `/v1/approvals`,
  `/v1/approval/{id}/approve|deny`）は既定では認証トークンを要求しない
  （後方互換）。**オプションのトークン認証**では scope が分かれる。
  - API scope（request/list/status）: `JARVIS_POLICY_APPROVAL_API_TOKEN`。
  - admin scope（approve/deny）: `JARVIS_POLICY_APPROVAL_ADMIN_TOKEN`。
    未設定なら API トークンへフォールバック（職務分離は効かない）。
  - 両 scope ともヘッダ名は `X-Jarvis-Approval-Token`。未設定/不正は 401。
  - 旧 `JARVIS_POLICY_APPROVAL_TOKEN` は policy 側の実変数ではない。UI は
    旧名を一時フォールバックとして受け付け、起動時に非推奨警告を出す。
  - gateway は `JARVIS_MCP_GW_APPROVAL_API_TOKEN`、UI は
    `JARVIS_POLICY_APPROVAL_API_TOKEN` / `JARVIS_POLICY_APPROVAL_ADMIN_TOKEN`
    を読み、対応する scope へ同じ値を送信する。health/metrics/check/
    kill-switch は認証対象外。ループバック外へ公開する場合はトークン設定が必須。
- `jarvis-mcp-gw` の承認 HTTP クライアントには 330 秒の overall timeout と
  2 秒の connect timeout を設定済み（2026-11-16 修正）。
  policy が応答を返さない場合も MCP 呼び出しは有限時間で fail-closed となる
  （回帰テスト `hung_policy_endpoint_times_out_and_denies` 追加）。

### CI 停止の原因（2026-11-16 実機確認・重要）

`master` への push  triggered CI は 2026-11-15 05:22 UTC 以降 **全件 failure**
（job は 1〜3 秒で steps なしで終了）。GitHub Checks API の annotation で
原因を特定した:

> "The job was not started because recent account payments have failed or
> your spending limit needs to be increased. Please check the 'Billing & plans'
> section in your settings"

つまり **コード・ワークフロー設定の不具合ではなく、GitHub アカウントの
支払い/ spending limit の問題**。再 run（attempt 2）でも同一だった。
最後の成功 run は 2026-11-15 02:38 UTC（`43668759909`）。
解決にはユーザーによる GitHub の Billing & plans での対応が必要
（エージェント権限外）。CI 復旧後は最新 push の gates 結果を再確認する。

CI が動かない間は `make ci-local`（後述）で同一ゲートをローカル実行できる。

## 4. CI/CD・レビュー基盤（優先度: 中）

| ID | タスク | 現状・根拠 | 次のアクション（案） |
|---|---|---|---|
| CI-1 | review-gate の Actions 配線方針決定 | **2026-11-17 追記（安全な部分配線）**: [手動 dispatch 限定の review-gate workflow](../../.github/workflows/review-gate.yml) を追加。完全な3ステージ定義（`ci/review-gate.yml`）は auto-merge と PR 自動実行を含むため、認証・権限方針が確定するまで未配線のまま参照用とする。新 workflow は `make ci-local`（Rust/UI/secret scan/safety guards）のみを実行し、merge / comment / 状態変更は一切行わない。 | 完全自動化（PR トリガー・cross-review・auto-merge）は OpenCode 認証情報とレビュー方針の決定後に別途対応。 **配線前に必要な修正一覧は後述「CI-1 配線前に必要な修正一覧」を参照。** |
| CI-2 | `opencode run` 依存の置換検討 | [adr/0002-runtime-agnostic.md](../../adr/0002-runtime-agnostic.md) はランタイム非依存化を規定（2026-11-17: accepted）。2026-11-15 確認: `opencode run` 依存は [ci/review-gate.yml](../../ci/review-gate.yml)（gate-check/cross-review/merge-gate/metrics の4箇所）と [AGENTS.md](../../AGENTS.md) の手動実行手順、[adr/0001-unified-architecture.md](../../adr/0001-unified-architecture.md) の非対話実行記述に残る（review-gate は CI 未配線）。 | **2026-11-16 実装済み（repo側）**: 4箇所の `opencode run` を [ci/scripts/review-run.sh](../../ci/scripts/review-run.sh) へ集約。`JARVIS_REVIEW_RUNNER`（既定 `opencode`）でバックエンド切替可能、runner 契約は `run <prompt> [--json]`。スタブによる4契約テスト（`bash ci/scripts/test-review-run.sh`）成功。代替ランタイムの選定・認証情報設定は残課題（ADR-0002 の段階移行方針と整合）。 |

### CI-1 配線前に必要な修正一覧（2026-11-18 監査）

監査対象は未配線の参照実装 [ci/review-gate.yml](../../ci/review-gate.yml) のみ。
[安全版 workflow](../../.github/workflows/review-gate.yml) は手動 dispatch 限定で
`make ci-local` のみを実行し、merge / comment / 状態変更を行わない。
安全版は現状のまま維持し、参照実装と混同しないこと。設定値・認証情報の値は
この一覧には記載しない。

#### permissions

- トップレベルの `permissions` は `contents: read`、`pull-requests: write`。
  Checks API を操作する step はなく `checks: write` は未使用だったため、
  2026-11-18 に参照実装から削除済み（判断を伴わない修正）。
- `concurrency` は 2026-11-18 に PR 単位へ追加済み（安全版と同じ/
  `cancel-in-progress: true`）。未配線の参照実装のため実機への影響はない。
- `gh pr comment` を行う gate-check / cross-review は `pull-requests: write`
  を要求する。コメントを残すかどうかを方針決定し、残さない場合は
  `contents: read` のみへ戻す（安全版と同じ最小権限）。
- auto-merge ジョブの `gh pr merge --auto --squash --delete-branch` は
  `contents: write` を要求するが、現状は `contents: read` のみで権限不足。
  昇格が必要なら対象ジョブに限定して付与する。
- fork からの `pull_request` では既定トークンが読み取り専用になり、
  comment / merge が失敗する。トリガー対象（同一リポジトリのみ等）を決定する。

#### OpenCode 依存

- gate-check / cross-review / merge-gate / metrics の4ジョブが、
  バージョン固定・チェックサム検証なしのリモートインストールスクリプトを
  パイプ実行して OpenCode CLI を導入している（supply chain リスク）。
- [ci/scripts/review-run.sh](../../ci/scripts/review-run.sh) の
  `JARVIS_REVIEW_RUNNER` で runner を切替できるが、CLI 導入 step は無条件。
  runner が OpenCode 以外のときは導入をスキップするか、
  [ADR-0002](../../adr/0002-runtime-agnostic.md) に沿って依存を撤去する。
- OpenCode のセッション権限を人手の承認なしで自動受け付けする設定が
  環境変数に定義されている。配線前に承認方針を決定し、自動受け付けを
  無効化するか jarvis-policy の承認ゲートへ委譲する。
- モデルプロバイダ等の認証情報の供給経路が workflow にない。
  CI 上でエージェントを動かす場合の秘密情報の扱いを決定する（CI-2 と同一）。
- secret scan / 依存グラフ等の追加ツールも `latest` の URL や
  グローバル npm インストールで取得しており、バージョン固定が必要。

#### auto-merge

- `auto-merge` ジョブは全ステージ成功時にマージを自動有効化し、
  人間の承認を経ずにマージしうる。review-flow の承認ゲート方針との
  整合性判断が先で、決定までジョブを配線しない。
- `workflow_dispatch` では入力 PR 番号を指定できるため、任意の PR を
  auto-merge 対象にしうる。実行者と対象 PR の制限を追加する。
- branch protection / 必須レビュー / 承認済みであることの明示的な確認が
  マージ前にない。有効化するなら検証 step を追加する。
- `concurrency` は 2026-11-18 に PR 単位へ追加済み（前述「permissions」参照）。
  安全版と同じ `cancel-in-progress: true` で同一 PR の再実行や複数 trigger の
  競合を抑止する。配線時も同等設定を維持する。

**判定**: 上記の権限・認証情報・自動承認・auto-merge の方針が確定するまで、
参照実装は未配線の参照用として維持し、安全版を配線経路とする。

## 5. アーキテクチャ/ドキュメント整合性（優先度: 中）

| ID | タスク | 現状・根拠 | 次のアクション（案） |
|---|---|---|---|
| DOC-1 | ADR-0001 未チェック項目の棚卸し・更新 | [adr/0001-unified-architecture.md](../../adr/0001-unified-architecture.md) に未チェック項目が残る（OCI A1+Tailscale 疎通、1Password CLI 設定、同一ワークスペース共有確認、Grafana Cloud アラート、Idle Compute 対策、review-flow CI ゲート統合）。OCI 前提項目は ADR-0004 により陳腐化。2026-11-15 時点の再分類メモ（ADR本文の状態変更は別判断）: **非適用候補（ADR-0004）**= Phase 0「OCI A1+Tailscale 疎通」、Phase 4「Idle Compute 対策」 / **継続・要再評価**= Phase 0「1Password CLI 設定・秘密情報注入確認」（現行 `bws` 利用、`otelcol` 等は SEC-1 で実機整合確認）、Phase 1「同一ワークスペース共有確認」（ローカル運用での現行要件・実態確認が必要）、Phase 4「Grafana Cloud アラート」（方針/設定確認）、Phase 4「review-flow CI ゲート統合」（CI-1/CI-2） / **備考**= Phase 4「rclone バックアップ」は成果物チェック済みだが OCI Object Storage 前提・現行未運用の注記あり（BK-1/BK-2/SEC-2）。 | ADR-0001 本文のチェック状態・注記を上記分類に沿って更新するか、必要に応じて補助ADR/個別タスクへ分離（実態確認・設定変更が必要な項目は別途証跡を取る）。**本文更新時の方針（案）**: (1) `[x]` は実態確認済みの証跡がある項目に限定し、未確認は `[ ]` のまま根拠/日付付き注記を残す。(2) ADR-0004 により前提が消えた項目は「非適用（理由・参照ADR）」と明記し、完了扱いにしない。(3) `bws`/`op` 混在・CI未配線等の進行中/要再評価項目は関連タスク（SEC-1, CI-1/CI-2, BK-1 等）へリンクする。(4) ADR本文の決定内容・ステータス変更が伴う場合は別判断とし、必要なら追補ADR/注記の粒度を分ける。 |
| DOC-2 | 参照ドキュメントの未解決事項の要否判断 | `docs/architecture/mpe-rust-latex-architecture-report.md` §12 / `docs/architecture/opencode-v2-oci-code-server.md` §未解決事項 に未解決事項があるが、いずれも提案/参照時点の検証・設計選択事項（MPE実機検証、旧OCI構成前提の選択肢等）であり、現行 JARVIS ローカル運用のアクティブタスクとは別。 | **保留（参照資料扱い）**: 該当テーマを実施する場合に個別タスク化する |

## 6. 運用上のフォローアップ（優先度: 低〜中）

| ID | タスク | 現状・根拠 | 次のアクション（案） |
|---|---|---|---|
| OPS-1 | MCP クライアント再接続の運用明文化 | **2026-11-17 解消（調査）**: 実機の DSH `@deepseek-ai/dsh-mcp-client` は `reconnect` 設定をサポートし、未指定でも既定で `enabled=true` / `maxAttempts=10` / 500ms〜30s の exponential backoff で自動再接続する（`connection.ts` の `RECONNECT_DEFAULTS` を実機確認）。gateway 再起動後の `Unauthorized: Session not found` は一時的な接続断として扱われ、再 `initialize` 込みで復旧する想定。設定変更は不要。 | 現状の `cordis.patch.yml` の `failOnStartupError: false` を維持。再接続を明示したい場合のみ `reconnect: { enabled: true, maxAttempts: 10 }` を追加可能だが既定と同一。 |

## 7. データ整合性・マイグレーション（P1-2 / P1-3、2026-11-18）

[todo.md](../../todo.md) の P1-2 / P1-3 について、実装を読み取り専用で確認した
判断材料を記録する。**判断は未確定（要判断）**で、実装変更・実機操作は
行っていない。秘密情報の値は記載しない。

### P1-2 PG / Qdrant 二段書き込みの補償（要判断）

#### 現状（Runtime Proof / P1-2）

- [builder.rs](../../rust/jarvis-context/src/builder.rs):89-111
  の `store_memory` は PostgreSQL へ
  `INSERT ... ON CONFLICT (id) DO UPDATE` の後に Qdrant へ `upsert_vector`
  を実行する（PG 先行・Qdrant 後続）。
- [builder.rs](../../rust/jarvis-context/src/builder.rs):255-272
  の `upsert_vector` は `upsert_points` の失敗を `ContextError::Qdrant`
  へ変換して返すだけで、リトライ・永続キュー・補償トランザクションはない。
- PG は同じ id の再送なら `ON CONFLICT` で content / metadata / updated_at が
  更新される。ただし Qdrant 失敗を検知して再投下する仕組みはなく、
  再送が来なければ埋め込みだけが欠落したままになる。
- [builder.rs](../../rust/jarvis-context/src/builder.rs):57-87 / 113-156 の
  `build_context` は `search_memory` で Qdrant を検索し、得た id で PG を
  取得して relevance 順に再構成する（P1-1）。埋め込みが欠けた memory は
  ベクトル検索から漏れ、直近履歴（`get_recent_memories`）には残る。
- [001_initial.sql](../../rust/migrations/001_initial.sql) に outbox / 失敗キュー
  相当のテーブルはなく、PG 側に embedding カラムもない。再埋め込みには
  埋め込みエンドポイントが必要（未設定時はベクトル検索をスキップする動作）。

#### 選択肢の比較（P1-2 / 要判断）

- **A. outbox テーブル + ワーカー**: 実装コスト高（新規テーブル・状態遷移・
  リトライ・ワーカーが必要）。データ喪失リスク低（PG と同じ tx で
  未送信を残せる）。既存データ互換は中（既存行のバックフィルが必要）。
- **B. 起動時整合性チェック + 再埋め込み**: コスト中（PG と Qdrant の id を
  突き合わせ、欠落分を再埋め込み）。喪失リスク低〜中（起動時のみの検出で
  埋め込みエンドポイント必須）。互換性高（スキーマ変更なし）。
- **C. 現状維持の明文化**: コスト低。喪失リスク中（Qdrant 失敗分は再送が
  無い限り欠落）。互換性高（変更なし）。受容するなら ADR 化と監視が要る。

**判断**: 保留（要判断）。許容する喪失ウィンドウと埋め込みエンドポイントの
有無を確定してから選択する。実機データの整合状態は未確認。

### P1-3 マイグレーション版管理（要判断）

#### 現状（Runtime Proof / P1-3）

- [builder.rs](../../rust/jarvis-context/src/builder.rs):207-224 の
  `create_pg_pool` が起動ごとに `batch_execute` で `001_initial.sql` を実行。
  DDL は全て `CREATE TABLE / INDEX IF NOT EXISTS`。
- [001_initial.sql](../../rust/migrations/001_initial.sql):67 の kill switch
  初期行のみ `INSERT ... ON CONFLICT DO NOTH`。
- sqlx は workspace に存在し `migrate` feature も有効
  （[Cargo.toml](../../rust/Cargo.toml):73）。ただし `sqlx::migrate!` の
  呼び出しや `_sqlx_migrations` を触るコードはなく、適用履歴は未管理。
- `rust/migrations/` は現行 001〜005 の 5 版（`001_initial.sql`、
  `002_pending_embeddings.sql`、`003_audit_decided_by.sql`、
  `004_pending_embeddings_retry.sql`、
  `005_pending_embeddings_dead_letter.sql`）が存在する。
- migration を追加したら jarvis-policy / jarvis-context / jarvis-migrations
  各 unit の範囲コメントとこの Runbook を同一コミットで更新すること。

#### 選択肢の比較（P1-3 / 要判断）

- **A. sqlx migrate へ移行**: コスト中（既存 DB に履歴テーブルが無いため
  001 を baseline 登録する初回手順が必要）。喪失リスク低（順序と適用履歴が
  管理される）。既存データ互換は要検証（baseline 登録を誤ると未適用／
  二重適用の乖離がありうる）。
- **B. デプロイ時 CLI へ分離**: コスト中（migrate サブコマンドと手順を追加し
  起動時 DDL を廃止）。喪失リスク低（適用タイミングを明示制御）。互換性高
  （SQL は流用可能）。ただし適用忘れの運用リスクがある。
- **C. 現状維持の明文化**: コスト低。`IF NOT EXISTS` のため再実行は安全だが、
  既存テーブルへの `ALTER` が入った時点で適用済み判定が不能になる。
  喪失リスク低〜中、互換性高（変更なし）。

**判断**: 保留（要判断）。既存 DB の有無と baseline 登録方針、および
デプロイ手順の実行主体を確定してから選択する。実機 DB は未確認。

## 承認待ち実機タスクの実行手順（確認 → 修正 → 再確認）

ここに、ユーザーが承認すれば実行できる実機タスクの手順を一元化する。
エージェントはこれらの操作を実行しない。
**値は一切記載しない**。必要な変数は利用者が `export VAR=...` 形式で設定し、
終了コードは `$?` で確認する。権限昇格が必要なコマンドは `sudo` を明示する。
各ブロックは「確認 → 修正 → 再確認」の順で、コピー&ペーストで実行できる。

### 共通前提（Kubuntu ローカル / repo ルート・読み取り専用）

```bash
#!/usr/bin/env bash
set -euo pipefail

# 必要な変数は利用者が export VAR=... で設定する（値はこの文書に書かない）
: "${JARVIS_REPO:?repo ルートを JARVIS_REPO に設定してから実行}"

echo "== 実行環境: Kubuntu ローカル / repo ルート =="
cd "$JARVIS_REPO"

echo "== 確認: 作業ツリー =="
git status --short

echo "== 確認: ローカルゲート（CI 停止時の代替）=="
rc=0
make ci-local || rc=$?
echo "ci-local exit: $rc"
[ "$rc" -eq 0 ]

echo "== 確認: 秘密スキャン =="
rc=0
make secret-scan || rc=$?
echo "secret-scan exit: $rc"
[ "$rc" -eq 0 ]
```

### BK-2: timer unit の再適用（Kubuntu ローカル → 実機 systemd）

repo 側 unit は `--repo-dir` 追加と `ProtectHome=read-only` 変更済み。
実機への再適用は root 必須で未実施（配置・有効化は 2026-11-19 済み）。

```bash
#!/usr/bin/env bash
set -euo pipefail

: "${JARVIS_REPO:?repo ルートを JARVIS_REPO に設定してから実行}"

echo "== 実行環境: Kubuntu ローカル（実機 systemd を root で操作）=="
cd "$JARVIS_REPO"

echo "== 確認: 現状の timer 状態 =="
rc=0
make timer-status || rc=$?
echo "timer-status exit: $rc"

echo "== 確認: 配置済み unit の ExecStart =="
systemctl cat jarvis-backup.service || true

echo "== 修正: 修正済み unit の再適用（root 必須・daemon-reload/再起動を含む）=="
sudo bash infra/scripts/install-backup-timer.sh --apply

echo "== 再確認: timer 状態 =="
make timer-status
systemctl is-active jarvis-backup.timer
systemctl is-enabled jarvis-backup.timer

echo "== 再確認: バックアップ整合性（読み取り専用）=="
rc=0
make verify-backup || rc=$?
echo "verify-backup exit: $rc"
[ "$rc" -eq 0 ]
```

### S9: 実復元演習（隔離先・本番リソース非接触）

自動テストは 47 tests pass 済み。実機の隔離先復元演習はユーザー承認待ち。

```bash
#!/usr/bin/env bash
set -euo pipefail

: "${JARVIS_REPO:?repo ルートを JARVIS_REPO に設定してから実行}"

echo "== 実行環境: Kubuntu ローカル / 隔離先 =="
cd "$JARVIS_REPO"

echo "== 確認: 自動テスト（合成フィクスチャ・47 tests）=="
bash infra/scripts/test-restore-local.sh

echo "== 確認: バックアップ整合性（読み取り専用）=="
rc=0
make verify-backup || rc=$?
echo "verify-backup exit: $rc"
[ "$rc" -eq 0 ]

echo "== 修正: 隔離先へ復元（dry-run が既定、--apply で隔離先のみ上書き）=="
bash infra/scripts/restore-local.sh --latest
bash infra/scripts/restore-local.sh --latest --apply

echo "== 再確認: 隔離先の健全性（件数のみ・値は設定に依存）=="
: "${QDRANT_URL:?Qdrant の URL を設定}"
: "${RESTORE_COLLECTION:?隔離コレクション名を設定}"
curl -fsS "$QDRANT_URL/collections/$RESTORE_COLLECTION" | jq '.result.points_count'

echo "== 本番反映は別途承認・チェックリスト満たす場合のみ =="
# 本番上書きは破壊的。以下をすべて満たした場合のみ手動で実行する。
# - make verify-backup が fail=0
# - 隔離先での復元検証が成功
# - 関連サービスの停止予定共有・旧データの退避先確保
# - bws.env の再配置方法を確認
# bash infra/scripts/restore-local.sh --latest --apply --confirm
```

### MIG: jarvis-migrations.service の配置・有効化・検証

`jarvis-migrations.service`（`Type=oneshot` / `RemainAfterExit=yes`）は共有
migration を実行する unit で、policy / context が `After=` + `Wants=` で
引く（所有権の詳細は [todo.md](../../todo.md) の Round 30 を参照）。
通常デプロイの
[build-selfhost.sh](../../infra/scripts/build-selfhost.sh) で配置され、
[install-backup-timer.sh](../../infra/scripts/install-backup-timer.sh) の
`--apply` でも同じ unit が配置される（内容は同一）。`Wants=` 経由で起動する
ため個別 `enable` は不要。`ExecStart` は `/usr/local/bin/cargo sqlx migrate run`
を使うため、service の PATH に `cargo-sqlx`（sqlx-cli）が無いと unit は
`failed` になる。開発環境では `cargo sqlx --version` が
`no such command: sqlx` を返すことを確認済み（未導入の根拠）。

#### systemd `Wants=` の意味（2026-11-19 訂正・I36-A）

**訂正**: `Wants=` は弱い依存であり、対象 unit が `failed` になっても
依存元（policy / context）の起動は失敗しない（fail-open）。従来の
「`Wants=` の unit が failed なら依存元も起動失敗」という記述は誤り。
health check（例: `curl http://127.0.0.1:8081/health`）が 200 でも
migration 成功は証明されない。必ず
`systemctl is-active jarvis-migrations`（`active (exited)` を期待）と
`journalctl -u jarvis-migrations -n 50 --no-pager` で個別に確認する。

なお I35-A の方針どおり context は `sqlx::migrate!()` で起動時に自ら
スキーマを完了させるため、pre-flight の失敗が直ちに「未適用」を意味するとは
限らない。それでも失敗を放置せず原因を確認する運用とする。

**真に fail-closed にする選択肢（今回は採用せず、記録のみ）**:

- **Requires= + After= を依存元へ追加**: policy / context の unit に
  `Requires=jarvis-migrations.service` と `After=jarvis-migrations.service`
  を宣言すると、migration が起動失敗／停止した場合に依存元も起動しない
  （`After=` 単独は順序のみで失敗を伝播しない）。実行中に後から migration が
  failed になった場合の自動停止までは保証しない点に注意。
- **ExecStartPre の CHECK を依存元へ追加**: policy / context の `ExecStartPre=`
  で migration の完了状態（`active (exited)` または `_sqlx_migrations` の
  適用履歴）を検査し、不充足なら起動を中止する。既存 unit の `ExecStart=` や
  hardening を変えずに fail-closed を実現できるが、検査条件の設計と保守が
  必要になる。

今回は記述訂正に留め、3 unit の `After=` / `Wants=` / `ExecStart=` /
hardening は変更しない。

#### sqlx-cli の導入方法の比較（既存依存を活かす観点）

- **A. feature を絞った `cargo install` でロック済み依存を再利用（採用）**:
  workspace は既に `sqlx = "0.8"`（`postgres` / `migrate` feature 有効）を
  [Cargo.toml](../../rust/Cargo.toml) と [Cargo.lock](../../rust/Cargo.lock) で
  管理済み。`--no-default-features --features postgres` で不要な mysql /
  sqlite ドライバを入れず、`--locked` で sqlx-cli 側のロック済み依存を使う。
  既存の Rust toolchain と sqlx 0.8 系統を活かし、新規の外部成果物を増やさない。
- **B. 既定 feature の `cargo install sqlx-cli`**: 使わない mysql / sqlite まで
  ビルドして依存が増え、ロックも無視されるため不採用。
- **C. ディストリのパッケージ / ビルド済みバイナリを配布**: Kubuntu の標準
  repo に sqlx-cli は無く、外部成果物の取得・検証経路が増える。既存の
  Cargo 依存を活かせないため不採用。
- **D. context の `sqlx::migrate!()` を唯一の主体にして unit を廃止**:
  unit を残して配線を直す本タスクのスコープ外（[todo.md](../../todo.md) の
  Round 30 に代替案として記録済み）。

**採用**: A。下記ブロックの `cargo install ... --root /usr/local` 1行を root
で実行する（値・秘密情報なし）。`--root /usr/local` は unit が参照する
`/usr/local/bin/cargo` と同じディレクトリへ `cargo-sqlx` を置くため。

#### 確認 → 修正 → 再確認

```bash
#!/usr/bin/env bash
set -euo pipefail

: "${JARVIS_REPO:?repo ルートを JARVIS_REPO に設定してから実行}"

echo "== 実行環境: Kubuntu ローカル / 実機 systemd を操作 =="
cd "$JARVIS_REPO"

echo "== 確認: sqlx-cli（cargo-sqlx）の有無（未導入なら no such command）=="
command -v cargo-sqlx || true

echo "== 確認: デプロイ経路に unit が含まれるか =="
grep -n 'jarvis-migrations.service' infra/scripts/build-selfhost.sh

echo "== 確認: Wants 配線（policy / context）=="
systemctl show -p Wants --value jarvis-policy || true
systemctl show -p Wants --value jarvis-context || true

echo "== 修正: sqlx-cli 導入（root・既存 sqlx 0.8 系統を活用・値なし）=="
sudo cargo install sqlx-cli --no-default-features --features postgres --locked --root /usr/local

echo "== 修正: 通常デプロイで unit を配置 =="
# build-selfhost.sh は jarvis ユーザーで実行する（root で実行すると
# JARVIS_USER=root になり他 unit の User= が書き換わる）。スクリプト内の sudo に権限が必要。
JARVIS_USER=jarvis REPO_DIR="$JARVIS_REPO" \
  bash infra/scripts/build-selfhost.sh

echo "== 再確認: unit を直接実行（oneshot・DB へ DDL 適用は冪等だが書き込み）=="
sudo systemctl start jarvis-migrations
systemctl is-active jarvis-migrations
systemctl status jarvis-migrations --no-pager -l

echo "== 再確認: policy 起動確認（Wants= は失敗を伝播しないので migration 成否は別途確認）=="
systemctl is-active jarvis-policy
curl -fsS http://127.0.0.1:8081/health
```

- `systemctl is-active jarvis-migrations` は `active (exited)` を期待する
  （`Type=oneshot` + `RemainAfterExit=yes`）。`failed` なら
  `journalctl -u jarvis-migrations -n 50 --no-pager` で原因（多くは
  cargo-sqlx 未配置、または bws 経由の URL 未注入）を確認する。
- policy / context を再起動して配線を確認する場合は
  `sudo systemctl restart jarvis-policy jarvis-context` を別途承認のもとで
  実行し、再び `systemctl is-active jarvis-migrations` と policy の health を
  確認する。S9・BK-2 の手順とは独立に実行できるが、本番 DB への書き込みを
  伴うため既存の承認窓に合わせる。

#### 適用主体の定義と配布ズレのリスク（I35-A、2026-11-19）

**方針（確定）**: マイグレーションの最終的な適用主体は
`jarvis-context` の `sqlx::migrate!()`
（[builder.rs](../../rust/jarvis-context/src/builder.rs):862）とする。
バイナリに埋め込まれたマイグレーションが権威であり、context の起動時に
`_sqlx_migrations` へ適用履歴を記録して適用を完了する。
`jarvis-migrations.service` はその**前段の pre-flight 適用**であり、
依存サービスの可用性を上げる補助にすぎず、所有権は持たない。
コードは変更せず、context 側の `migrate!()` は残す（受け入れ条件）。

**判断根拠**:

- 2つの適用経路（unit の `cargo sqlx migrate run` と context の
  `migrate!()`）は、sqlx の advisory lock により同時実行なら直列化される。
  したがって「二重適用そのもの」は故障要因ではない。
- 一方で sqlx は適用済みマイグレーションの checksum を
  `_sqlx_migrations` に保存する。適用主体を unit と決めて context 側を
  廃止すれば、配布ソースのズレを隠蔽できるが、バイナリ側の権威を失う。
- 起動主体が自分でスキーマを完了できる方が fail-safe であり、
  unit は「先に適用して起動を速くする」最適化として扱うのが自然である。
  以上の理由で、埋め込みマイグレーションを権威とする本方針を採用した。

**残るリスク（配布ズレ時の checksum mismatch）**:

- `/opt/jarvis/rust/migrations`（unit が `--source` で読む）と
  `/opt/jarvis/bin/jarvis-context`（`migrate!()` がビルド時に埋め込み）が
  異なるコミットから配布されると、既に適用済みのバージョンの内容が
  変わったと判定され、context は起動時に checksum mismatch で失敗する。
  unit の pre-flight が先に走ると `_sqlx_migrations` へ追加された行が
  context 側の埋め込みセットと突き合わされ、バージョンの欠落や checksum
  不一致として起動失敗になる（sqlx は適用済み行を書き換えない）。
- これは意図した fail-closed 挙動であり、黙って起動させることはしない。
  **軽減策は「両者を同一コミットから配布する」ことのみ**で、
  `sqlx migrate baseline` や履歴行の手削除はデータ不整合を招くため行わない。

**今回の変更（軽減策の実装）**:

- 3 unit のコメントから "Single owner" に相当する表現を削除し、
  上記の方針（context が最終適用主体、unit は pre-flight）へ置換した。
  `After=` / `Wants=` / `ExecStart=` / hardening は不変。
  `Wants=` は失敗を伝播しない（fail-open）ため、記述も合わせて訂正した
  （選択肢とともに MIG 節の「systemd `Wants=` の意味」を参照）。
- [build-selfhost.sh](../../infra/scripts/build-selfhost.sh) に、
  ビルド元 HEAD を `/opt/jarvis/bin/.jarvis-build-commit` へ記録するのに加え、
  `rust/migrations/*.sql` を `/opt/jarvis/rust/migrations/` へ配置し、
  同一 HEAD を `.jarvis-source-commit` として生成するようにした（I36-A）。
  ソースディレクトリ・`*.sql`・マーカーの欠落は警告ではなく fail-fast とし、
  新規導入でも `--source` が空にならないことを保証する。
  マーカー不一致も従来どおりデプロイを停止する。
  既存のビルド／バイナリ配置／unit 生成／daemon-reload の各処理と、
  「jarvis ユーザーで実行し sudo を使う」「dry-run モードは持たない」という
  契約は変更していない。
- なお P1-3 の「現状」節は `migrate!()` 導入前のスナップショットであり、
  現在は [builder.rs](../../rust/jarvis-context/src/builder.rs):862 に
  `migrate!()` が存在する。本節の記述を現状として優先する。

### P1-4: バックアップ外部化（設計決定が先）

保全先・RPO/RTO が未決定のため、方針確定後に値を設定して実行する。

```bash
#!/usr/bin/env bash
set -euo pipefail

: "${JARVIS_REPO:?repo ルートを JARVIS_REPO に設定してから実行}"
: "${JARVIS_BACKUP_ROOT:?バックアップ保存先ルートを設定}"

echo "== 実行環境: Kubuntu ローカル =="
cd "$JARVIS_REPO"

echo "== 確認: 現状はローカル保存（単一ディスク）=="
make backup-preflight
df -h "$JARVIS_BACKUP_ROOT"

echo "== 修正: 別媒体へ 1 世代コピー（保存先決定後に値を設定）=="
: "${BACKUP_SRC:?取得元 set のパスを設定}"
: "${BACKUP_DST:?外部保存先のパスを設定}"
rc=0
rsync -a -- "$BACKUP_SRC/" "$BACKUP_DST/" || rc=$?
echo "rsync exit: $rc"
[ "$rc" -eq 0 ]

echo "== 再確認: バックアップ整合性（コピー元・読み取り専用）=="
# コピー先の検証は保全先と RPO/RTO の決定後に手順を定める。
make verify-backup
```

### P0-7: Grafana 認証の供給経路（設計のみ・実装未実施）

現状は `GRAFANA_CLOUD_AUTH` がどの経路からも注入されていない。
A / B / C の方針決定後に unit / 設定を編集し、root で再起動・送信成功を確認する。

```bash
#!/usr/bin/env bash
set -euo pipefail

: "${JARVIS_REPO:?repo ルートを JARVIS_REPO に設定してから実行}"

echo "== 実行環境: Kubuntu ローカル / 読み取り専用確認 =="
cd "$JARVIS_REPO"

echo "== 確認: 設定側の参照 =="
grep -n 'GRAFANA_CLOUD_AUTH' infra/config/otelcol-config.yaml || true

echo "== 確認: 稼働 unit の起動経路 =="
systemctl cat otelcol.service || true

echo "== 確認: env の有無のみ（値は出力しない）=="
if [ -n "${GRAFANA_CLOUD_AUTH+x}" ]; then
  echo "GRAFANA_CLOUD_AUTH: set"
else
  echo "GRAFANA_CLOUD_AUTH: unset"
fi
```

### P1-12: Webhook の公開範囲（設計のみ・未実装）

現状は loopback bind + Tailscale の信頼境界を前提とし、認証・署名はない。
Tailscale 外へ公開する場合は認証・署名検証・レート制限を再設計してから適用する。

```bash
#!/usr/bin/env bash
set -euo pipefail

: "${JARVIS_REPO:?repo ルートを JARVIS_REPO に設定してから実行}"

echo "== 実行環境: Kubuntu ローカル / 読み取り専用確認 =="
cd "$JARVIS_REPO"

echo "== 確認: 登録済み webhook ルート =="
grep -n 'webhook/' rust/jarvis-event/src/normalizer.rs || true

echo "== 確認: 待ち受け先（repo 既定値）=="
grep -n '127.0.0.1' rust/jarvis-event/src/config.rs || true

echo "== 確認: 実機のリスニング状態（root 必須・値は出力しない）=="
sudo ss -ltnp
```

## メモ

- 本一覧は「実装タスク」だけでなく「方針決定が先に必要な項目」を含みます（特に BK-1, MCP-1, SEC-1, CI-1）。
- 実機変更・デプロイ・秘密情報変更・外部サービス操作を伴う項目は、別途の確認と手順に従って進めてください。
- 承認待ち項目の実行窓は「承認待ち実機タスクの実行手順」節を参照してください。
