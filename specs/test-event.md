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
| REQ-18 | `/health` は HEALTH_OK_BODY(200) または HEALTH_OPENCODE_UNAVAILABLE_BODY(503) のみを返し、未知本文は fail-closed |

REQ-18 の固定本文は共通の `HEALTH_OK_BODY` と本サービス固有の
`HEALTH_OPENCODE_UNAVAILABLE_BODY` の 2 種類に閉じる。監視側はこの 2 本文への
完全一致以外を不完全 probe として fail-closed に倒す。

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
| R-9 | /health 本文が改変・未知形状になり下流停止が見逃される | T1 | unit >= 0.90 |

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
| contract testing | 5.2.12 | /health の固定本文と OpenCode 連動 degraded 契約 |

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
  # I61-A: BareJson の JSON 入力受け取り契約（webhook / v1/events 共通）
  When Content-Type を外した JSON 本文で POST する
  Then 空本文の 415 を返す（ヘッダ付与を促す）
  When axum 既定上限 2 MiB を超える本文で POST する
  Then 空本文の 413 を返す（本文縮小を促す）
  When 型不正・構文エラー、または本文読み取り失敗（UnknownBodyError、I64-C）の本文で POST する
  Then axum の 422 を採らずサイズ超過扱いもせず空本文の 400 を返す
  Then 4 種とも rejection 文と入力本文バイトを応答・ログへ出さない
  Then ログは DEBUG の error_kind=invalid_input / field=body 固定語彙のみ
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

TC-E1（policy 到達不可 fail-closed）と TC-E2
（/v1/events の source 正規化・correlation 生成）は 2026-11-18 に
回帰テストを追加しカバー済み（24 テスト）。
実機 policy サービスを用いた結合試験は対象外。
なお TC-E1 / TC-E2 は回帰テストの説明であり、個別の gherkin Scenario
としては宣言しない（Traceability の行は既存テストとの対応を示す）。
TC-E3（HA WebSocket の state_changed 解析と非 event 無視）も同じく回帰
テストでカバー済みである。`rust/jarvis-event/src/sources.rs` の
`parses_state_changed_event` と `ignores_non_event_messages` が根拠テスト
であり、Traceability の TC-E3 行はこれらの対応を示す。
実機 HA サービスを用いた結合試験は対象外。

### /health 契約 (REQ-18)

```gherkin
  # I103-D: Round 93 契約マーカー
Scenario: TC-HC1 /health healthy の固定本文
  Given OpenCode への到達性 probe が成功する
  When GET /health を呼ぶ
  Then status は 200 で本文は HEALTH_OK_BODY と完全バイト一致する
  Then Content-Type は application/json で reason キーは無い
```

```gherkin
  # I103-D: Round 93 契約マーカー
Scenario: TC-HC2 /health degraded の固定本文
  Given OpenCode が到達不可または probe がタイムアウトする
  When GET /health を呼ぶ
  Then status は 503 で本文は HEALTH_OPENCODE_UNAVAILABLE_BODY と完全バイト一致する
  Then reason は opencode_unavailable 固定で入力や接続情報を本文に出さない
```

```gherkin
  # I103-D: Round 93 契約マーカー
Scenario: TC-HC3 未知本文の fail-closed 拒否
  Given 中継が本文を書き換える（余分/重複キー、未知 reason、前後空白）または status を改変する
  When 監視側がその probe を既知 2 本文へ照合する
  Then 完全一致しない本文は OK と分類せず fail-closed（停止側）へ倒す
  Then event の /health は既知 2 本文以外を生成せず、未知本文を返さない
```

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
| TC-HC1 | REQ-18 | v5.0.0-V2.4.1 | SP800-53r5.2/SC-5 | ATE_FUN.1 | 5.2.12 | T1 |
| TC-HC2 | REQ-18 | v5.0.0-V16.5.3 | SP800-53r5.2/SI-4 | ATE_FUN.1 | 5.2.12 | T1 |
| TC-HC3 | REQ-18 | v5.0.0-V16.5.1 | SP800-53r5.2/SI-4 | ATE_COV.2 | 5.4.1 | T1 |

ASVS / SP 800-53 に直接対応しない行は `-` とした (空欄ではない)。
REQ-1〜REQ-15 と TC-W1〜W4 / TC-B1〜B4 / TC-E1〜E3 を網羅する。
REQ-16 / REQ-17 と TC-B5 / TC-B6 は満杯時の API 拒否契約を網羅する。
REQ-18 と TC-HC1〜TC-HC3 は `/health` の固定本文契約を網羅する。

## 既存テストとの対応

`/health` 契約は [http.rs](../rust/jarvis-event/src/http.rs) の
`health_response` / `health` / `opencode_reachable` が実装する。根拠となる
テスト関数は `health_uses_core_ok_body_and_fixed_503_degraded_body`
（200 は `HEALTH_OK_BODY`、503 は `HEALTH_OPENCODE_UNAVAILABLE_BODY` の
バイト一致と `application/json` を pin）、
`opencode_probe_flags_a_released_loopback_port`、
`opencode_probe_accepts_a_local_listener_stub`、
`health_probe_url_comes_from_config_not_direct_env` である。

固定 BODY 定数は [jarvis-core/src/lib.rs](../rust/jarvis-core/src/lib.rs) の
`HEALTH_OK_BODY` が共通で、degraded 側は本サービスの
`HEALTH_OPENCODE_UNAVAILABLE_BODY` が `HEALTH_STATUS_DEGRADED` 語彙と
reason `opencode_unavailable` を保持する。core 側の pin は
`health_wire_bodies_pin_the_wire_contract` と
`health_status_vocabulary_pins_the_wire_contract` が担う。

未知本文の fail-closed は共有の分類機構で担保する。
[policy_client.rs](../ui/src/policy_client.rs) の
`health_probe_unexpected_200_body_fails_closed` /
`health_probe_unexpected_503_body_fails_closed` /
`health_probe_duplicate_key_body_fails_closed` /
`health_probe_extra_key_body_fails_closed` /
`health_probe_unknown_shape_fails_closed` /
`health_probe_padded_body_fails_closed` が、既知本文への完全一致以外を
停止側へ倒すことを検証する。policy 専用の health 契約
（`specs/test-ui.md:80-84`）は変更しない。

本タスクで `I103-D` 契約マーカー（TC-HC1〜HC3）を追加し、上記の未閉鎖
gherkin ブロックを本文へ戻したため `Scenario:` 宣言は 14 → 13 行になった。
`spec_hash` の変更に伴う証跡再生成と、`notes` への
`HEALTH_OK_BODY` / `HEALTH_OPENCODE_UNAVAILABLE_BODY` / `fail-closed` の
明記は親が行う。本エージェントは証跡を変更しない。
