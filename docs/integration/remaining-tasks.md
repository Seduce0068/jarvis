# JARVIS 残タスク / 他タスク洗い出し（2026-11-15 確認）

本ドキュメントは、リポジトリ内容・設定・実装・ローカル systemd 実態・ドキュメント記載を点検して整理した「現時点で残っている/判断待ちのタスク」の一覧です。
**安全第一**の観点で、破壊的操作や未確認の前提に依存する作業は「要判断」として分離しています。

## 確認前提（この洗い出し時点）

- ブランチ/作業ツリー: `master...origin/master`、未コミット変更なし
- GitHub Issues: オープン 0 件（`gh issue list --state open`）
- ローカル稼働サービス: `jarvis-policy` / `jarvis-mcp-gw` / `jarvis-event` / `jarvis-context` は `active` / `enabled`
- バックアップ: `jarvis-backup.service` / `jarvis-backup.timer` は repo に存在するがローカル未導入（not-installed）
- 方針: OCI は使用しない（[adr/0004-local-only-no-oci.md](../../adr/0004-local-only-no-oci.md)）

## 1. MCP Gateway / ツール機能（優先度: 高）

| ID | タスク | 現状・根拠 | 次のアクション（案） |
|---|---|---|---|
| MCP-1 | 承認フローの設計・実装・E2E 配線 | **2026-11-15 実装済み（repo側）**: gateway が `requires_approval=true` のツール呼び出しを jarvis-policy の HTTP 承認フロー（`POST /v1/approval`）に転送し、オペレータが `POST /v1/approval/{id}/approve|deny` で決定するまで待機する（`rust/jarvis-mcp-gw/src/approval.rs`, `lib.rs`）。失敗・期限切れは fail-closed で拒否。承認必須ツールも `tools/list` に表示されるよう変更（旧「非表示」ワークアラウンド廃止）。E2E テスト追加（`http::tests::execute_endpoint_blocks_until_operator_approval` 等）。 | 実機適用は次回 `deploy-mcp-gw-safe.sh --apply` 時（policy HTTP addr 既定 `127.0.0.1:8081` は unit 既存設定と整合）。**オペレーター承認UI は GPUI コンソール（`ui/`, ADR-0005）で実装済み**（2026-11-16、実機ビルド・起動検証済み、`make ui-run`）。監査永続化は今後の拡張。 |
| MCP-2 | 未実装ツールのネイティブ実装または上流 MCP 登録 | `web.search` / `homeassistant.get_state` / `android.get_device_info` はネイティブ未実装のため承認必須化・非表示。`filesystem.write` / `shell.execute` / `git.commit` / `github.create_pr` / `homeassistant.call_service` / `android.send_notification` も承認待ち/非表示（[tool-matrix.md](tool-matrix.md), [dsh.md](dsh.md)）。 | **2026-11-16 現状**: MCP-1 実装で未実装ツールも `tools/list` に表示されるようになり、呼び出しは承認フローを通過した後 `no native handler ... register an upstream MCP server` で fail-closed に失敗する（安全だが UX 上のデッドエンド）。ネイティブ実装には外部サービス接続（HA/Android/Web 検索）と資格情報、書き込み系には systemd `ReadWritePaths` の拡張が必要であり、ツール単位の利用要件判断が先。当面は現状の fail-closed 挙動を維持する。 |
| MCP-3 | allowlist `sandbox_type`（ツール単位）の整理 | **2026-11-15 解消（repo側）**: ADR-0002 の `opencode` CLI sandbox fallback 撤去に伴い、option A（dead config 削除）を採用。[infra/config/mcp_allowlist.toml](../../infra/config/mcp_allowlist.toml) から per-tool `sandbox_type` キーを削除（TOML parse 検証済み）。gateway 全体設定 `JARVIS_MCP_GW_SANDBOX_TYPE` は既存 unit/env 互換のため解析のみ残存し、実行経路には影響しない（[adr/0002-runtime-agnostic.md](../../adr/0002-runtime-agnostic.md) 更新済み）。 | `/etc/jarvis/mcp_allowlist.toml` への同期は次回 `deploy-mcp-gw-safe.sh --apply` 時（適用まで `check-deploy-drift.sh` は PENDING を表示する）。将来ツール単位の隔離が必要になった場合は、上流 MCP サーバ側の sandbox 設定（`register_mcp_server` のコマンド）で実装する。 |
| MCP-4 | 読み取り基準ディレクトリの絞り込み | **2026-11-15 解消（repo側）**: [infra/systemd/jarvis-mcp-gw.service](../../infra/systemd/jarvis-mcp-gw.service) の `JARVIS_MCP_GW_BASE_DIR` を `/home/koppe` から repo ルート（`/home/koppe/ドキュメント/作業用/jarvis/jarvis`）へ絞り込み。`ProtectHome=read-only` は `/home` 読取に必要なため維持。smoke にも `JARVIS_MCP_GW_BASE_DIR="$repo"` を追加し、狭い基準での `filesystem.list`/`git.status` を検証する。 | `/etc` への同期は次回 `deploy-mcp-gw-safe.sh --apply` 時。適用後は repo 外の読み取りが拒否されることを代表的 `tools/call` で確認する（実機変更は別判断）。 |

## 2. バックアップ / DR・OCI レガシー整理（優先度: 高）

| ID | タスク | 現状・根拠 | 次のアクション（案） |
|---|---|---|---|
| BK-1 | バックアップ保存先の再決定と実装更新 | [adr/0004-local-only-no-oci.md](../../adr/0004-local-only-no-oci.md) で OCI 非使用・保存先再決定が未解決。`rust/scripts/backup.rs` / `restore.rs` / `infra/scripts/backup.sh` は OCI Object Storage(rclone)・`op inject`・`/opt/jarvis` `/home/jarvis` 前提のレガシー。2026-11-15: [backup-redesign-notes.md](backup-redesign-notes.md) に要件・論点・移行ステップのドラフトに加え、レガシー資産の前提棚卸し（read-only）、実装・導入の安全ゲート、保存先候補の比較観点を整理。 | 保存先（例: ローカル/NAS/別ストレージ）・保持期間・暗号化・復旧手順・`--dry-run` 相当の事前条件を決め、[backup-redesign-notes.md](backup-redesign-notes.md) の安全ゲート/比較観点を踏まえてスクリプト/README/unit を再設計する（実装・実機導入は別判断）。 |
| BK-2 | バックアップ timer/service のローカル運用化 | `infra/systemd/jarvis-backup.service` / `.timer` は repo に存在するがローカル未導入（not-installed）。timer は `OnCalendar=daily` + `Persistent=true` + `RandomizedDelaySec=1h`、service は `User=jarvis`・`/opt/jarvis`・`/home/jarvis/.cache/rust-script`・`/run/rclone`・`op.env` 前提のレガシー（2026-11-15 確認）。 | **BK-1/SEC-2 決定と実装現行化の完了後**に、現行ホスト/配置/認証方式へ合わせて unit を更新して導入する（実機変更は別判断）。有効化前の目安: (1) repo→実機の差分・所有者/権限/依存パス/認証供給経路を確認し、`systemctl daemon-reload` を含む導入/ロールバック手順を用意、(2) timer 有効化前に service を手動実行して非破壊検証（可能なら dry-run 相当、出力先・生成物・整合性確認、失敗時ログ確認）、(3) 隔離先で restore/dry-run を検証して復旧手順と成功判定を確認、(4) 問題なければ `systemctl enable --now jarvis-backup.timer` 等で有効化し、初回実行・`journalctl`・生成物/保持ポリシーを確認。`Persistent=true` により未実行分の追従が起き得るため、有効化タイミングと実装状態の整合を事前に確認する。 |
| BK-3 | OpenTofu/OCI 後始末資産の扱い決定 | [infra/opentofu/](../../infra/opentofu/) は未使用レガシー（[infra/opentofu/README.md](../../infra/opentofu/README.md)）。後始末用に [infra/scripts/decommission-oci.sh](../../infra/scripts/decommission-oci.sh) あり（既定は dry-run、`--apply` で destroy 実行。`tofu`/OCI CLI 設定/`infra/opentofu/environments/prod` の init/state 等が前提。実行時は `tofu init`・`state list`・最終 state バックアップ（`$HOME/jarvis-oci-state-<ts>.json`）・`destroy` plan 作成を行う）。2026-11-15 確認: `infra/opentofu/environments/prod/.terraform/terraform.tfstate` がローカルに存在（内容は未確認）。 | 「保持」「削除」「OCI リソース削除実行」の方針決定。**実行判断/実行前の確認例（read-only・dry-run優先）**: (1) `tofu`/OCI CLI/認証情報・対象 tenancy/region・`terraform.tfvars` 等の前提を確認し、state の所在・鮮度・バックアップ（別保管含む）を確保する。(2) OCI コンソール/CLI で実リソース有無・保護すべきデータ（Object Storage/Block Volume 等）の残存を確認し、未追跡リソースや state 不整合がないか点検する（A1 Free 枠再取得不可等の影響も考慮）。(3) まず dry-run（既定）で `state list` と destroy plan の対象を確認し、出力を保存/レビューしてから `--apply` の要否を判断する。(4) 実削除時のロールバック困難性・Tailscale 管理画面の後処理等を手順化し、必要な承認/権限のもとで実施する（実削除/OCI 操作は別判断・手順）。 |
| BK-4 | レガシー設定テンプレートの整理 | `infra/config/*.env` は参照テンプレート扱いだが記述状態が混在（2026-11-15 確認）: [policy.env](../../infra/config/policy.env), [context.env](../../infra/config/context.env), [event.env](../../infra/config/event.env), [otel.env](../../infra/config/otel.env) は先頭に legacy/reference template 注記があり `op://` 参照を含む。一方 [code_server.env](../../infra/config/code_server.env), [mcp_gateway.env](../../infra/config/mcp_gateway.env) は `op://` を含まないが「Populated by 1Password CLI at runtime」等の旧前提記述が残り、現行 unit/`bws`/実機配置との整合は未確認（実機 secret/運用変更なし）。 | ファイル単位で棚卸しし、「現行参照テンプレートとして維持」「注記/値の更新」「legacy 凍結」「削除」の方針を現行運用（bws/op 等）・installed unit 実態確定後に判断する（実機 secret の変更・不要ファイル削除は別判断）。 |

### BK/SEC 進捗（2026-11-16 追記）

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
| SEC-1 | otelcol unit の repo/実機差分の整合 | repo: [infra/systemd/otelcol.service](../../infra/systemd/otelcol.service) は `op run` + `/etc/jarvis/op.env` + `User=jarvis`。2026-11-15 実機確認（メタデータ/設定内容の閲覧なし）: `/etc/systemd/system/otelcol.service` は `User=koppe`/`Group=koppe` かつ `op run`・`EnvironmentFile` なしで `/usr/local/bin/otelcol-contrib --config /etc/jarvis/otelcol-config.yaml` を直接起動し `active`/`enabled`。`/etc/jarvis/otel.env`・`/etc/jarvis/op.env` は存在せず、`/etc/jarvis/otelcol-config.yaml` は `root:root 0644`（資格情報供給元は未特定）。[AGENTS.md](../../AGENTS.md) には op→bws 移行未完了の記載あり。 | 実機の正しい起動/認証方式を非破壊で特定し（資格情報の供給元・参照先・必要権限、**secret の値は確認/記録/共有しない**）、repo か実機のどちらを正とするか決定。統一時は実行ユーザー/グループ、`EnvironmentFile`/`op run`/`bws` 等、config 読取権限、`/var/log/jarvis` 等の書込先を含めて整合させ、監視停止・認証失敗リスクを考慮したデプロイ/ロールバック手順で実施。**非破壊確認の例（read-only・値を出さない）**: `systemctl status otelcol --no-pager`、`systemctl cat otelcol.service`（unit 本文に資格情報が直書きされていないか目視確認）、`find /etc/systemd -path '*otelcol*' -ls`（drop-in/override 有無と権限）、`ls -ld /etc/jarvis /var/log/jarvis 2>/dev/null; ls -l /etc/jarvis/otelcol* /etc/jarvis/otel.env /etc/jarvis/op.env 2>/dev/null`（存在・所有者・mode のみ）、`journalctl -u otelcol -n 200 --no-pager`（認証失敗/endpoint エラー等の有無を確認し、共有時は秘匿情報をマスク）。`systemctl show -p Environment/EnvironmentFiles ...` や env ファイル/設定本文の表示は secret 値を露出し得るため原則行わない（必要時は権限管理された手順で別途判断）。 |
| SEC-2 | バックアップ系の 1Password 依存解消 | バックアップ系レガシー資産に 1Password 依存が残る（2026-11-15 確認）: [infra/systemd/jarvis-backup.service](../../infra/systemd/jarvis-backup.service) は `/etc/jarvis/op.env`（`OP_SERVICE_ACCOUNT_TOKEN` 想定）を参照、[rust/scripts/backup.rs](../../rust/scripts/backup.rs) / [rust/scripts/restore.rs](../../rust/scripts/restore.rs) は `op inject` を使用。OCI/rclone 前提とも結合。 | BK-1 の保存先/認証設計と合わせて解消（現行運用の秘密管理方式との整合を要確認、実機 secret 変更は別判断） |

### MCP-1 承認フローのセキュリティ境界（2026-11-16 レビュー）

- jarvis-policy の承認 API（`/v1/approval`, `/v1/approvals`,
  `/v1/approval/{id}/approve|deny`）は **認証トークンを要求しない**。
  現在は `127.0.0.1` bind + systemd の実行ユーザー分離により
  ローカルプロセス以外からは到達できないため許容しているが、
  **ループバック外へ公開する前に認証（至少 admin token または
  mTLS）を追加する必要がある**。
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
| CI-1 | review-gate の Actions 配線方針決定 | [ci/review-gate.yml](../../ci/review-gate.yml) は 3-stage 定義として repo に存在するが `.github/workflows/` には未配線（2026-11-15 確認: `.github/workflows/` は `ci.yml` のみ）。現状の定義: PR/workflow_dispatch トリガー、`ubuntu-latest`、`gate-check`→`cross-review`→`merge-gate`→`auto-merge` + 常時 `metrics`。OpenCode CLI を `curl ... https://opencode.ai/v2/install | bash` で導入し `opencode run` で `/review-flow gate-check`・`/cross-review`・`/review-flow metrics` を実行、JSON/Markdown 出力を `jq`/`grep` で判定・投稿。追加ツール導入（gitleaks/trufflehog/madge）、Rust toolchain（一部ジョブで `aarch64-unknown-linux-gnu` target 指定）、`gh pr check/comment/merge` + `secrets.GITHUB_TOKEN`、成功時の自動 squash merge/delete-branch を含む。`permissions:` 明示なし、OpenCode の認証/モデル設定や `/review-flow`・`/cross-review` コマンド提供元の前提は workflow 内で未定義。`needs.cross-review.outputs.verdict` を参照するが job outputs 未宣言に見える等、配線前の動作検証が必要。2026-11-15: `ci/review-gate.yml` 先頭コメントおよび `AGENTS.md` に CI-1/CI-2 追跡参照を追記済み。 | **2026-11-16 追記（repo側・配線は未実施）**: [ci/review-gate.yml](../../ci/review-gate.yml) の明確な不具合を修正（cross-review の outputs 未宣言、`gh pr check --conclusion` は存在しないサブコマンド、merge-gate の OpenCode 未導入・PR番号フォールバック欠落、package-lock 不在の `cache: npm`、permissions 未明示）。YAML parse 検証済み。**依然として `.github/workflows/` へは未配線**（OpenCode 認証情報・runner 要件・auto-merge 可否の判断が必要）。 |
| CI-2 | `opencode run` 依存の置換検討 | [adr/0002-runtime-agnostic.md](../../adr/0002-runtime-agnostic.md) はランタイム非依存化を志向（proposed）。2026-11-15 確認: `opencode run` 依存は [ci/review-gate.yml](../../ci/review-gate.yml)（gate-check/cross-review/merge-gate/metrics の4箇所）と [AGENTS.md](../../AGENTS.md) の手動実行手順、[adr/0001-unified-architecture.md](../../adr/0001-unified-architecture.md) の非対話実行記述に残る（review-gate は CI 未配線）。 | **2026-11-16 実装済み（repo側）**: 4箇所の `opencode run` を [ci/scripts/review-run.sh](../../ci/scripts/review-run.sh) へ集約。`JARVIS_REVIEW_RUNNER`（既定 `opencode`）でバックエンド切替可能、runner 契約は `run <prompt> [--json]`。スタブによる4契約テスト（`bash ci/scripts/test-review-run.sh`）成功。代替ランタイムの選定・認証情報設定は残課題（ADR-0002 の段階移行方針と整合）。 |

## 5. アーキテクチャ/ドキュメント整合性（優先度: 中）

| ID | タスク | 現状・根拠 | 次のアクション（案） |
|---|---|---|---|
| DOC-1 | ADR-0001 未チェック項目の棚卸し・更新 | [adr/0001-unified-architecture.md](../../adr/0001-unified-architecture.md) に未チェック項目が残る（OCI A1+Tailscale 疎通、1Password CLI 設定、同一ワークスペース共有確認、Grafana Cloud アラート、Idle Compute 対策、review-flow CI ゲート統合）。OCI 前提項目は ADR-0004 により陳腐化。2026-11-15 時点の再分類メモ（ADR本文の状態変更は別判断）: **非適用候補（ADR-0004）**= Phase 0「OCI A1+Tailscale 疎通」、Phase 4「Idle Compute 対策」 / **継続・要再評価**= Phase 0「1Password CLI 設定・秘密情報注入確認」（現行 `bws` 利用、`otelcol` 等は SEC-1 で実機整合確認）、Phase 1「同一ワークスペース共有確認」（ローカル運用での現行要件・実態確認が必要）、Phase 4「Grafana Cloud アラート」（方針/設定確認）、Phase 4「review-flow CI ゲート統合」（CI-1/CI-2） / **備考**= Phase 4「rclone バックアップ」は成果物チェック済みだが OCI Object Storage 前提・現行未運用の注記あり（BK-1/BK-2/SEC-2）。 | ADR-0001 本文のチェック状態・注記を上記分類に沿って更新するか、必要に応じて補助ADR/個別タスクへ分離（実態確認・設定変更が必要な項目は別途証跡を取る）。**本文更新時の方針（案）**: (1) `[x]` は実態確認済みの証跡がある項目に限定し、未確認は `[ ]` のまま根拠/日付付き注記を残す。(2) ADR-0004 により前提が消えた項目は「非適用（理由・参照ADR）」と明記し、完了扱いにしない。(3) `bws`/`op` 混在・CI未配線等の進行中/要再評価項目は関連タスク（SEC-1, CI-1/CI-2, BK-1 等）へリンクする。(4) ADR本文の決定内容・ステータス変更が伴う場合は別判断とし、必要なら追補ADR/注記の粒度を分ける。 |
| DOC-2 | 参照ドキュメントの未解決事項の要否判断 | `docs/architecture/mpe-rust-latex-architecture-report.md` §12 / `docs/architecture/opencode-v2-oci-code-server.md` §未解決事項 に未解決事項があるが、いずれも提案/参照時点の検証・設計選択事項（MPE実機検証、旧OCI構成前提の選択肢等）であり、現行 JARVIS ローカル運用のアクティブタスクとは別。 | **保留（参照資料扱い）**: 該当テーマを実施する場合に個別タスク化する |

## 6. 運用上のフォローアップ（優先度: 低〜中）

| ID | タスク | 現状・根拠 | 次のアクション（案） |
|---|---|---|---|
| OPS-1 | MCP クライアント再接続の運用明文化 | gateway 再起動後に MCP セッションが失効し再初期化が必要（前ゴールで `tools/call` 等が `Unauthorized: Session not found` となる事象を確認）。2026-11-15: [dsh.md](dsh.md) / [tool-matrix.md](tool-matrix.md) に再接続手順の注記を追記。 | クライアント実装側の自動再接続/再初期化（失敗時リトライ含む）の要否を検討し、必要なら実装タスク化。**検討観点（実装は別判断）**: (1) 検知条件は `Unauthorized: Session not found` 等のセッション失効エラーや接続断を明示的に扱い、恒久的な認可/設定エラーと混同しない。(2) 復旧は transport 再接続 + `initialize` の再実行を基本とし、endpoint/設定が同一なら再利用可能とする（[dsh.md](dsh.md) / [tool-matrix.md](tool-matrix.md) の運用メモと整合）。(3) リトライは回数・間隔・上限時間を設け、無限再試行や過負荷を避ける。(4) 冪等性: 現状の read-only ツールは再試行しやすいが、将来の書き込み/承認待ち/副作用ありツールでは自動再試行条件を厳格化する（失敗位置の不明確さをユーザーに提示）。(5) 再接続中/失敗時のユーザー通知、ログ、進行中操作の扱い（再実行要否）を定義し、必要なら DSH/Cordis 側のクライアント設定・実装箇所を特定してタスク化する。 |

## メモ

- 本一覧は「実装タスク」だけでなく「方針決定が先に必要な項目」を含みます（特に BK-1, MCP-1, SEC-1, CI-1）。
- 実機変更・デプロイ・秘密情報変更・外部サービス操作を伴う項目は、別途の確認と手順に従って進めてください。
