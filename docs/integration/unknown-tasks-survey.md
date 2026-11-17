---
type: Reference
title: "JARVIS 未知残タスク・機能追加案 調査レポート（2026-11-17）"
description: "5観点のエージェント並列調査と6回目の重複吸収・妥当性評価で洗い出した、既存 remaining-tasks.md 未掲載の残タスク・機能追加案"
tags:
  - survey
  - remaining-tasks
  - jarvis
generated:
  by: "Agent team (5 survey agents + 1 synthesis agent)"
  at: "2026-11-17T20:14:01+09:00"
verified:
  by: "lead (Runtime Proof: high-priority 5 findings)"
  at: "2026-11-17T20:14:01+09:00"
status: active
---

## 概要

エージェントチームをふんだんに使う指示に従い、5観点（Rust コード／インフラ／ドキュメント／UI・DSH／セキュリティ運用）を並列調査し、6回目の統合エージェントで重複吸収・妥当性評価を行った結果である。正本は [remaining-tasks.md](remaining-tasks.md) とし、そこに載っていない項目を中心に採用した。

| 項目 | 内容 |
|---|---|
| 入力 | 5観点・49件（RC 9 / IN 10 / DC 10 / UX 10 / SE 10） |
| 判定 | 採用 35件（高5 / 中20 / 低10）、保留 7件、却下 0件 |
| 重複吸収 | 7グループを統合（2件→1件ずつ、計7件を吸収）。49 − 7 = 42件を個別判定 |
| メソッド | ①正本 `remaining-tasks.md` 全182行を精読し既知 ID（MCP-1〜4 / BK-1〜4 / SEC-1,2 / CI-1,2 / DOC-1,2 / OPS-1）を確定 ②全 finding を「根本原因・対象ファイル・対策」でクラスタリング ③高優先度・却下判断に関わる14箇所を `grep` / `read` で確認 ④既知の再掲は採用表から除外し「境界」へ回す |
| 信頼度 | 高＝該当行を直接確認／中＝具体的なファイル:行があり制御フローから妥当だが実機・実行未検証／低（保留）＝推測・再現根拠なし・設計判断依存 |

### 親エージェントによる検証（Runtime Proof）

統合エージェントの「Runtime Proof 確認済み」主張を無条件採用せず、親エージェントが高優先度5件を直接 `read` で再確認した。結果は以下のとおりで、5件すべて証拠と一致した。

| ID | 検証した行 | 確認結果 |
|---|---|---|
| A-H1 | `infra/config/otelcol-config.yaml:6,8,10,16,18` | 5端点すべて `0.0.0.0`。`:163` も含む説明は統合側の報告で、確認した5端点で矛盾なし |
| A-H2 | `infra/scripts/check-deploy-drift.sh:19-31` | allowlist 欠落時に `if` ブロック全体をスキップし、`:26-30` の `web.fetch` danger 判定も走らない |
| A-H3 | `rust/jarvis-policy/src/config.rs:38,66`, `rust/jarvis-policy/src/lib.rs:35-48` | 既定値 `PLACEHOLDER_KILL_SWITCH_TOKEN` = `"changeme"` と opt-in フィールド（既定 false）を確認。`lib.rs` は flag が true のときのみ拒否し、既定では `tracing::warn!` で起動を継続 |
| A-H4 | `rust/jarvis-context/src/builder.rs:72,80-102` | `:72` は `build_context` 内で `pii_filter.filter(context)` を適用。`:80` の `store_memory` は `content`/`metadata` を生のまま INSERT し、`:98-99` の Qdrant 登録にもフィルタ呼び出しなし |
| A-H5 | `ui/src/approvals.rs:111-125`, `:70-85` | `:117-121` で `error` を設定した直後の `:124` が `apply_poll(refresh)` を呼び、成功時に `:75` が `error = None` に上書き。`:78-83` はエラー時も `pending` を保持 |

なお、CI 実行・ビルド・実機 `ss -lnt` 等は未実施である。高信頼度とは「当該行の存在確認まで」を意味し、動作検証は各「次のアクション」に委ねる。

## 採用：未知の残タスク・機能追加案

### 高優先度

| ID | タイトル | 優先度 | 証拠 | 次のアクション | 出典観点 |
|---|---|---|---|---|---|
| A-H1 | otelcol が `0.0.0.0` にリッスンし「localhost only」方針と矛盾 | 高 | `infra/config/otelcol-config.yaml:6,8,10,16,18` が `0.0.0.0`。`infra/systemd/otelcol.service:9,18` は `User=koppe`・直接実行で境界なし | 全端点を `127.0.0.1` へ変更し、実機 unit と合わせて `ss -lnt` で確認。方針文書との整合も明記 | IN / INF-1 |
| A-H2 | drift 検査が allowlist 欠落を見逃し fail-open | 高 | `infra/scripts/check-deploy-drift.sh:19-31`：配備版 allowlist 不在だと allowlist 検査と `web.fetch` の danger 判定ごとスキップされ `drift`/`danger` が立たない | 欠落時に `say "allowlist" "MISSING"; danger=1` を設定し、事後ゲートが失敗する回帰テストを追加 | IN / INF-2 |
| A-H3 | kill switch 既定トークン `changeme` は opt-in strict ガードで拒否可能 | 高 | `rust/jarvis-policy/src/config.rs:38` が opt-in フィールド（既定 false）、`:66` が既定値 `PLACEHOLDER_KILL_SWITCH_TOKEN` = `"changeme"`。`rust/jarvis-policy/src/lib.rs:35-48` は flag=true のときだけ bail し、既定は warn で起動継続 | ガードは実装済み。本番有効化はトークン注入を値を見ずに確認した後に判断（[local-operations-runbook.md](local-operations-runbook.md) の P0-10 節） | SE / SEC-KS-01 |
| A-H4 | `store_memory` に PII フィルタ未適用 | 高 | `rust/jarvis-context/src/builder.rs:72` は組み立て時に `pii_filter.filter(context)` を通すが `:80` の `store_memory` は生の `content`/`metadata` を DB・Qdrant へ保存。※設計意図が「LLM 送信時のみ」なら保存時は対象外の可能性あり、要方針確認 | 保存前にも `pii_filter.filter()` を通すか、「保存データは対象外」と明文化するテストを追加。既存保存データの再スキャン方針も決定 | SE / PII-01 |
| A-H5 | 承認エラーが直後の再ポールで上書きされ消える＋接続障害時にボタンが有効なまま | 高 | `ui/src/approvals.rs:117-121` で `error` を設定した直後の `:124` が `apply_poll(refresh)` を呼び、成功時に `:75` が `error = None` に戻す。`:78-83` はエラー時も `pending` を保持し、`:180-195` のボタンは常に有効 | `error` をポール結果と独立保持し次回操作まで表示。エラー中は決定ボタンを無効化し最終成功時刻を表示（UI-2 と UI-5 を同一箇所の状態管理として統合） | UX / UI-2 + UI-5 |

### 中優先度

| ID | タイトル | 優先度 | 証拠 | 次のアクション | 出典観点 |
|---|---|---|---|---|---|
| A-M1 | MCP-GW gRPC `execute_tool` の本番 `unwrap()` | 中 | `rust/jarvis-mcp-gw/src/grpc.rs:28,34` の `from_str(&req.args).unwrap()` / `to_string(&result).unwrap()`、`:46` 経由で `serve_grpc` から本番起動 | gRPC 境界で JSON を `Result` 化し `Status::invalid_argument` を返す回帰テストを追加 | RC / RUST-1 |
| A-M2 | jarvis-event の unbounded channel（バックプレッシャーなし） | 中 | `rust/jarvis-event/src/normalizer.rs:31` の `mpsc::unbounded_channel()`、`:61` の `let _ = self.event_tx.send(event);`、`:118-128` の4 webhook が無制限 `emit` | `mpsc::channel(N)` + `try_send` またはドロップ metric を導入し、キュー上限を設定化。A-M3 と同時設計 | RC / RUST-2 |
| A-M3 | Webhook 4ルートに認証・署名検証なし | 中 | `rust/jarvis-event/src/normalizer.rs:118-127` に `/webhook/{ha,android,gmail,calendar}` を `post` 登録するのみ、ガード・署名検証なし | loopback 前提を明記した上で共有トークン／HMAC 署名検証と簡易レート制限を追加するか、受け入れリスクを ADR に記録 | RC + SE / RUST-5 + SEC-EX-01 |
| A-M4 | Grafana Cloud 資格情報の参照先が unit 側で未供給 | 中 | `infra/config/otelcol-config.yaml:128` が `${env:GRAFANA_CLOUD_AUTH}` を要求するが、`infra/systemd/otelcol.service` に `EnvironmentFile` も `bws run` もなし | 実機の `systemctl show otelcol -p Environment` と exporter 失敗ログを読み取り供給経路を確定し unit 注記を更新（既知 SEC-1 の具体化） | IN / INF-3 |
| A-M5 | `make verify-backup` が「読み取り専用」を謳いながら `/opt` へ書き込む | 中 | `Makefile:69-71`：説明は「読み取り専用・最新スクリプトを同期」、本文は `install -m 0755 ... /opt/jarvis/scripts/verify-backup.sh` と `sudo -u postgres` | 説明を「同期を伴う」に訂正するか、同期を `install-backup-timer --apply` 側へ移して真の read-only にする | IN / INF-4 |
| A-M6 | バックアップ unit に PostgreSQL/Qdrant への `Wants=` なし | 中 | `infra/systemd/jarvis-backup.service:4-5` は `After=` のみ、`:12` が `pg_dump -d jarvis`、`:68` が Qdrant へ curl | `Wants=postgresql.service qdrant.service` を追加し、`systemd-analyze verify` と手動起動で依存を検証 | IN / INF-5 |
| A-M7 | CI と `make ci-local` のゲート内容が非対称 | 中 | `Makefile:43-45` は secret_scan・legacy guards を実行するが、`.github/workflows/ci.yml:36-58` の `extra_gates` は arm64 クロスビルドと `cargo test --ignored` のみ。正本 `:13-15` は「同等」と記載 | secret scan と safety guards を `extra_gates` へ追加するか、正本の「同等」記述を修正 | IN / INF-6 |
| A-M8 | drift 検査対象が Cedar ポリシー・スキーマ・PII 設定に及ばない | 中 | `infra/scripts/check-deploy-drift.sh:19-47` は binary/allowlist/`casbin_policy.csv`/unit のみを比較。`policies.cedar`・`cedar_schema.cedarschema`・`pii_patterns.toml` は未検査 | 配備先の設定一覧を取得し対象3ファイルも `cmp` 対象へ追加（A-H2 と同じ枠組み） | IN / INF-7 |
| A-M9 | review-gate が runner 切替でも OpenCode CLI を無条件インストール | 中 | `ci/review-gate.yml:25,57-59,122-124,197-199,239-241`：`JARVIS_REVIEW_RUNNER` 切替可だが4ジョブの `Install OpenCode CLI` に `if:` なし | CI-2 に合わせ、代替 runner 選択時にインストールをスキップする条件を追加するか制約を明記 | DC / CI-3 |
| A-M10 | ADR-0001 / AGENTS.md の記述が現状と乖離 | 中 | `adr/0001-unified-architecture.md:125,127`（bws 移行済み・手動 gate 配線済み）vs `:134,167`（1Password「要再評価」・review-gate「未配線」のまま `[ ]`）。`AGENTS.md:54,106-107` も「not wired」と記載 | 2026-11-17 追記に合わせ「手動 dispatch のみ配線済み／PR 自動実行・auto-merge は未配線」と区別して更新 | DC / DOC-3 + DOC-4 |
| A-M11 | バックアップ文書のブロッカー節が陳腐化し暗号化見解も矛盾 | 中 | `docs/integration/backup-redesign-notes.md:143-147` は「保存先未決定・暗号化未決定」をブロッカーとするが、同 `:187-193` は保存先決定・実装、正本 `:57` は決着扱い | ブロッカー節を「2026-11-17 解消」と更新し、未解決は timer 実機適用のみと明示。暗号化要否は両ドキュメントを同一見解へ揃える | DC + SE / DOC-5 + BK-05 |
| A-M12 | ADR の OKF メタデータがテンプレートと不一致 | 中 | `adr/0003-native-web-fetch.md:1` と `adr/0004-local-only-no-oci.md:1` は `#` で開始（frontmatter なし）。`adr/0001:14`,`0002:7`,`0005:14` と `docs/adr/template.md:3` の `date` は `YYYY-MM-DD` で RFC3339 違反 | 自社テンプレートに従い `type`/`title`/RFC3339 `date` を付与し、本文見出しを `##` 起点へ揃える。テンプレートも併せて改める | DC / DOC-7 + DOC-8 |
| A-M13 | dsh.md の「承認UI・通知・監査永続化は今後の拡張」が陳腐化 | 中 | `docs/integration/dsh.md:125` が今後の拡張とする一方、正本 `:48` と `ui/src/approvals.rs:1-3` は GPUI 承認コンソールが実装・E2E 済みを示す | 承認UI は実装済みに訂正し、未実装の通知・監査永続化のみを残タスクとして分離 | DC + UX / DOC-6 + DSH-1 |
| A-M14 | 承認・却下が1クリックで即確定 | 中 | `ui/src/approvals.rs:180-195`：`div().on_click()` のみ、確認ダイアログ／二段階操作なし | 破壊的ツールのみ再確認ダイアログを挟む。A-H5 のボタン無効化と同じ状態管理に載せる | UX / UI-1 |
| A-M15 | キーボード操作・アクセシビリティ属性が皆無 | 中 | `ui/src/approvals.rs:172-196`：ボタンが `div` + `on_click` のみ、`role`／フォーカス／キー操作なし | Tab フォーカス、Enter/Space 決定、`role=button` 相当の意味づけを追加 | UX / UI-3 |
| A-M16 | 決定済み要求の履歴表示・CSV/JSON エクスポートなし | 中 | `ui/src/approvals.rs:14-22` は状態が `pending` のみ。`docs/integration/dsh.md:125` の監査永続化は今後の拡張 | 決定履歴をローカル保持し CSV/JSON エクスポートを追加（DB 監査＝A-R7 とは分離） | UX / UI-4 |
| A-M17 | ntfy 通知が DSH の permission prompt のみで承認キューと未連携 | 中 | `cordis.patch.yml:75-89` は permission prompt 用、`ui/src/approvals.rs:11-12` は別系統のポーリング。`dsh.md:125` も「通知は今後の拡張」 | policy の pending を ntfy へ橋渡しする薄い連携を追加（CFG-2 の公開トピック問題を同時に解消する設計にする） | UX / CFG-1 |
| A-M18 | kill switch の poison／永続化失敗時に fail-closed へ回復しない | 中 | `rust/jarvis-policy/src/kill_switch.rs:82-83,92-93,124-125,146-147,182-183` の `read()/write().unwrap()`（poison 時 panic）、`:162-167` は `std::fs::write` 失敗を warn のみで復帰 | poison 時はキルスイッチ有効側へ回復するヘルパーへ置換し、永続化失敗時は arm（fail-closed）へ切り替えて監査ログ・alert に飛ばす | RC + SE / RUST-7 + SEC-KS-02 |
| A-M19 | PII フィルタが文字列以外と Object キーを走査しない | 中 | `rust/jarvis-context/src/pii.rs:86-105` は `Value::String` のみ置換、数値型・キー名は対象外 | **2026-11-18: 値のみ走査・キー/数値/bool/null は非対象としてテストで明文化。キー redact は JSON 構造を壊すため判断待ち**（A-H4 と同時に方針確定） | SE / PII-02 |
| A-M20 | `web.fetch` の宛先ドメイン制限（egress allowlist）未実装 | 中 | `docs/integration/tool-matrix.md:151` が未実装と明記、`rust/jarvis-mcp-gw/src/sandbox.rs:348-375` は公開 IP 判定のみでドメ固有の許可リストなし | 許可ドメイン一覧を config 化し、リダイレクト先にも同一リストを適用。A-R4 の接続時 IP 再検証も同じ経路で検討 | SE / SEC-SSRF-02 |

### 低優先度

| ID | タイトル | 優先度 | 証拠 | 次のアクション | 出典観点 |
|---|---|---|---|---|---|
| A-L1 | context `search_memory` の `limit` に上限なし | 低 | `rust/jarvis-context/src/grpc.rs:80`、`http.rs:52`、`builder.rs:271` で `limit` をそのまま Qdrant へ渡す | `max_search_limit`（例 100）を config に追加し、超過時に clamp／エラー化するテストを追加 | RC / RUST-3 |
| A-L2 | allowlist の `timeout_ms` が未検証・経路で clamp が不一致 | 低 | `rust/jarvis-mcp-gw/src/allowlist.rs:38-45` は TOML parse のみ、`sandbox.rs:99` は `.clamp(1_000, 15_000)`、`gateway.rs:179` は生 `Duration::from_millis` | `AllowedTool` 読み込み時に `timeout_ms`/`max_args_size` の範囲を検証し、全呼び出し経路で同じ clamp を共有 | RC / RUST-4 |
| A-L3 | context gRPC が不正 JSON/ID を黙って既定値に化ける | 低 | `rust/jarvis-context/src/grpc.rs:30,33,53,56` の `unwrap_or_else(new_v4)` / `unwrap_or_default()` | 入力不正を `Status::invalid_argument` で返し、生成 ID は呼び出し側が明示する方式へ変更 | RC / RUST-6 |
| A-L4 | `#[allow(dead_code)]` で未使用フィールドを抑制 | 低 | `rust/jarvis-event/src/normalizer.rs:11` struct 全体に属性、`:13-17` の `ha_client`/`android_client` 等は参照箇所なし | 未使用クライアントを削除するか、`sources.rs:70,265` の WebSocket 取得への配線を実装 | RC / RUST-9 |
| A-L5 | systemd unit コメントが旧 1Password 前提のまま | 低 | `infra/systemd/jarvis-policy.service:14`、`jarvis-event.service:14`、`jarvis-context.service:14` が 1Password 前提の記載だが `ExecStart` は `bws run` | 3 unit のコメントを `bws run` + `/etc/jarvis/bws.env` 前提へ書き換え | IN / INF-8 |
| A-L6 | Grafana ダッシュボードがラベル未設定に依存し手動 import 固定 | 低 | `infra/grafana/README.md:19-21`、`:38-39` | sidecar 側へ request-path ラベル追加を検討し、import 手順を runbook 化 | IN / INF-10 |
| A-L7 | `docs/architecture/AGENTS.md` が存在しない資料を参照 | 低 | `docs/architecture/AGENTS.md:4` が `markdown_mpe_rust_latex_architecture_report_2026-10-09.md` を指定するが実在は `mpe-rust-latex-architecture-report.md` | 参照先を実在ファイル名へ修正 | DC / DOC-9 |
| A-L8 | ADR-0002 の残課題が未 ID 化・未追跡 | 低 | `adr/0002-runtime-agnostic.md:65`「ACP session / doctor の自動契約テストは未実装」に相当 ID なし | 「範囲外」で固定するか CI-3 等として残タスク一覧に登録し判断を記録 | DC / DOC-11 |
| A-L9 | ポーラーが非フォーカス時も3秒間隔で常時稼働 | 低 | `ui/src/approvals.rs:11-12,38-67`：ウィンドウ存活中は無条件ループ | 非アクティブ時に間隔延長または一時停止を検討（A-H5 と同じ箇所） | UX / UI-6 |
| A-L10 | トークン注入時に env 未設定・値不一致の事前検証なし | 低 | `cordis.patch.yml:42-46` が `${GITHUB_MCP_TOKEN}` を注入、`ui/src/main.rs:22` は承認トークン env のみ | 起動時に未設定を検出して明示失敗させ、`failOnStartupError: false` と整合 | UX / CFG-3 |

## 保留・却下

| 元 ID | 判定 | 理由 | 吸収先／再開条件 |
|---|---|---|---|
| RUST-8 | 保留 | 新規性が「推測」。gRPC 経由のみ principal をクライアントが選択できるが、50052 が実際に loopback 限定かは未確認 | `ss -lntp \| grep 50052` で確認後、必要なら gRPC もサーバ側 principal 固定へ |
| DOC-10 | 保留 | OKF 索引規約の ADR ディレクトリへの適用範囲が未定義で、設計判断が必要 | ADR を索引規約対象外と明文化するか、`adr/index.md` 追加を方針決定後に判断 |
| CFG-2 | 保留 | `ntfy.sh` + 固定 topic は確認できるが通知ペイロードに機微な引数が含まれるか未確認で、漏洩リスクの根拠が不足 | A-M17 の実装時にペイロードを確認し、非公開トピック化を決定 |
| CFG-2 | 保留継続・実装見送り | - | 後述の補足を参照 |
| SEC-SSRF-01 | 保留 | DNS rebinding による TOCTOU はコード構造から可能性はあるが再現根拠なし。推測を機能バグ扱いしない基準に該当 | A-M20 実装時に接続時 IP 再検証を同時検討 |
| OBS-01 | 保留（既知の再掲） | `adr/0001-unified-architecture.md:164` と正本 DOC-1「Grafana Cloud アラート」に既出 | 正本 DOC-1 更新時に「kill switch 発動・承認タイムアウト・バックアップ失敗」を通知対象として追記 |
| INF-9 | 保留（既知の再掲・証跡として吸収） | BK-3 の既知判断に、`decommission-oci.sh:30,49-50,55` の dry-run でも state pull / plan 生成が走るという証跡を追加したもの | BK-3 の判断材料中に記録 |
| AUD-01 | 保留（既知の再掲） | `rust/jarvis-policy/src/approval.rs:1-6` の監査永続化 TODO は正本 MCP-1 と同一内容 | A-M13 / A-M16 で要件を分離した上で MCP-1 更新時に最小監査テーブル設計を位置づける |

**CFG-2 補足（2026-11-18）**: ntfy は DSH の permission prompt 用で、
jarvis-policy の承認キューとは別系統。公開トピックで機微な `context` を
流すリスクがあるため橋渡しは実装しない。非公開トピック／セルフホストと、
id・resource のみの通知契約を先に確定する（判断待ち）。

## 既知タスクとの境界

| 採用 ID | 関連する既知 ID | 境界の説明 |
|---|---|---|
| A-H1, A-M4 | SEC-1 | SEC-1 は unit 差分の解消済みタスク。A-H1 は配置アドレス、A-M4 は資格情報の供給経路未特定を具体化 |
| A-H2, A-M8 | MCP-3 | デプロイ経路の整合性は「更新済み」とあるが、欠落 fail-open と対象外ファイルは未カバー |
| A-H3, A-M18 | MCP-1 | 承認フローの fail-closed は実装済み。その前提となる kill switch 本体の fail-closed が抜けていた |
| A-H4, A-M19 | （正本に ID なし） | 「PII filtering on all context sent to LLMs」は送信時のみを述べており、保存時は未定義という新しい境界 |
| A-H5, A-M14, A-M15, A-L9 | MCP-1 | 承認 API／GPUI コンソールは実装済み。UI 側のエラー表示・誤操作保護・アクセシビリティは未カバー |
| A-M5, A-M6 | BK-2 | timer/service のローカル運用化（実機未導入）の unit 依存欠落と検証手順の説明不一致 |
| A-M7, A-M9 | CI-1, CI-2 | 配線後の残りとして、ゲート非対称と OpenCode 無条件インストールを指摘 |
| A-M10 | CI-1, DOC-1 | DOC-1 の更新方針に沿って bws 移行済み／手動 gate 配線済みの証跡を反映 |
| A-M11, A-L5 | BK-1, BK-3, BK-4, SEC-2 | 実装側は更新済みだがドキュメント／unit コメント側が未更新 |
| A-M12, A-L7, A-L8 | DOC-1, DOC-2 | 既知 ID は内容に関するもの。形式・参照ファイル名・残課題 ID 化は別軸 |
| A-M13, A-M16, A-M17, A-L10 | MCP-1, OPS-1 | dsh.md 記述同期、ローカル履歴、通知連携、起動時検証。`failOnStartupError: false` と整合 |
| A-M20 | MCP-2 | `web.fetch` 実装時に必要な egress 制限を先に定義するもの |
| 保留 OBS-01 | DOC-1 | 「Grafana Cloud アラート」として既に分類済み |

## 6回目の吸収メモ

### 重複の統合方法

1. キーは「根本原因＋対象ファイル」で統一した。ID の接頭辞（RUST / INF / DOC / UI / CFG / SEC）は観点の違いにすぎないため、同じコード行・同じドキュメント節を指すものは1行へまとめた。
2. 統合した7グループ: ①Webhook 認証（RC+SE）②kill switch fail-closed（RC+SE）③dsh.md 陳腐化（DC+UX）④ADR OKF 準拠（DC 内）⑤バックアップ文書矛盾（DC+SE）⑥ADR-0001/AGENTS.md 記述同期（DC 内）⑦承認 UI エラー状態管理（UX 内）。優先度は最も高い証拠側に寄せ、出典観点と元 ID を残した。
3. 同じファイルでも対策が別なら統合しなかった。例: `check-deploy-drift.sh` の A-H2（欠落 fail-open）と A-M8（対象外ファイル）は、前者がゲート設計、後者が `cmp` 対象追加で独立して修正・検証できるため別行とした。
4. 既知の再掲は採用表へ載せず「境界」へ回した。ただし新証跡がある場合は「既知 ID の具体化」として採用表に残し、出典にその旨を明記した（A-M4 = SEC-1 の具体化）。

### 妥当性判断基準

1. Runtime Proof 優先: 自ら `grep` / `read` で当該行を確認できたものは高信頼度として採用。委譲元の証拠と矛盾した場合は現行コードを優先する（今回は矛盾なし）。
2. 具体性と制御フロー: ファイル:行が明記され、コード構造から影響が妥当と導けるものは中で採用。実機・実行検証は未実施と明記し、未確認を推測に格上げしない。
3. 推測は保留: RUST-8 / DOC-10 / CFG-2 / SEC-SSRF-01 は再現根拠なし。無条件採用せず、再開条件（実機確認・ペイロード確認・設計判断）を必ず添えた。
4. 既知再掲は新規 ID を与えない: 正本に同一内容の ID がある場合は「保留（既知の再掲）」とし、既知 ID の更新項目として吸収。件数インフレを防止した。
5. 優先順位: ①公開境界・認証・既定資格情報 → ②ゲートの fail-open → ③データ漏洩・保存リスク → ④ユーザー誤操作 → ⑤ゲート不整合・ドキュメント矛盾 → ⑥保守性・軽微な設計改善。セキュリティ・認証変更は敵対的にレビューし、性能・推測系は機能バグと混ぜなかった。

### 特に留意した点

A-H4（PII 保存時フィルタ）は「PII filtering on all **context sent to LLMs**」という既存記述と微妙に緊張関係にあるため、採用しつつも「設計意図が送信時のみなら保存時は対象外の可能性」を証拠列に残した。また A-M20 と SEC-SSRF-01 は同じ `web.fetch` でも、前者は明記済みの未実装要件、後者は再現なき推測、という違いで採用／保留を分けた。

## 既存 todo.md との重複吸収

プロジェクトルートの [todo.md](../../todo.md) は、同じ2026-11-17に「5ラウンド×5視点＝25エージェント」で実施されたクロスレビューの集約である。「6回目で重複を吸収する」指示に従い、本レポートの35件を同ファイルの P0-1〜P3-7 と照合した。

| 判定 | 本レポート ID | todo.md の対応 |
|---|---|---|
| 重複（吸収） | A-H1 / A-H5 / A-M4 / A-M5 / A-M7 / A-M9 / A-M10 / A-M12 / A-M13 / A-M19 / A-L2 / A-L5 / A-L7 / A-L9 / A-L10 | P0-6 / P0-8 / P0-7 / P3-3 / P2-4 / P2-2 / P3-1 / P1-10 / P3-2 / P2-1 / P3-7 / P3-1 / P3-2 / P3-6 / P3-6 |
| 部分重複（新証跡として補足） | A-M1 / A-M6 / A-M11 / A-M14 / A-M16 / A-M18 / A-M20 / A-L1 / A-L3 / A-L6 | P0-3 / P1-5 / P3-3 / P3-6 / P3-6 / P0-4 / 正本 MCP-2 / P3-7 / P3-7 / P0-7 |
| 新規（todo.md 未掲載、追記対象） | A-H2 / A-H3 / A-H4 / A-M2 / A-M3 / A-M8 / A-M15 / A-M17 / A-L4 / A-L8 | — |

新規10件は [todo.md](../../todo.md) の「追加調査」節へ集約した。本レポートは個別の証拠と吸収過程の詳細ログとして残す。

## メモ

- 本レポートは調査結果であり、実装・設定変更は行っていない。各「次のアクション」は提案であり、実行には別途の判断と標準検証ループが必要である。
- 5観点の調査エージェントと1統合エージェントを起動した。初回ワークフローでは3観点と統合が `null` を返したため、プロンプトを簡素化して再実行し、5観点＋統合のすべてを回収した。
