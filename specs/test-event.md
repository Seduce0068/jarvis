---
type: Test Specification
title: "jarvis-event Webhook・Backpressure テスト仕様"
description: "webhook ペイロード変換と bounded channel の動的試験設計"
tags:
  - test-spec
  - jarvis-event
  - webhook
  - backpressure
generated:
  by: spec-writer
  at: "2026-11-18T02:17:43+09:00"
status: draft
---

適合形態は `tailored` のみ。ASVS / SP 800-53 / Common Criteria への
適合宣言は行わない。本ファイルは仕様のみで、テストコードは含まない。

## Test basis

| 項目 | 内容 |
| --- | --- |
| test item | `jarvis-event` の webhook 変換と `try_emit_event` |
| 根拠 | [todo.md](../todo.md) P1-9, P1-11, P2-1 |
| 判定基準 | 実装の観測可能挙動と既存 Rust テストの期待値 |
| 対象外 | webhook 認証 (P1-12 で設計判断待ち)、実 OpenCode 呼び出し |
| プロファイル | `web-api-l2-moderate` を参照した tailored |
| 基準出典 | [ASVS 5.0.0][asvs], [SP 800-53 r5.2][sp53], [CC Part 3][cc3] |

[asvs]: https://github.com/OWASP/ASVS/releases/tag/v5.0.0_release
[sp53]: https://csrc.nist.gov/pubs/sp/800/53/r5/upd1/final
[cc3]: https://commoncriteriaportal.org/cc/index.cfm

### 要求と test oracle

| ID | 要求 (合否の根拠) |
| --- | --- |
| REQ-1 | HA webhook は `event_type` を type とし、無い/文字列以外は unknown |
| REQ-2 | Android webhook は `type` を type とし、無い/文字列以外は unknown |
| REQ-3 | Gmail は固定 type `new_email` で payload の type 指定を無視する |
| REQ-4 | Calendar は固定 type `event_update` で type 指定を無視する |
| REQ-5 | 4 種の webhook は元の payload を変更せず保持する |
| REQ-6 | id/timestamp/correlation_id は呼び出し側が注入する |
| REQ-7 | 4 ルートが /webhook/ha, android, gmail, calendar に登録される |
| REQ-8 | イベント channel は bounded で容量は 1024 固定 |
| REQ-9 | 容量内では emit がイベントを enqueue する |
| REQ-10 | 満杯時は WARN を出し、イベントを落とし、ブロックしない |
| REQ-11 | event_sender の clone も同じ容量を共有する |
| REQ-12 | 受信側クローズ時は panic せず debug ログで破棄する |
| REQ-13 | policy が到達不能なら auto_respond を許可しない (fail-closed) |
| REQ-14 | /v1/events の source 未知値は Manual、correlation 未指定は UUID |
| REQ-15 | HA WebSocket の state_changed を解析し、非 event は無視する |
| REQ-16 | `/v1/events` はキュー満杯時に WARN を出し 503 を返す |
| REQ-17 | gRPC `EmitEvent` はキュー満杯時に WARN を出し RESOURCE_EXHAUSTED を返す |

## Risk & tier

| risk_id | リスク | tier | 完了基準 |
| --- | --- | --- | --- |
| R-1 | payload 誤解析でイベント種別が誤分類される | T2 | unit >= 0.80 |
| R-2 | 固定 type webhook に型指定を偽装される | T2 | unit >= 0.80 |
| R-3 | flood 状態でメモリが無制限に伸びる | T1 | unit >= 0.90 |
| R-4 | 満杯時にハンドラがブロックして可用性が落ちる | T1 | unit >= 0.90 |
| R-5 | ドロップ済みイベントが処理される | T1 | unit >= 0.90 |
| R-6 | 停止時の closed channel で panic する | T2 | unit >= 0.80 |
| R-7 | policy 到達不能時に自動応答が fail-open する | T1 | unit >= 0.90 |
| R-8 | source/correlation 欠落で追跡性が失われる | T3 | unit >= 0.60 |

tier は `web-api-l2-moderate` の定義に従う。

- T1: 可用性・fail-closed に関わる処理 (unit 0.90)
- T2: 主要変換ロジック (unit 0.80)
- T3: 補助・周辺機能 (unit 0.60)

完了基準は上記カバレッジに加え、全シナリオの oracle が満たされること。

## Design technique

29119-2:2021 8.2 の TD1 (test model) / TD2 (coverage item) /
TD3 (test case) の順で導出した。

| 技法 | 29119-4 節 | 適用箇所 |
| --- | --- | --- |
| equivalence partitioning | 5.2.1 | 文字列/欠落/非文字列の type 項目 |
| boundary value analysis | 5.2.3 | キュー 0/1/1024/1025 |
| decision table testing | 5.2.6 | source x type 項目 x 固定 type |
| state transition testing | 5.2.8 | open -> full -> closed |
| scenario testing | 5.2.9 | webhook -> emit -> policy gate |
| error guessing | 5.4.1 | 空 payload、closed receiver、未知 source |

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

### Webhook ペイロード変換 (REQ-1〜REQ-7)

```gherkin
Scenario: TC-W1 HA の event_type 変換
  Given payload に文字列の event_type がある
  When ha_webhook_event を呼ぶ
  Then source は HomeAssistant、type はその文字列
  Then payload は入力と同一
```

```gherkin
Scenario: TC-W2 欠落・非文字列の type
  Given event_type が無い、または数値の payload
  When HA と Android の変換を呼ぶ
  Then いずれも type は unknown になる
  Then payload は変更されない
```

```gherkin
Scenario: TC-W3 Android の type 変換
  Given payload に文字列の type がある
  When android_webhook_event を呼ぶ
  Then source は Android、type はその文字列
```

```gherkin
Scenario: TC-W4 固定 type の 2 経路
  Given payload に偽の type/event_type がある、または空 payload
  When Gmail と Calendar の変換を呼ぶ
  Then type は new_email / event_update に固定される
  Then payload の type 指定は無視される
```

### Bounded channel backpressure (REQ-8〜REQ-12)

```gherkin
Scenario: TC-B1 容量内の enqueue
  Given 容量 1024 の channel
  When emit を 1 回呼ぶ
  Then 1 件が enqueue され内容が保持される
```

```gherkin
Scenario: TC-B2 満杯時の shed load
  Given 1024 件が既に enqueue されている
  When 1025 件目を emit する
  Then WARN ログが出る
  Then 新しいイベントは破棄され、ハンドラはブロックしない
  Then 受け取れる件数は 1024 のまま
```

```gherkin
Scenario: TC-B3 clone sender と容量共有
  Given event_sender の clone がある
  When 本体で 1024 件を埋めて clone から 1 件送る
  Then clone のイベントも破棄される
```

```gherkin
Scenario: TC-B4 クローズ済み channel
  Given 受信側が drop 済みの channel
  When try_emit_event を呼ぶ
  Then panic せず debug ログのみで終了する
```

### 満杯時の API 拒否契約 (REQ-16〜REQ-17)

```gherkin
Scenario: TC-B5 HTTP の満杯時 503
  Given /v1/events の channel が満杯である
  When emit リクエストを送る
  Then WARN ログが出る
  Then SERVICE_UNAVAILABLE (503) を返す
  Then ハンドラはブロックせず後続リクエストを処理できる
```

```gherkin
Scenario: TC-B6 gRPC の満杯時 RESOURCE_EXHAUSTED
  Given EmitEvent の channel が満杯である
  When EmitEvent RPC を送る
  Then WARN ログが出る
  Then RESOURCE_EXHAUSTED を返す
  Then ハンドラはブロックせず後続 RPC を処理できる
```

### 異常系・入口 (REQ-13〜REQ-15)

```gherkin
Scenario: TC-E1（policy 到達不可 fail-closed）と TC-E2
（/v1/events の source 正規化・correlation 生成）は 2026-11-18 に
回帰テストを追加しカバー済み（24 テスト）。
実機 policy サービスを用いた結合試験は対象外。

## Traceability

| Scenario | Req | ASVS | 800-53 | CC | Technique | Tier |
| --- | --- | --- | --- | --- | --- | --- |
| TC-W1 | REQ-1 | v5.0.0-V2.2.1 | SP800-53r5.2/AU-3 | ATE_FUN.1 | 5.2.1 | T2 |
| TC-W2 | REQ-1 | v5.0.0-V2.2.1 | SP800-53r5.2/AU-3 | ATE_COV.2 | 5.2.1 | T2 |
| TC-W2 | REQ-2 | v5.0.0-V2.2.1 | SP800-53r5.2/AU-3 | ATE_FUN.1 | 5.2.6 | T2 |
| TC-W3 | REQ-2 | v5.0.0-V2.2.2 | SP800-53r5.2/AU-3 | ATE_FUN.1 | 5.2.1 | T2 |
| TC-W4 | REQ-3 | v5.0.0-V2.2.2 | SP800-53r5.2/AU-3 | ATE_FUN.1 | 5.2.6 | T2 |
| TC-W4 | REQ-4 | v5.0.0-V2.2.2 | SP800-53r5.2/AU-3 | ATE_FUN.1 | 5.2.6 | T2 |
| TC-W1 | REQ-5 | - | SP800-53r5.2/AU-3 | ATE_FUN.1 | 5.2.9 | T2 |
| TC-W1 | REQ-6 | - | SP800-53r5.2/AU-3 | ATE_FUN.1 | 5.2.9 | T2 |
| TC-W1 | REQ-7 | - | SP800-53r5.2/AC-3 | ATE_FUN.1 | 5.2.9 | T2 |
| TC-B1 | REQ-8 | v5.0.0-V15.2.2 | SP800-53r5.2/SC-5 | ATE_COV.2 | 5.2.3 | T1 |
| TC-B1 | REQ-9 | - | SP800-53r5.2/SC-5 | ATE_FUN.1 | 5.2.3 | T1 |
| TC-B2 | REQ-10 | v5.0.0-V2.4.1 | SP800-53r5.2/SC-5 | ATE_COV.2 | 5.2.3 | T1 |
| TC-B3 | REQ-11 | - | SP800-53r5.2/SC-5 | ATE_FUN.1 | 5.2.8 | T1 |
| TC-B4 | REQ-12 | v5.0.0-V16.5.3 | SP800-53r5.2/SI-4 | ATE_FUN.1 | 5.4.1 | T2 |
| TC-B5 | REQ-16 | v5.0.0-V2.4.1 | SP800-53r5.2/SC-5 | ATE_FUN.1 | 5.2.3 | T1 |
| TC-B6 | REQ-17 | v5.0.0-V2.4.1 | SP800-53r5.2/SC-5 | ATE_FUN.1 | 5.2.3 | T1 |
| TC-E1 | REQ-13 | v5.0.0-V8.3.1 | SP800-53r5.2/AC-3 | ATE_FUN.1 | 5.4.1 | T1 |
| TC-E2 | REQ-14 | v5.0.0-V16.2.4 | SP800-53r5.2/AU-3 | ATE_FUN.1 | 5.2.1 | T3 |
| TC-E3 | REQ-15 | - | SP800-53r5.2/AU-3 | ATE_FUN.1 | 5.2.9 | T2 |

ASVS / SP 800-53 に直接対応しない行は `-` とした (空欄ではない)。
REQ-1〜REQ-15 と TC-W1〜W4 / TC-B1〜B4 / TC-E1〜E3 を網羅する。
REQ-16 / REQ-17 と TC-B5 / TC-B6 は満杯時の API 拒否契約を網羅する。
