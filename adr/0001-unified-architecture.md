---
type: Architecture-Decision
title: "ADR-0001: Unified JARVIS Architecture - OpenCode V2 as Agent Runtime"
description: "統合アーキテクチャ決定: JARVIS Personal AI と OpenCode V2 OCI Code Server の境界・重複・依存を解消し、OpenCode V2 を JARVIS の公式 Agent Runtime および開発 IDE として位置づける。"
tags:
  - adr
  - jarvis
  - opencode
  - oci
  - architecture
status: accepted
deciders:
  - "Sovereign AI Agent"
date: "2026-10-09"
---

## Context

現在、以下の2ドキュメントが独立して存在し、境界・責務・用語に重複・不整合がある。

1. `jarvis_personal_ai_architecture.md` - JARVIS全体アーキテクチャ (スマホ・HA・サーバー・LLM・Memory・Policy)
2. `opencode-v2-oci-code-server-architecture-report.md` - OCI上の code-server/OpenCode

**主な不整合:**

- "Agent Runtime" が `OpenClaw` (JARVIS側) と `OpenCode V2` (OCI側) で二重定義
- MCP導入順・管理が別個
- 秘密情報管理・ネットワーク境界・ストレージ戦略が各々で異なる前提
- 用語: "JARVIS Server" vs "OCI VM"、"MCP Host" vs "OpenCode MCP"

---

## Decision

### 1. OpenCode V2 を JARVIS の **公式 Agent Runtime** として採用

| 要素 | 採用 | 備考 |
| ---- | ---- | ---- |
| Agent 実行基盤 | **OpenCode V2** (`opencode serve` + TUI/CLI) | 自前実装は中止 |
| IDE | **code-server** (同一VM内、systemd管理) | ブラウザIDE担当 |
| MCP Host | **OpenCode 内蔵 MCP Client** | JARVIS側の `MCPHost` を委譲 |
| モデル切替/フォールバック | **OpenCode 内蔵 Model Router** | Tier 0-4 をマッピング |
| セッション管理 | **OpenCode セッション** | 永続化は仕様に従う |
| 非対話実行 | **OpenCode CLI (`opencode run`)** | CI/CD・バッチ処理で利用 |

### 2. JARVIS 固有コンポーネントを **Rust 製サイドカーサービス** として分離

OpenCode ではカバーできない **セキュリティ境界・ポリシー・文脈構築** のみ自前実装:

| サービス | 責務 | 技術 |
| -------- | ---- | ---- |
| `jarvis-policy` | Policy Engine (Cedar/Casbin)、承認ゲート、キルスイッチ | Rust + Cedar |
| `jarvis-mcp-gw` | MCP Gateway (allowlist、サンドボックス、能力ベース認可) | Rust + rmcp |
| `jarvis-event` | Event Normalizer (HA/Android/外部API→統一イベント) | Rust |
| `jarvis-context` | Context Builder (Memory検索、PII除去、プロンプト組立) | Rust |

**通信:** OpenCode ⇄ JARVIS Rust サービスは **ローカル gRPC/Unix Socket** / **HTTP/JSON** (MCP拡張)

### 3. インフラ・運用の統一

| 領域 | 統一方針 |
| ---- | -------- |
| **IaC** | OpenTofu 単一リポジトリ (`infra/opentofu/`) |
| **プロセス管理** | systemd 全サービス (`infra/systemd/`) |
| **秘密情報** | **1Password CLI (`op`)** で注入、Git/state排除 |
| **バックアップ/DR** | OCI Object Storage + rclone (cron/systemd timer) |
| **監視** | OpenTelemetry Collector → Grafana Cloud Free |
| **ネットワーク** | Tailscale のみ公開、code-server/OpenCode Web は 127.0.0.1 バインド |
| **コンテナ** | 初期は **なし** (systemd ネイティブ)。将来 Podman/Docker Compose 移行パスのみ確保 |

### 4. 用語の統一 (Ubiquitous Language)

| 従来 (JARVIS側) | 従来 (OCI側) | 統一用語 |
| ---------------- | ------------ | -------- |
| JARVIS Server | OCI VM / Code Server Host | **JARVIS Host** |
| OpenClaw / Agent Runtime | OpenCode V2 Server | **Agent Runtime (OpenCode)** |
| MCP Host | OpenCode MCP | **MCP Gateway (jarvis-mcp-gw)** |
| Policy Engine | (なし) | **Policy Engine (jarvis-policy)** |
| Long-term Memory | (OpenCode Session) | **Memory Store (PG + Qdrant)** |
| Audit Log | (なし) | **Audit Log (JSON → Object Storage)** |

---

## Consequences

### Positive

- 自前 Agent ランタイム実装コスト **削減** (OpenCode: セッション、MCP、モデル切替、UI、非対話実行を標準装備)
- セキュリティクリティカルな境界のみ **Rust で堅牢に実装** (Policy、MCP GW、キルスイッチ)
- 開発環境 (code-server) と Agent Runtime (OpenCode) が **同一ワークスペースを共有** → 開発ループ高速化
- 単一 IaC・単一プロセス管理・単一秘密管理で **運用簡素化**

### Negative / Risks

- OpenCode V2 の仕様変更・破壊的変更への追従リスク → バージョン固定 + 変更監視
- OpenCode の MCP 実装が JARVIS 要件を満たすか要検証 → `jarvis-mcp-gw` でラップ吸収
- 単一 VM (OCI A1) への集約 → Idle Compute 回収時の全停止リスク → **自動再プロビジョニング必須**

### Neutral

- OCI Always Free 枠内で完結 (2 OCPU / 12GB / 200GB Block + 10TB Object)
- Arm64 互換性確認必須 (OpenCode, code-server, Rust toolchain, PostgreSQL, Qdrant すべて対応済み)

---

## Implementation Plan

> 進捗メモ (2026-10-31): `[x]` はリポジトリ成果物としての実装完了を示す。
> 実機 (OCI) での稼働・疎通確認はデプロイ時に別途行う。
>
> **2026-11-15 更新（ADR-0004 反映）**: インフラ前提は OCI からローカル Kubuntu + systemd 運用へ変更。
> OCI 固有の未実施項目は「非適用（ADR-0004）」、前提が変わった項目は「要再評価」として扱う。
> 全体の現行残タスク整理は [docs/integration/remaining-tasks.md](../docs/integration/remaining-tasks.md) を参照。
>
> **2026-11-15 追記（DOC-1 継続タスク参照）**: 本 ADR の未チェック/要再評価項目は backlog で個別管理する。
> - Phase 0「1Password CLI 設定・秘密情報注入確認」: 現行は `bws` 利用が主、`otelcol` 等の実機/設定整合は **SEC-1**（[remaining-tasks.md](../docs/integration/remaining-tasks.md)）で要確認。
> - Phase 1「同一ワークスペース共有確認」: ローカル運用での現行要件・実態の確認が必要（未ID化、要再評価）。
> - Phase 4「Grafana Cloud アラート」: 方針・設定確認が必要（継続タスク）。
> - Phase 4「rclone バックアップ」: 成果物はあるが OCI 前提・現行未運用。保存先/認証/運用化は **BK-1, BK-2, SEC-2**。
> - Phase 4「review-flow CI ゲート統合」: `ci/review-gate.yml` 未配線、`opencode run` 依存整理含め **CI-1, CI-2**。
> - 関連する運用/セキュリティフォローアップ: **MCP-4**（読み取り基準ディレクトリ縮小）、**OPS-1**（gateway 再起動後の MCP 再初期化）。

### Phase 0: 統合基盤 (Week 1)

- [x] この ADR 承認・マージ
- [ ] OpenTofu で OCI A1 VM + Tailscale 疎通 — **非適用（ADR-0004: OCI 不使用、ローカル運用）**
- [ ] 1Password CLI 設定・秘密情報注入確認 — **要再評価（現行は `bws` 利用、`otelcol` 等の移行状況は要確認）**

### Phase 1: 開発環境 + Agent Runtime (Week 1-2)

- [x] code-server systemd 化
- [x] OpenCode V2 インストール・`opencode serve` systemd 化
- [ ] 同一ワークスペース (`~/repos`) 共有確認 — **要再評価（未ID化、ローカル運用での現行要件・実態を確認）**

### Phase 2: JARVIS Rust サービス雛形 (Week 2-3)

- [x] Cargo ワークスペース作成 (`rust/`)
- [x] `jarvis-policy` (Cedar ポリシー + gRPC)
- [x] `jarvis-mcp-gw` (rmcp + allowlist)
- [x] `jarvis-event` (HA/Android Webhook 受信 → 統一イベント)
- [x] `jarvis-context` (PostgreSQL/Qdrant クライアント + PIIフィルタ)

### Phase 3: 統合・ポリシー適用 (Week 3-4)

- [x] OpenCode → `jarvis-policy` / `jarvis-mcp-gw` 連携 (MCP ツールとして登録)
- [x] キルスイッチ実装・テスト
- [x] キルスイッチ状態の永続化 (`/var/lib/jarvis/kill-switch.json`)
- [x] サイドカーの認可を policy gRPC に集約 (fail-closed)
- [x] イベント駆動フロー: HA → jarvis-event → jarvis-context → OpenCode → Policy → MCP GW

### Phase 4: 観測性・DR・自動化 (Week 4-5)

- [x] OpenTelemetry Collector 導入
- [x] サービスからの OTLP トレースエクスポート (`OTEL_EXPORTER_OTLP_ENDPOINT`)
- [x] Prometheus メトリクス公開 (`/metrics`)
- [x] Grafana Cloud ダッシュボード (`infra/grafana/dashboards/jarvis-overview.json`)
- [ ] Grafana Cloud アラート — **継続タスク（未ID化、要方針・設定確認）**
- [x] rclone バックアップ (PostgreSQL dump, Qdrant snapshot, 設定ファイル) — **要再評価（成果物としては存在するが、OCI Object Storage 前提・現行ローカルでは未運用。ADR-0004/BK-1 参照）**
- [ ] Idle Compute 対策: ヘルスチェック + `tofu apply` 自動再実行 (systemd timer) — **非適用（ADR-0004: OCI 不使用）**
- [ ] review-flow CI ゲート統合 — **継続タスク（`ci/review-gate.yml` は未配線、CI-1 参照）**

---

## References

- [OpenCode V2 Docs](https://opencode.ai/v2/docs)
- [OpenCode V2 MCP](https://opencode.ai/v2/docs/mcp-servers)
- [Cedar Policy Language](https://www.cedarpolicy.com/)
- [rmcp - Rust MCP SDK](https://github.com/rmcp-rs/rmcp)
- [OpenTofu](https://opentofu.org/)
- [OCI Always Free](https://docs.oracle.com/en-us/iaas/Content/FreeTier/freetier_topic-Always_Free_Resources.htm)
- [1Password CLI](https://developer.1password.com/docs/cli/get-started/)
