---
type: Test Specification
title: "jarvis-context PII フィルタ・検索順序 テスト仕様"
description: "再帰的 redact / enabled=false / 不正パターン / reorder_by_ids / スコア保持"
tags:
  - test-spec
  - jarvis-context
  - pii
  - search
generated:
  by: spec-writer
  at: "2026-11-18T01:49:06+09:00"
status: draft
---

適合形態は `tailored` のみ。ASVS / SP 800-53 / Common Criteria への
適合宣言は行わない。本ファイルは仕様のみで、テストコードは含まない。

## Test basis

| 項目 | 内容 |
| --- | --- |
| test item | `jarvis-context` の `PiiFilter`、`reorder_by_ids`、`attach_scores` |
| 根拠 | [todo.md](../todo.md) P1-9, P1-1, P0-11 |
| 判定基準 | 実装の観測可能挙動と既存 Rust テストの期待値 |
| 対象外 | PostgreSQL/Qdrant 実機、embedding モデル精度 |
| プロファイル | `web-api-l2-moderate` を参照した tailored |
| 基準出典 | [ASVS 5.0.0][asvs], [SP 800-53 r5.2][sp53], [CC Part 3][cc3] |

[asvs]: https://github.com/OWASP/ASVS/releases/tag/v5.0.0_release
[sp53]: https://csrc.nist.gov/pubs/sp/800/53/r5/upd1/final
[cc3]: https://commoncriteriaportal.org/cc/index.cfm

### 要求と test oracle

| ID | 要求 (合否の根拠) |
| --- | --- |
| REQ-1 | PII パターン一致文字列を設定した replacement で置換する |
| REQ-2 | 文字列は JSON の object/array を再帰的に走査して redact する |
| REQ-3 | 文字列以外の値 (数値・bool・null) は変更しない |
| REQ-4 | `enabled=false` のとき入力をそのまま返す (passthrough) |
| REQ-5 | 不正な正規表現は警告してスキップし、他のパターンは適用する |
| REQ-6 | 設定ファイル欠落/不正時は既定パターンと enabled=true に戻す |
| REQ-7 | `reorder_by_ids` は Qdrant の関連度 ID 順に並べ直す |
| REQ-8 | PG に無い ID はスキップし、ID に無い余分な行は破棄する |
| REQ-9 | 空の ID 列または空の取得行では空結果を返す |
| REQ-10 | `build_context` は組み立て後に PII フィルタを適用する |
| REQ-11 | 予算超過時は memories/recent を落とし event は保持する |
| REQ-12 | `attach_scores` が Qdrant のスコアを `MemoryEntry.score` へ付与し、順序変更後も id とスコアが対応する |
| REQ-13 | スコア 0 は `Some(0.0)`、ランク外は `None` とし、両者を区別する |

`MemoryEntry.score` は `Option<f32>` で serde default を持ち、
legacy JSON からスコアが欠落しても後方互換で読める。

## Risk & tier

| risk_id | リスク | tier | 完了基準 |
| --- | --- | --- | --- |
| R-1 | PII を含む文脈が LLM へ送信される | T1 | unit >= 0.90 |
| R-2 | 設定ミスでフィルタが素通しになる | T1 | unit >= 0.90 |
| R-3 | 不正パターンで redact 全体が停止する | T2 | unit >= 0.80 |
| R-4 | 設定ファイル不正で既定保護が効かない | T1 | unit >= 0.90 |
| R-5 | 検索順が失われ関連度ランキングが壊れる | T2 | unit >= 0.80 |
| R-6 | 欠損/余分行で重複や誤順序が発生する | T2 | unit >= 0.80 |
| R-7 | 予算超過で必要な event データまで失われる | T3 | unit >= 0.60 |
| R-8 | refetch でスコアが失われ関連度が壊れる / 0 と欠落が混同される | T2 | unit >= 0.80 |

tier は `web-api-l2-moderate` の定義に従う。

- T1: 個人情報・認可に関わる処理 (unit 0.90 / integration 0.80)
- T2: 主要業務ロジック (unit 0.80 / integration 0.60)
- T3: 補助・周辺機能 (unit 0.60 / integration 0.40)

完了基準は上記カバレッジに加え、全シナリオの oracle が満たされること。

## Design technique

29119-2:2021 8.2 の TD1 (test model) / TD2 (coverage item) /
TD3 (test case) の順で導出した。

| 技法 | 29119-4 節 | 適用箇所 |
| --- | --- | --- |
| equivalence partitioning | 5.2.1 | 値型、pattern 有効性、enabled の真偽 |
| boundary value analysis | 5.2.3 | 予算境界、ID 列 0/1/n |
| decision table testing | 5.2.6 | enabled x pattern x 値型 |
| metamorphic testing | 5.2.11 | PG 行順を変えても reorder 結果は不変 |
| scenario testing | 5.2.9 | build_context の組み立て〜truncate |
| error guessing | 5.4.1 | 不正 TOML、欠損行、余分な行 |

カバレッジ測定は 29119-4:2021 Clause 6 に従い、等価クラス・境界値・
分岐を記録する。ミューテーションスコアは本スライスでは未測定。

## Document map

| 29119-3:2021 情報項目 | 所在 |
| --- | --- |
| test design specification | Design technique, Scenarios |
| test case specification | Traceability, Scenarios |
| test procedure specification | Scenarios の Given/When/Then |
| test environment requirements | Test basis, evidence `environment` |
| test result / execution log | `evidence/test-run-*.json` |
| test completion report | evidence の `results` と完了報告 |

適合形態は `tailored` (29119-3:2021 4.1.3)。full conformance は主張しない。

## Scenarios

### PII フィルタ (REQ-1〜REQ-6)

```gherkin
Scenario: TC-P1 再帰的な redact
  Given email パターンが有効な PiiFilter
  When object と array が入れ子の JSON を filter する
  Then 全ての文字列の一致部分が [REDACTED] になる
  Then 数値などの非文字列は元のまま残る
```

```gherkin
Scenario: TC-P2 無効化時は passthrough
  Given enabled=false の PiiFilter
  When PII を含む JSON を filter する
  Then 出力は入力と完全に一致する
```

```gherkin
Scenario: TC-P3 不正パターンの隔離
  Given 不正な正規表現と有効な email パターンが混在する
  Given replacement は既定の [REDACTED] ではなく [X] が設定されている
  When from_config でフィルタを構築する
  Then 警告が出て起動は成功する
  When 文字列を filter する
  Then email は [X] に置換され、例外は出ない
```

```gherkin
Scenario: TC-P4 設定ファイルの fallback
  Given pii_patterns_path のファイルが欠落または不正 TOML
  When PiiFilter::new を呼ぶ
  Then 既定パターン (email/phone_jp/credit_card/api_key/ipv4/uuid) が有効
  Then enabled=true で redact が動作する
```

### 検索順序保持 (REQ-7〜REQ-9)

```gherkin
Scenario: TC-O1 関連度順の保持
  Given PG の row 順が C, A, B
  When Qdrant の ID 順 B, A, C で reorder_by_ids を呼ぶ
  Then 返却順は B, A, C になる
```

```gherkin
Scenario: TC-O2 欠損行と余分行
  Given 取得行が A と未知の ID
  When ID 列 B, A で reorder_by_ids を呼ぶ
  Then 返却は A のみになる
  Then 未知の ID と余分な行は含まれない
```

```gherkin
Scenario: TC-O3 空入力
  Given ID 列が空、または取得行が空
  When reorder_by_ids を呼ぶ
  Then 空の Vec を返す
```

### スコア保持 (REQ-12, REQ-13)

```gherkin
Scenario: TC-S1 並べ替え後のスコア追従
  Given PostgreSQL の取得行と Qdrant の (id, score) がある
  When reorder_by_ids の結果へ attach_scores を適用する
  Then 返却順は ID 順で、各 id のスコアは元の対応を保つ
```

```gherkin
Scenario: TC-S2 ゼロと欠落の区別
  Given スコア 0.0 の id とスコアを持たない id がある
  When attach_scores を適用する
  Then 前者の score は Some(0.0)、後者の score は None になる
```

### 文脈組み立て (REQ-10, REQ-11)

```gherkin
Scenario: TC-C 組み立て後のフィルタ適用
  Given event/memories/recent を含む文脈
  When build_context を呼ぶ
  Then 各フィールドの PII が redact された値が返る
```

```gherkin
Scenario: TC-C2 トークン予算の境界
  Given 直列化サイズが予算内の文脈
  When truncate_context_to_tokens を呼ぶ
  Then 文脈は変更されない
  Given 予算超過の文脈
  When 同じ関数を呼ぶ
  Then memories/recent は空配列になり event は残る
```

## Traceability

| Scenario | Req | ASVS | 800-53 | CC | Technique | Tier |
| --- | --- | --- | --- | --- | --- | --- |
| TC-P1 | REQ-1 | v5.0.0-V14.2.4 | SP800-53r5.2/SC-28 | ATE_FUN.1 | 5.2.1 | T1 |
| TC-P1 | REQ-2 | v5.0.0-V14.2.6 | SP800-53r5.2/SC-28 | ATE_FUN.1 | 5.2.9 | T1 |
| TC-P1 | REQ-3 | v5.0.0-V2.2.1 | SP800-53r5.2/SC-28 | ATE_FUN.1 | 5.2.1 | T1 |
| TC-P2 | REQ-4 | v5.0.0-V13.1.2 | SP800-53r5.2/CM-7 | ATE_FUN.1 | 5.2.6 | T1 |
| TC-P3 | REQ-5 | v5.0.0-V16.5.2 | SP800-53r5.2/CM-7 | ATE_FUN.1 | 5.4.1 | T2 |
| TC-P4 | REQ-6 | v5.0.0-V16.5.2 | SP800-53r5.2/CM-7 | ATE_FUN.1 | 5.4.1 | T1 |
| TC-O1 | REQ-7 | - | - | ATE_FUN.1 | 5.2.11 | T2 |
| TC-O2 | REQ-8 | - | - | ATE_COV.2 | 5.2.1 | T2 |
| TC-O3 | REQ-9 | - | - | ATE_COV.2 | 5.2.3 | T2 |
| TC-S1 | REQ-12 | - | - | ATE_FUN.1 | 5.2.11 | T2 |
| TC-S2 | REQ-13 | - | - | ATE_COV.2 | 5.2.3 | T2 |
| TC-C | REQ-10 | v5.0.0-V14.2.4 | SP800-53r5.2/SC-28 | ATE_FUN.1 | 5.2.9 | T1 |
| TC-C2 | REQ-11 | - | - | ATE_FUN.1 | 5.2.3 | T3 |

順序保持は機能的正確性の要件であり、ASVS / SP 800-53 に直接対応する
要件がない行は `-` とした (空欄ではない)。それ以外は ASVS を v5.0.0、
SP 800-53 を Release 5.2、CC を ISO/IEC 15408-3:2022 の構成要素 ID とする。

## 既存テストとの対応

既存テストは保持しつつ回帰テストを追加した。主な対応は
[pii.rs](../rust/jarvis-context/src/pii.rs),
[builder.rs](../rust/jarvis-context/src/builder.rs) を参照。
実行結果は `evidence/` の test-run JSON に記録する。

TC-P4 (設定 fallback)、TC-C (filter→truncate 順序)、PII 境界
(object key を保持し値のみ redact) は回帰テスト追加でカバー済み（17 テスト）。
TC-S1 / TC-S2 は `builder.rs` の純粋テスト（Qdrant/PostgreSQL 接続なし）で
カバー済み。TC-P3 の `[X]` はテスト用 replacement を Given で明示した。
実機 DB/Qdrant を用いる結合試験は対象外として未検証のままとする。
