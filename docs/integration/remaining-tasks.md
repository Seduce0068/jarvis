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
| MCP-1 | 承認フローの設計・実装・E2E 配線 | 承認必須ツールは `tools/list` 非表示で実用不可（`web.fetch` 等）。allowlist で `requires_approval=true` の項目あり（[infra/config/mcp_allowlist.toml](../../infra/config/mcp_allowlist.toml), [tool-matrix.md](tool-matrix.md), [dsh.md](dsh.md), [adr/0003-native-web-fetch.md](../../adr/0003-native-web-fetch.md)）。policy 側には承認待ち/決定の仕組みがある（`rust/jarvis-policy/src/approval.rs` 等）。 | 承認主体・UI/API・タイムアウト・監査要件を決め、gateway〜クライアント〜policy の通しフローを実装/検証 |
| MCP-2 | 未実装ツールのネイティブ実装または上流 MCP 登録 | `web.search` / `homeassistant.get_state` / `android.get_device_info` はネイティブ未実装のため承認必須化・非表示。`filesystem.write` / `shell.execute` / `git.commit` / `github.create_pr` / `homeassistant.call_service` / `android.send_notification` も承認待ち/非表示（[tool-matrix.md](tool-matrix.md), [dsh.md](dsh.md)）。 | 利用要件の高い順に実装方針（native or upstream）と安全制約（承認・egress・sandbox）を決定 |
| MCP-3 | allowlist `sandbox_type`（ツール単位）の整理 | allowlist 各ツールに `sandbox_type` があるが、[rust/jarvis-mcp-gw/src/allowlist.rs](../../rust/jarvis-mcp-gw/src/allowlist.rs) の `AllowedTool` は当該フィールドを持たず読み取らないため、[infra/config/mcp_allowlist.toml](../../infra/config/mcp_allowlist.toml) の per-tool 値は実効設定になっていない（2026-11-15 確認）。実際のサンドボックス種別は [rust/jarvis-mcp-gw/src/config.rs](../../rust/jarvis-mcp-gw/src/config.rs) の gateway 全体設定（例: env `JARVIS_MCP_GW_SANDBOX_TYPE`）を [rust/jarvis-mcp-gw/src/sandbox.rs](../../rust/jarvis-mcp-gw/src/sandbox.rs) で参照する形。 | 方針を選択: (A) dead config を repo から削除して単一の真実（gateway 全体設定）に寄せる、または (B) `AllowedTool`/実行経路に per-tool `sandbox_type` を追加してツール単位ルーティングを実装。いずれも repo 変更時は `/etc/jarvis/mcp_allowlist.toml`・systemd env 等の実機差分、デプロイ/rollback/smoke/drift 手順への影響を考慮 |
| MCP-4 | 読み取り基準ディレクトリの絞り込み | `ProtectHome=read-only` + `JARVIS_MCP_GW_BASE_DIR=/home/koppe` によりホーム配下全体が読み取り可能（2026-11-15 実測/文書: [tool-matrix.md](tool-matrix.md), [infra/systemd/jarvis-mcp-gw.service](../../infra/systemd/jarvis-mcp-gw.service)）。`filesystem.*` / `git.*` はこの基準配下で動作するが、機微ファイルへの到達リスクが残る。 | 実運用に必要な最小ディレクトリへ限定し、unit/env・allowlist 想定・drift/smoke（`tools/call`）で確認（変更時はデプロイ/ロールバック手順に従う） |

## 2. バックアップ / DR・OCI レガシー整理（優先度: 高）

| ID | タスク | 現状・根拠 | 次のアクション（案） |
|---|---|---|---|
| BK-1 | バックアップ保存先の再決定と実装更新 | [adr/0004-local-only-no-oci.md](../../adr/0004-local-only-no-oci.md) で OCI 非使用・保存先再決定が未解決。`rust/scripts/backup.rs` / `restore.rs` / `infra/scripts/backup.sh` は OCI Object Storage(rclone)・`op inject`・`/opt/jarvis` `/home/jarvis` 前提のレガシー。2026-11-15: [backup-redesign-notes.md](backup-redesign-notes.md) に要件・論点・移行ステップのドラフトを整理。 | 保存先（例: ローカル/NAS/別ストレージ）・保持期間・暗号化・復旧手順を決めてスクリプト/README/unit を再設計（実装・実機導入は別判断） |
| BK-2 | バックアップ timer/service のローカル運用化 | `infra/systemd/jarvis-backup.service` / `.timer` は存在するがローカル未導入（not-installed）。unit 内容もレガシー前提（`User=jarvis`, `op.env`, `/run/rclone` 等）。 | BK-1 決定後に unit を現行環境へ合わせて導入、`systemctl enable --now` と復旧テストを実施 |
| BK-3 | OpenTofu/OCI 後始末資産の扱い決定 | [infra/opentofu/](../../infra/opentofu/) は未使用レガシー（[infra/opentofu/README.md](../../infra/opentofu/README.md)）。後始末用に [infra/scripts/decommission-oci.sh](../../infra/scripts/decommission-oci.sh) あり（既定は dry-run、`tofu`/OCI CLI/init/state 等が前提）。2026-11-15 確認: `infra/opentofu/environments/prod/.terraform/terraform.tfstate` がローカルに存在（内容は未確認）。 | 「保持」「削除」「OCI リソース削除実行」の方針決定。state のバックアップ・所在・妥当性確認と、OCI 実リソース有無の確認を行った上で判断（実削除/OCI 操作は別判断・手順） |
| BK-4 | レガシー設定テンプレートの整理 | `infra/config/*.env` に `op://` 参照が残る（2026-11-15 確認: [policy.env](../../infra/config/policy.env), [context.env](../../infra/config/context.env), [event.env](../../infra/config/event.env), [otel.env](../../infra/config/otel.env)）。[AGENTS.md](../../AGENTS.md)/[infra/README.md](../../infra/README.md) では参照テンプレート扱いで、各ファイル先頭に legacy/reference template 注記あり（実機 secret/運用変更なし）。 | 残りの具体値（パス・認証方式等）の整備/削除方針は現行運用（bws/op 等）確定後に判断（実機 secret の変更は別判断） |

## 3. シークレット管理・構成ドリフト（優先度: 中〜高）

| ID | タスク | 現状・根拠 | 次のアクション（案） |
|---|---|---|---|
| SEC-1 | otelcol unit の repo/実機差分の整合 | repo: [infra/systemd/otelcol.service](../../infra/systemd/otelcol.service) は `op run` + `/etc/jarvis/op.env` + `User=jarvis`。2026-11-15 実機確認（メタデータ/設定内容の閲覧なし）: `/etc/systemd/system/otelcol.service` は `User=koppe`/`Group=koppe` かつ `op run`・`EnvironmentFile` なしで `/usr/local/bin/otelcol-contrib --config /etc/jarvis/otelcol-config.yaml` を直接起動し `active`/`enabled`。`/etc/jarvis/otel.env`・`/etc/jarvis/op.env` は存在せず、`/etc/jarvis/otelcol-config.yaml` は `root:root 0644`（資格情報供給元は未特定）。[AGENTS.md](../../AGENTS.md) には op→bws 移行未完了の記載あり。 | 実機の正しい起動/認証方式を非破壊で特定し（資格情報の供給元・参照先・必要権限、secret 内容は確認しない）、repo か実機のどちらを正とするか決定。統一時は実行ユーザー/グループ、`EnvironmentFile`/`op run`/`bws` 等、config 読取権限、`/var/log/jarvis` 等の書込先を含めて整合させ、監視停止・認証失敗リスクを考慮したデプロイ/ロールバック手順で実施 |
| SEC-2 | バックアップ系の 1Password 依存解消 | バックアップ系レガシー資産に 1Password 依存が残る（2026-11-15 確認）: [infra/systemd/jarvis-backup.service](../../infra/systemd/jarvis-backup.service) は `/etc/jarvis/op.env`（`OP_SERVICE_ACCOUNT_TOKEN` 想定）を参照、[rust/scripts/backup.rs](../../rust/scripts/backup.rs) / [rust/scripts/restore.rs](../../rust/scripts/restore.rs) は `op inject` を使用。OCI/rclone 前提とも結合。 | BK-1 の保存先/認証設計と合わせて解消（現行運用の秘密管理方式との整合を要確認、実機 secret 変更は別判断） |

## 4. CI/CD・レビュー基盤（優先度: 中）

| ID | タスク | 現状・根拠 | 次のアクション（案） |
|---|---|---|---|
| CI-1 | review-gate の Actions 配線方針決定 | [ci/review-gate.yml](../../ci/review-gate.yml) は 3-stage workflow 定義として存在するが `.github/workflows/` には未配線（2026-11-15 確認: `.github/workflows/` は `ci.yml` のみ）。OpenCode CLI / `opencode run`、review-flow/cross-review 実行、追加ツール導入、必要な認証情報・runner 環境が前提（[AGENTS.md](../../AGENTS.md), [adr/0001-unified-architecture.md](../../adr/0001-unified-architecture.md)）。 | OpenCode 認証情報・runner 要件を踏まえ、配線/手動運用継続/代替手段を決定（配線する場合は workflow の前提・ターゲット環境も要見直し） |
| CI-2 | `opencode run` 依存の置換検討 | [adr/0002-runtime-agnostic.md](../../adr/0002-runtime-agnostic.md) はランタイム非依存化を志向。2026-11-15 確認: `opencode run` 依存は [ci/review-gate.yml](../../ci/review-gate.yml)（gate-check/cross-review/merge-gate/metrics 実行）と [AGENTS.md](../../AGENTS.md) の手動手順記載に残る（review-gate は CI 未配線）。[adr/0001-unified-architecture.md](../../adr/0001-unified-architecture.md) の非対話実行記述も OpenCode CLI 前提。 | 置換範囲（workflow 定義・運用手順・ADR 記述）と代替ランタイム/実行方式・移行ステップを定義（実装/配線は別判断） |

## 5. アーキテクチャ/ドキュメント整合性（優先度: 中）

| ID | タスク | 現状・根拠 | 次のアクション（案） |
|---|---|---|---|
| DOC-1 | ADR-0001 未チェック項目の棚卸し・更新 | [adr/0001-unified-architecture.md](../../adr/0001-unified-architecture.md) に未チェック項目が残る（OCI A1+Tailscale 疎通、1Password CLI 設定、同一ワークスペース共有確認、Grafana Cloud アラート、Idle Compute 対策、review-flow CI ゲート統合）。OCI 前提項目は ADR-0004 により陳腐化。 | 「完了」「不要/置換」「継続タスク」へ再分類し、注記または別 ADR/タスク化 |
| DOC-2 | 参照ドキュメントの未解決事項の要否判断 | `docs/architecture/mpe-rust-latex-architecture-report.md` §12 / `docs/architecture/opencode-v2-oci-code-server.md` §未解決事項 に未解決事項があるが、いずれも提案/参照時点の検証・設計選択事項（MPE実機検証、旧OCI構成前提の選択肢等）であり、現行 JARVIS ローカル運用のアクティブタスクとは別。 | **保留（参照資料扱い）**: 該当テーマを実施する場合に個別タスク化する |

## 6. 運用上のフォローアップ（優先度: 低〜中）

| ID | タスク | 現状・根拠 | 次のアクション（案） |
|---|---|---|---|
| OPS-1 | MCP クライアント再接続の運用明文化 | gateway 再起動後に MCP セッションが失効し再初期化が必要（前ゴールで `tools/call` 等が `Unauthorized: Session not found` となる事象を確認）。2026-11-15: [dsh.md](dsh.md) / [tool-matrix.md](tool-matrix.md) に再接続手順の注記を追記。 | クライアント実装側の自動再接続/再初期化（失敗時リトライ含む）の要否を検討し、必要なら実装タスク化 |

## メモ

- 本一覧は「実装タスク」だけでなく「方針決定が先に必要な項目」を含みます（特に BK-1, MCP-1, SEC-1, CI-1）。
- 実機変更・デプロイ・秘密情報変更・外部サービス操作を伴う項目は、別途の確認と手順に従って進めてください。
