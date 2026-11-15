# JARVIS Architecture Resource Analysis

*Document source: `report.md` | Calculations: `calc.rs` | Generated: `dist/`*

> **2026-11-15 注記（ADR-0004）:** 本書は **OCI Always Free (Ampere A1 Flex)**
> を前提としたリソース試算の参照資料です。OCI は現行の実行基盤ではありません
> （現行はローカル Kubuntu + systemd）。現行のインフラ方針・運用状況・残タスクは
> [ADR-0004: Local-only (no OCI)](../../adr/0004-local-only-no-oci.md) と
> [docs/integration/remaining-tasks.md](../integration/remaining-tasks.md)
> を参照してください。

---

## 1. 概要

本ドキュメントは、JARVIS Personal AI システムを **OCI Always Free (Ampere A1 Flex:
2 OCPU, 12 GB RAM)** 上で運用する際のリソース要件を、Rust で計算・検証した結果を示す。

- **文書原本**: この `report.md` (Markdown + LaTeX数式記法)
- **計算ロジック**: `calc.rs` (Rust、単体実行可能)
- **表示・実行**: Markdown Preview Enhanced (MPE) Code Chunk
- **共有成果物**: `dist/report.md` (計算済み), `dist/report.html`

> **注意**: MPE の `@import` 構文と Code Chunk 実行結果は、MPE プレビューでのみ反映される。
> GitHub 等の一般的な Markdown ビューアでは計算結果は表示されないため、共有時は `dist/` 以下の生成物を使用すること。

---

## 2. アーキテクチャ構成

JARVIS は以下のサービスを単一 VM 上で systemd 管理する：

| サービス | 役割 | 技術 |
| --- | --- | --- |
| `jarvis-policy` | Policy Engine (Cedar/Casbin), Kill Switch | Rust |
| `jarvis-mcp-gw` | MCP Gateway (Allowlist, Sandbox) | Rust |
| `jarvis-event` | Event Normalizer (HA, Android, Webhooks) | Rust |
| `jarvis-context` | Context Builder (PostgreSQL, Qdrant, PII) | Rust |
| `opencode` | Agent Runtime (OpenCode V2 Server) | TypeScript/Node |
| `code-server` | Browser IDE | TypeScript/Node |
| `postgresql` | Long-term Memory, Audit Log | PostgreSQL 14 |
| `qdrant` | Vector Memory | Qdrant |
| `otelcol` | OpenTelemetry Collector | Go |
| `system/os` | OS 基盤オーバーヘッド | Ubuntu 22.04 |

---

## 3. 算定式

各サービスのリソース要件を $r_i = (ocpu_i, mem_i, disk_i, cpu\_weight_i)$ とし、総需要を総和で求める：

$$
\begin{aligned}
OCPU_{total} &= \sum_{i} ocpu_i \\
Mem_{total} &= \sum_{i} mem_i \\
Disk_{total} &= \sum_{i} disk_i \\
CPU_{weighted} &= \sum_{i} ocpu_i \times cpu\_weight_i
\end{aligned}
$$

利用率は Always Free 上限 $L = (2.0, 12.0, 200.0)$ に対する比率：

$$
U_{resource} = \frac{Resource_{total}}{Resource_{limit}} \times 100\%
$$

月間使用時間 (24/7 稼働前提)：

$$
H_{monthly} = Resource_{total} \times 24 \times 30
$$

無料枠超過時のコスト試算 (東京リージョン従量課金)：

$$
C = \max(0, OCPU_{used} - 2.0) \times 720 \times \$0.0085
    + \max(0, Mem_{used} - 12.0) \times 720 \times \$0.0012
$$

---

## 4. 計算結果

以下の表は `calc.rs` を MPE Code Chunk として実行した結果である。

@import "calc.rs" {cmd="rust-script" output="markdown" hide=true}

---

## 5. 解釈と判断

> **Verdict**: ⚠️ 無料枠超過（保守合算ベース。数値根拠は §4 の計算出力を参照）

### 5.1 リソース適合性

§4 の通り、**サービス割当の保守的な同時合算では OCPU 需要が Always Free 上限を上回る**。
これは全サービスが同時に割当を使い切る最悪ケースの和であり、
`cpu_weight`（時間平均ビジー率の想定）による加重平均負荷は上限に対して十分小さい。
実運用上の拘束条件は **メモリ** である。

メモリは余裕が僅少なため、以下の運用制約を守る：

1. **同時ビルド・重いコンパイルは避ける** (Rust リリースビルドは一時的に 2-3 GB 追加消費)
2. **PostgreSQL shared_buffers は 512 MB 以下に制限**
3. **Qdrant キャッシュサイズは 1 GB 以下に制限**
4. **code-server 拡張は最小限に留める**

CPU の保守合算超過分は、systemd `CPUQuota` による上限強制と実行時間帯の平準化で吸収する。

### 5.2 スケーリング余地

- **Disk**: 余裕は計算出力の Headroom 列を参照。ログ・バックアップ・DB 成長に十分
- **Object Storage (10 TB)**: 長期バックアップ・監査ログに過不足なし
- **CPU**: 保守合算では余裕がない。恒常的な超過が観測された場合は、サービス割当の見直し
  （例: `opencode` / `code-server` の縮小）で合算を上限以下に調整する

### 5.3 コストリスク

- 無料枠を恒常的に超過する場合、計算出力の Cost Projection に示す従量課金相当のコストが参考値として生じ得る
- 実効負荷（加重平均ベース）が上限内であれば追加コストは $0.00
- 最大の実質リスクは Idle Compute 回収（ADR-0001 の対策: 自動再プロビジョニング）

---

## 6. 感度分析

シナリオ別の数値は §4 計算出力の Sensitivity Analysis を参照。解釈は以下の通り。

### 6.1 メモリ 10% 増加シナリオ

PostgreSQL / Qdrant のワークロード増大でメモリ需要が 10% 増えた場合、
総需要は上限を超過する（計算出力参照）。対策としてスワップファイル 2 GB の確保、または
`code-server` メモリ上限の引き下げを行う。

### 6.2 CPU 負荷 20% 増加シナリオ

ビルド・バッチ実行等で負荷が増えた場合でも、加重平均ベースでは余裕がある（計算出力参照）。
ただし保守合算ベースでは既に上限を超過しているため、`nice -n 10` / `CPUQuota` / 時間帯分散で
ピークを制御する。

---

## 7. 検証手順

### 7.1 単体実行確認

```bash
cd docs/architecture
rust-script calc.rs
```

期待出力: 上記「4. 計算結果」と同一の Markdown テーブル。

### 7.2 MPE プレビュー確認

1. VS Code / code-server で `report.md` を開く
2. MPE プレビューを開く (`Ctrl+Shift+V` またはコマンドパレット)
3. Code Chunk 実行 (`Shift+Enter` または `Ctrl+Shift+Enter`)
4. 表が正しくレンダリングされること、数式が MathJax/KaTeX で表示されることを確認

### 7.3 共有用成果物生成

MPE プレビューで以下を実行：

- **Save as Markdown** → `dist/report.md` に保存
- **Export HTML (offline)** → `dist/report.html` に保存

生成物を GitHub 等で開き、数式・表が崩れていないか確認。

---

## 8. 変更履歴とトレーサビリティ

| 日付 | 変更内容 | 検証者 |
| --- | --- | --- |
| 2026-10-09 | 初版作成 (ADR-0001 準拠) | Sovereign AI Agent |
| 2026-10-15 | 数値整合、Verdict マーカー、感度分析移管 | Sovereign AI Agent |

---

## 9. 参考資料

- [ADR-0001: Unified Architecture](../../adr/0001-unified-architecture.md)
- [OCI Always Free Resources](https://docs.oracle.com/en-us/iaas/Content/FreeTier/freetier_topic-Always_Free_Resources.htm)
- [OpenCode V2 Docs](https://opencode.ai/v2/docs)
- [MPE Code Chunk](https://github.com/shd101wyy/markdown-preview-enhanced/blob/master/docs/code-chunk.md)
