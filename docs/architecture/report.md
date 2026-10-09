# JARVIS Architecture Resource Analysis

*Document source: `report.md` | Calculations: `calc.rs` | Generated: `dist/`*

---

## 1. 概要

本ドキュメントは、JARVIS Personal AI システムを **OCI Always Free (Ampere A1 Flex: 2 OCPU, 12 GB RAM)** 上で運用する際のリソース要件を、Rust で計算・検証した結果を示す。

- **文書原本**: この `report.md` (Markdown + LaTeX数式記法)
- **計算ロジック**: `calc.rs` (Rust、単体実行可能)
- **表示・実行**: Markdown Preview Enhanced (MPE) Code Chunk
- **共有成果物**: `dist/report.md` (計算済み), `dist/report.html`

> **注意**: MPE の `@import` 構文と Code Chunk 実行結果は、MPE プレビューでのみ反映される。GitHub 等の一般的な Markdown ビューアでは計算結果は表示されないため、共有時は `dist/` 以下の生成物を使用すること。

---

## 2. アーキテクチャ構成

JARVIS は以下のサービスを単一 VM 上で systemd 管理する：

| サービス | 役割 | 技術 |
|----------|------|------|
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
C = \max(0, OCPU_{used} - 2.0) \times 720 \times \$0.0085 + \max(0, Mem_{used} - 12.0) \times 720 \times \$0.0012
$$

---

## 4. 計算結果

以下の表は `calc.rs` を MPE Code Chunk として実行した結果である。

@import "calc.rs" {cmd="rust-script" output="markdown" hide=true}

---

## 5. 解釈と判断

### 5.1 リソース適合性

計算結果の通り、**全サービスの総需要は Always Free 枠内に収まる** (OCPU 87.5%, Memory 95.8%, Disk 12.5%)。

特にメモリ余裕が **0.5 GB (4.2%)** と僅少であるため、以下の運用制約を守る：

1. **同時ビルド・重いコンパイルは避ける** (Rust リリースビルドは一時的に 2-3 GB 追加消費)
2. **PostgreSQL shared_buffers は 512 MB 以下に制限**
3. **Qdrant キャッシュサイズは 1 GB 以下に制限**
4. **code-server 拡張は最小限に留める**

### 5.2 スケーリング余地

- **OCPU 余裕: 0.25 OCPU** — 軽量バッチ処理や将来的なサービス追加に対応可能
- **Disk 余裕: 175 GB** — ログ・バックアップ・DB 成長に十分
- **Object Storage: 10 TB** — 長期バックアップ・監査ログに過不足なし

### 5.3 コストリスク

無料枠内に収まるため **月額コストは $0.00**。Idle Compute 回収リスクのみが実質的リスク。

---

## 6. 感度分析

### 6.1 メモリ 10% 増加シナリオ

PostgreSQL または Qdrant のワークロード増大でメモリ需要が 10% 増えた場合：

- 新総メモリ: $11.45 \times 1.1 = 12.6$ GB → **上限超過 (105%)**
- 対策: スワップファイル 2 GB 確保、または `code-server` メモリ制限を 1 GB に削減

### 6.2 OCPU 20% 増加シナリオ

同時ビルド等で CPU 負荷が増えた場合：

- 加重 CPU 需要: $0.99 \times 1.2 = 1.19$ OCPU-equivalent
- 実 OCPU 使用率: 87.5% → 実測値で 100% 接近、レスポンス低下リスク
- 対策: `cargo build` 等は `nice -n 10` または専用時間帯に実行

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
|------|----------|--------|
| 2026-10-09 | 初版作成 (ADR-0001 準拠) | Sovereign AI Agent |

---

## 9. 参考資料

- [ADR-0001: Unified Architecture](../adr/0001-unified-architecture.md)
- [OCI Always Free Resources](https://docs.oracle.com/en-us/iaas/Content/FreeTier/freetier_topic-Always_Free_Resources.htm)
- [OpenCode V2 Docs](https://opencode.ai/v2/docs)
- [MPE Code Chunk](https://github.com/shd101wyy/markdown-preview-enhanced/blob/master/docs/code-chunk.md)