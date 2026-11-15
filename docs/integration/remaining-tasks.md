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
| MCP-3 | allowlist `sandbox_type`（ツール単位）の整理 | allowlist 各ツールに `sandbox_type` があるが、`AllowedTool` は当該フィールドを持たず読み取らない（[infra/config/mcp_allowlist.toml](../../infra/config/mcp_allowlist.toml), `rust/jarvis-mcp-gw/src/allowlist.rs`, [tool-matrix.md](tool-matrix.md)）。実際の sandbox 種別は gateway 設定/環境変数側。 | 「死んだ設定を削除」か「ツール単位ルーティングを実装」を選択（変更時は repo と `/etc/jarvis` の整合・デプロイ手順も考慮） |
| MCP-4 | 読み取り基準ディレクトリの絞り込み | `ProtectHome=read-only` + `JARVIS_MCP_GW_BASE_DIR=/home/koppe` によりホーム配下全体が読み取り可能（[tool-matrix.md](tool-matrix.md)）。機微ファイルへの到達リスクが残る。 | 実運用に必要な最小ディレクトリへ限定し、drift/smoke テストで確認 |

## 2. バックアップ / DR・OCI レガシー整理（優先度: 高）

| ID | タスク | 現状・根拠 | 次のアクション（案） |
|---|---|---|---|
| BK-1 | バックアップ保存先の再決定と実装更新 | [adr/0004-local-only-no-oci.md](../../adr/0004-local-only-no-oci.md) で OCI 非使用・保存先再決定が未解決。`rust/scripts/backup.rs` / `restore.rs` / `infra/scripts/backup.sh` は OCI Object Storage(rclone)・`op inject`・`/opt/jarvis` `/home/jarvis` 前提のレガシー。 | 保存先（例: ローカル/NAS/別ストレージ）・保持期間・暗号化・復旧手順を決めてスクリプトを再設計 |
| BK-2 | バックアップ timer/service のローカル運用化 | `infra/systemd/jarvis-backup.service` / `.timer` は存在するがローカル未導入（not-installed）。unit 内容もレガシー前提（`User=jarvis`, `op.env`, `/run/rclone` 等）。 | BK-1 決定後に unit を現行環境へ合わせて導入、`systemctl enable --now` と復旧テストを実施 |
| BK-3 | OpenTofu/OCI 後始末資産の扱い決定 | [infra/opentofu/](../../infra/opentofu/) は未使用レガシー（[infra/opentofu/README.md](../../infra/opentofu/README.md)）。後始末用に [infra/scripts/decommission-oci.sh](../../infra/scripts/decommission-oci.sh) あり。`infra/opentofu/environments/prod/.terraform/terraform.tfstate` の存在を確認。 | 「保持」「削除」「OCI リソース削除実行」の方針決定。削除時は state 保持要否を確認（state 内容の妥当性は未確認） |
| BK-4 | レガシー設定テンプレートの整理 | `infra/config/*.env` に `op://` 参照が残る（`policy.env`, `context.env`, `event.env`, `otel.env`）。[AGENTS.md](../../AGENTS.md)/[infra/README.md](../../infra/README.md) では参照テンプレート扱い。 | 現行運用（bws/直接 env 等）に合わせた注記・整理で誤用を減らす（実機 secret の変更は別判断） |

## 3. シークレット管理・構成ドリフト（優先度: 中〜高）

| ID | タスク | 現状・根拠 | 次のアクション（案） |
|---|---|---|---|
| SEC-1 | otelcol unit の repo/実機差分の整合 | repo: [infra/systemd/otelcol.service](../../infra/systemd/otelcol.service) は `op run` + `/etc/jarvis/op.env` + `User=jarvis`。実機 `/etc/systemd/system/otelcol.service` は `User=koppe` かつ `op run` なしの直接起動（差分確認済み）。[AGENTS.md](../../AGENTS.md) には op→bws 移行未完了の記載あり。 | 実機の正しい起動/認証方式を確認し、repo か実機のどちらかに統一（変更時は監視停止リスクに注意） |
| SEC-2 | バックアップ系の 1Password 依存解消 | `backup.rs`/`restore.rs`/`jarvis-backup.service` が `op inject`・`OP_SERVICE_ACCOUNT_TOKEN` 等に依存。 | BK-1 の保存先/認証設計と合わせて解消 |

## 4. CI/CD・レビュー基盤（優先度: 中）

| ID | タスク | 現状・根拠 | 次のアクション（案） |
|---|---|---|---|
| CI-1 | review-gate の Actions 配線方針決定 | [ci/review-gate.yml](../../ci/review-gate.yml) は存在するが `.github/workflows/` には未配線（`.github/workflows/review-gate.yml` 不在）。[AGENTS.md](../../AGENTS.md), [adr/0001-unified-architecture.md](../../adr/0001-unified-architecture.md) にも未統合の記載。 | OpenCode 認証情報・runner 要件を踏まえ、配線/手動運用継続/代替手段を決定 |
| CI-2 | `opencode run` 依存の置換検討 | [adr/0002-runtime-agnostic.md](../../adr/0002-runtime-agnostic.md) に TODO。review-gate や運用手順にも `opencode run` 前提が残る。 | 置換範囲（review-gate, AGENTS 手順等）と移行ステップを定義 |

## 5. アーキテクチャ/ドキュメント整合性（優先度: 中）

| ID | タスク | 現状・根拠 | 次のアクション（案） |
|---|---|---|---|
| DOC-1 | ADR-0001 未チェック項目の棚卸し・更新 | [adr/0001-unified-architecture.md](../../adr/0001-unified-architecture.md) に未チェック項目が残る（OCI A1+Tailscale 疎通、1Password CLI 設定、同一ワークスペース共有確認、Grafana Cloud アラート、Idle Compute 対策、review-flow CI ゲート統合）。OCI 前提項目は ADR-0004 により陳腐化。 | 「完了」「不要/置換」「継続タスク」へ再分類し、注記または別 ADR/タスク化 |
| DOC-2 | 参照ドキュメントの未解決事項の要否判断 | `docs/architecture/mpe-rust-latex-architecture-report.md` / `docs/architecture/opencode-v2-oci-code-server.md` に未解決事項セクションあり。 | 現行プロジェクトのアクティブタスクか、参照資料として保留かを判断 |

## 6. 運用上のフォローアップ（優先度: 低〜中）

| ID | タスク | 現状・根拠 | 次のアクション（案） |
|---|---|---|---|
| OPS-1 | MCP クライアント再接続の運用明文化 | gateway 再起動後に MCP セッションが失効し再初期化が必要（前ゴールで `Session not found` を確認）。 | 再接続手順・影響範囲を運用ドキュメントに明記、必要なら自動復旧の検討 |

## メモ

- 本一覧は「実装タスク」だけでなく「方針決定が先に必要な項目」を含みます（特に BK-1, MCP-1, SEC-1, CI-1）。
- 実機変更・デプロイ・秘密情報変更・外部サービス操作を伴う項目は、別途の確認と手順に従って進めてください。
