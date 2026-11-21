---
type: Test Specification
title: "承認コンソール UI テスト仕様"
description: "エラー保持 / 期限切れ強調 / scope 別トークン / a11y / kill switch pin / 409 winner / probe fail-closed"
tags:
  - test-spec
  - gpuui
  - approval
  - security
generated:
  by: spec-writer
  at: "2026-11-20T05:18:30+09:00"
status: draft
---

適合形態は `tailored` のみ。ASVS / SP 800-53 / Common Criteria への
適合宣言は行わない。本ファイルは仕様のみで、テストコードは含まない。

## Test basis

| 項目 | 内容 |
| --- | --- |
| test item | [approvals.rs](../ui/src/approvals.rs) と [policy_client.rs](../ui/src/policy_client.rs) |
| 根拠 | [todo.md](../todo.md) P1-9, P0-8, P3-6, P3-8 |
| 判定基準 | 実装の観測可能挙動と既存 Rust テストの期待値 |
| 対象外 | GPUI 実ウィンドウの目視確認、実 policy サービスとの結合 |
| プロファイル | `web-api-l2-moderate` を参照した tailored |
| 基準出典 | [ASVS 5.0.0][asvs], [SP 800-53 r5.2][sp53], [CC Part 3][cc3] |

[asvs]: https://github.com/OWASP/ASVS/releases/tag/v5.0.0_release
[sp53]: https://csrc.nist.gov/pubs/sp/800/53/r5/upd1/final
[cc3]: https://commoncriteriaportal.org/cc/index.cfm

### 要求と test oracle

| ID | 要求 (合否の根拠) |
| --- | --- |
| REQ-1 | 保留中キューは 3 秒間隔で background executor から取得する |
| REQ-2 | ポール失敗時は既知のリストを残し接続エラーを表示する |
| REQ-3 | 決定結果は refresh の後に適用され、直次の poll で上書きされない |
| REQ-4 | 決定エラーは 3 秒後の refresh でも消えず理由を表示する |
| REQ-5 | action_status は 1 refresh 周期で解除され件数表示へ戻る |
| REQ-6 | 期限切れ行は背景色と「期限切れ」badge で強調される |
| REQ-7 | 期限切れ強調は表示のみで、決定処理は変更しない |
| REQ-8 | 承認引数は切り詰めず完全な pretty JSON を表示する |
| REQ-9 | 承認/却下は focusable・tab_stop・Role::Button・aria_label を持つ |
| REQ-10 | Tab / Shift-Tab でフォーカス移動し、Enter/Space は1回だけ決定する |
| REQ-11 | pending 一覧 GET に API scope、approve/deny POST に admin scope のトークンを X-Jarvis-Approval-Token として付ける |
| REQ-12 | リダイレクトは追従せず、トークンは元ホスト外へ漏れない |
| REQ-13 | 応答の無い policy はタイムアウトし UI を固めない |
| REQ-14 | pending の JSON は追加フィールドを許容し、空配列も受け付ける |
| REQ-15 | base URL の末尾スラッシュは正規化される |
| REQ-16 | ウィンドウ閉鎖時は poller が停止する |
| REQ-17 | pending の非2xx (401/500/3xx) はエラーとし空キューと誤らない |
| REQ-18 | decide の非2xx (401/500/3xx) はエラーとし成功と誤らない |
| REQ-19 | トークン未設定の scope ではヘッダを送らない。3xx では再送しない |
| REQ-20 | admin scope 未設定時は API scope へフォールバックする。レガシー ENV は警告付きで API scope にフォールバックし、空文字は未設定として扱う |
| REQ-21 | kill switch 発動中は承認済みの「実行停止」表示を独立 pin で保持し、poll で件数に上書きしない |
| REQ-22 | kill switch が解除されたときのみ停止 pin を外し通常表示へ戻す |
| REQ-23 | already-decided 409 は status 端点の winner を読み AlreadyDecided として表示する |
| REQ-24 | winner が読めない 409 はエラーとし、winner の理由本文は表示しない |
| REQ-25 | kill switch 409 は共有本文一致または空/読取不能本文で停止を pin し、winner 取得をしない |
| REQ-26 | probe は fail-closed。timeout・接続失敗・読み取り不能本文は active=true とする |
| REQ-27 | 固定本文との完全バイト一致（trim なし）の healthy 応答のみ active=false とし、latch と pin を解除する |
| REQ-28 | 409 の停止 latch は racing した healthy probe より優先する |
| REQ-29 | 410 本文が `APPROVAL_EXPIRED_BODY` の trim 後完全一致なら Expired を返し winner 取得しない |
| REQ-30 | 410 本文が未知/改変/空/読取不能/409 本文なら fail-closed とし Expired を捏造しない |

トークン関連の REQ-11 / REQ-19 / REQ-20 は
[policy_client.rs](../ui/src/policy_client.rs) の scope 定数に対応する。
API scope は `JARVIS_POLICY_APPROVAL_API_TOKEN`、レガシー
`JARVIS_POLICY_APPROVAL_TOKEN` は警告付きフォールバック、admin scope は
`JARVIS_POLICY_APPROVAL_ADMIN_TOKEN` を読み、未設定時は API scope へ
フォールバックする。両 scope ともヘッダ名は `X-Jarvis-Approval-Token`。

409 の分類は fail-closed である。`jarvis-core` が公開する共有固定本文
`ALREADY_DECIDED_409_BODY` の trim 後完全一致の場合のみ winner 取得へ進み、
`KILL_SWITCH_409_BODY`・空・読み取り不能・未知／改変本文はすべて非常停止側へ
分類する。winner の取得は status 端点で API scope を優先し、未設定なら
admin scope を使う。

`/health` の分類も fail-closed である。`jarvis-core` が公開する
`HEALTH_OK_BODY` / `HEALTH_KILL_SWITCH_BODY` / `HEALTH_NO_POLICY_BODY` を
**トリムなしの完全バイト一致**で照合する（I66-C）。前後空白つきの本文、
余分／重複キー、未知の形状、読み取り不能な本文、`200` / `503` 以外の status
はすべて不完全 probe として `Err` → `true`（停止側）へ倒れる。

410 の分類も fail-closed である。`jarvis-core` が公開する共有固定本文
`APPROVAL_EXPIRED_BODY` を **trim 後完全一致**で確認した場合のみ、決定 POST を
`Decision::Expired`（終局・winner なし）として表示し、status 端点の winner
取得へ進まない。改変・未知・空・読取不能な本文や 409 固定本文は不一致として
エラーとし、`Expired` を捏造しない。共有本文は core / policy / UI / mcp-gw の
pin で片側変更を検出する。

## Risk & tier

| risk_id | リスク | tier | 完了基準 |
| --- | --- | --- | --- |
| R-1 | 決定エラーが refresh で消え利用者が誤解する | T1 | unit >= 0.90 |
| R-2 | 期限切れ要求を誤って承認する / 410 改変本文で Expired を誤表示する | T1 | unit >= 0.90 |
| R-3 | 承認トークンがリダイレクト先へ漏洩する | T1 | unit >= 0.90 |
| R-4 | キーボードだけでは承認操作に到達できない | T2 | unit >= 0.80 |
| R-5 | 引数省略で危険なコマンドが隠れる | T1 | unit >= 0.90 |
| R-6 | policy 無応答で UI スレッドが停止する | T2 | unit >= 0.80 |
| R-7 | poller が停止せずリソースをリークする | T3 | unit >= 0.60 |
| R-8 | JSON 形状違いでキューが読めない | T3 | unit >= 0.60 |
| R-9 | 非2xx が空キュー/成功と誤認される | T1 | unit >= 0.90 |
| R-10 | scope 誤りでトークンが別 API へ送られる / 未設定 scope で誤送信される | T1 | unit >= 0.90 |
| R-11 | 発動中の kill switch で承認済み表示が件数に上書きされ誤認される | T1 | unit >= 0.90 |
| R-12 | 409 winner が読めず失敗/誤表示、winner 理由が漏れる | T1 | unit >= 0.90 |
| R-13 | kill switch 409 が winner と誤認され停止表示が消える | T1 | unit >= 0.90 |
| R-14 | probe の transient 失敗で停止 pin が外れ fail-open になる | T1 | unit >= 0.90 |

tier は `web-api-l2-moderate` の定義に従う。

- T1: 承認判断・秘密情報に関わる処理 (unit 0.90)
- T2: 主要 UI/通信ロジック (unit 0.80)
- T3: 補助・周辺機能 (unit 0.60)

完了基準は上記カバレッジに加え、全シナリオの oracle が満たされること。

## Design technique

29119-2:2021 8.2 の TD1 (test model) / TD2 (coverage item) /
TD3 (test case) の順で導出した。

| 技法 | 29119-4 節 | 適用箇所 |
| --- | --- | --- |
| equivalence partitioning | 5.2.1 | 成功/失敗、期限前/後、空/非空キュー、410 本文一致/不一致 |
| boundary value analysis | 5.2.3 | 3 秒 refresh、timeout、長い引数 |
| decision table testing | 5.2.6 | 決定結果 x refresh x action_status、scope x トークン設定 |
| state transition testing | 5.2.8 | 読込 -> 決定 -> 次 poll |
| scenario testing | 5.2.9 | 演算子がキューを確認し決定する |
| error guessing | 5.4.1 | 302 応答、hang、追加 JSON 項目 |

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

### エラー保持と期限切れ強調 (REQ-1〜REQ-8)

```gherkin
Scenario: TC-U1 決定エラーの保持
  Given 承認要求が保留中
  When 承認が 409 で失敗し、3 秒後の poll が成功する
  Then 失敗理由は消えずに表示される
  Then キューは最新の内容に更新される
```

```gherkin
Scenario: TC-U2 成功後の状態遷移
  Given 決定が成功した直後
  When 次の poll が件数を返す
  Then 決定メッセージは 1 周期だけ保持される
  Then その後は「保留中: N 件」に戻る
```

```gherkin
Scenario: TC-U3 poll 失敗
  Given policy への接続が失敗する
  When apply_poll がエラーを受け取る
  Then 直前のキューは残る
  Then 接続エラーと原因が表示される
```

```gherkin
Scenario: TC-U4 期限切れの強調
  Given expires_at が過去の要求と未来の要求
  When 行を描画する
  Then 期限切れ行のみ強調色と「期限切れ」が表示される
  Then 決定ハンドラの挙動は同じである
```

```gherkin
Scenario: TC-U5 引数の完全表示
  Given 500 文字を超える context
  When format_args で表示文字列を作る
  Then 全文字が含まれる
  Then 先頭は "args: {" である
```

### キーボード/a11y (REQ-9, REQ-10)

```gherkin
Scenario: TC-U6 フォーカスと操作
  Given 承認コンソールが表示されている
  When Tab / Shift-Tab を押す
  Then 承認・却下ボタン間を移動できる
  Then 各ボタンは Button role と aria_label を持つ
  When フォーカスボタンで Enter または Space を押す
  Then 決定は一度だけ送信される
```

### policy クライアント (REQ-11〜REQ-16, REQ-19, REQ-20)

```gherkin
Scenario: TC-U7 scope 別トークンヘッダ
  Given API scope と admin scope の両方にトークンが設定されている
  When pending / approve / deny を呼ぶ
  Then pending には API scope のトークンが付く
  Then approve / deny には admin scope のトークンが付く
  Then ヘッダ名はいずれも X-Jarvis-Approval-Token で、3xx では再送しない
```

```gherkin
Scenario: TC-U8 リダイレクト不追従
  Given policy が 302 と別ポートの Location を返す
  When token 付きで pending を呼ぶ
  Then リダイレクト先へ要求が送られない
  Then トークンは漏洩しない
  Then 3xx はステータス付きエラーとして返る
```

```gherkin
Scenario: TC-U9 タイムアウト
  Given policy が応答しない
  When pending を呼ぶ
  Then 設定した timeout でエラーを返す
  Then UI スレッドはブロックしない
```

```gherkin
Scenario: TC-U10 JSON 形状と URL
  Given 追加フィールド付きの pending JSON
  When レスポンスを解析する
  Then 追加フィールドは無視される
  Given 空配列の pending と末尾スラッシュ付き base URL
  When 同じく解析・正化する
  Then 空リストを受け付け URL は正規化される
```

```gherkin
Scenario: TC-U11 pending の非2xx
  Given 401 / 500 / 3xx のレスポンス
  When pending を呼ぶ
  Then HTTP ステータスを含むエラーを返す
  Then 空のキューとして扱わない
```

```gherkin
Scenario: TC-U12 decide の非2xx
  Given approve / deny が 401 / 500 / 3xx を返す
  When それぞれを呼ぶ
  Then HTTP ステータスを含むエラーを返す
  Then 成功として表示しない
```

### policy トークンの scope 解決 (REQ-19, REQ-20)

```gherkin
Scenario: TC-U13 scope 別フォールバックと未設定時
  Given admin scope のトークンが未設定
  When approve / deny を呼ぶ
  Then API scope のトークンが X-Jarvis-Approval-Token として付く
  Given API scope のトークンも空文字で未設定
  When pending と approve を呼ぶ
  Then いずれのリクエストにも X-Jarvis-Approval-Token を送らない
```

```gherkin
Scenario: TC-U14 レガシー ENV の警告付きフォールバック
  Given レガシー ENV のみで API scope トークンが設定されている
  When 起動時にトークンを解決する
  Then API scope に設定され警告が出る
  Given 正規 ENV とレガシー ENV の両方が設定されている
  When 起動時にトークンを解決する
  Then 正規 ENV が優先され警告は出ない
```

### kill switch pin / 409 / probe (REQ-21〜REQ-28)

```gherkin
Scenario: TC-U15 kill switch 中の停止表示 pin
  Given 承認が成功し kill switch が発動している
  When 後続の poll が件数を返す
  Then 「承認済み・実行停止」が保持され件数表示に上書きされない
  When kill switch が解除される
  Then pin が外れ通常の件数表示へ戻る
```

```gherkin
Scenario: TC-U16 already-decided 409 の winner 解決
  Given approve が already-decided の 409 を返す
  When status 端点から winner を読む
  Then AlreadyDecided として winner の決定を表示する
  Then エラー表示も winner の理由本文も出さない
  Given winner が読めない
  When 同じ 409 を処理する
  Then エラーとして fail-closed にする
```

```gherkin
Scenario: TC-U17 kill switch 409 の共有本文分類
  Given policy が固定本文 KILL_SWITCH_409_BODY の 409 を返す
  When decide_outcome で分類する
  Then KillSwitchBlocked として停止表示を pin する
  Then winner 取得は行わず理由本文も表示しない
  Given 409 本文が空または読み取り不能
  When 同じく分類する
  Then winner 取得へ進まず停止側へ fail-closed に分類する
```

```gherkin
Scenario: TC-U18 probe の fail-closed と 409 優先
  Given /health probe が timeout または読み取り不能な本文を返す
  When kill_switch_active を評価する
  Then true を返し停止 pin を維持する
  Given probe が先に healthy を返した後に kill switch 409 本文が届く
  When 停止 latch を適用する
  Then 409 本文が probe に優先し停止表示が残る
  Given /health が前後空白つきの healthy 本文を返す
  When kill_switch_active を評価する
  Then Err として fail-closed にし true を返し停止 pin を維持する
  Given /health が固定本文 HEALTH_OK_BODY を完全バイト一致で返す
  When kill_switch_active を評価する
  Then false を返し latch と pin が解除される
```

### 期限切れ 410 の本文確認 (REQ-29, REQ-30)

```gherkin
Scenario: TC-U19 410 の共有本文確認と fail-closed
  Given approve / deny が 410 Gone を返す
  And 本文は共有固定本文 APPROVAL_EXPIRED_BODY と trim 後完全一致する
  When decide_outcome で分類する
  Then Decision::Expired を返す
  Then status 端点の winner 取得へ進まずエラー表示も出さない
  Given 410 本文が改変・未知・空・読取不能または 409 固定本文である
  When 同じく分類する
  Then エラーとして fail-closed にし Expired を捏造しない
  Then status 端点へのアクセスを行わない
```

```gherkin
Scenario: TC-U20 ウィンドウ閉鎖時の poller 停止
  Given approvals view の poller が起動している
  When ウィンドウ閉鎖イベントを受信する
  Then poller は停止し後続のpollを行わない
  Then リソースリークを起こさない
```

## Traceability

| Scenario | Req | ASVS | 800-53 | CC | Technique | Tier |
| --- | --- | --- | --- | --- | --- | --- |
| TC-U2 | REQ-1 | v5.0.0-V16.5.2 | SP800-53r5.2/AU-3 | ATE_FUN.1 | 5.2.6 | T1 |
| TC-U1 | REQ-3 | v5.0.0-V16.5.2 | SP800-53r5.2/AU-3 | ATE_FUN.1 | 5.2.6 | T1 |
| TC-U1 | REQ-4 | v5.0.0-V16.5.2 | SP800-53r5.2/AU-3 | ATE_FUN.1 | 5.2.8 | T1 |
| TC-U2 | REQ-5 | - | SP800-53r5.2/AU-3 | ATE_FUN.1 | 5.2.8 | T2 |
| TC-U3 | REQ-2 | v5.0.0-V16.5.2 | SP800-53r5.2/SI-4 | ATE_FUN.1 | 5.4.1 | T1 |
| TC-U4 | REQ-6 | - | SP800-53r5.2/AC-3 | ATE_FUN.1 | 5.2.1 | T1 |
| TC-U4 | REQ-7 | - | SP800-53r5.2/AC-3 | ATE_FUN.1 | 5.2.1 | T1 |
| TC-U5 | REQ-8 | v5.0.0-V3.2.2 | SP800-53r5.2/AC-3 | ATE_COV.2 | 5.2.3 | T1 |
| TC-U6 | REQ-9 | - | SP800-53r5.2/AC-3 | ATE_FUN.1 | 5.2.1 | T2 |
| TC-U6 | REQ-10 | - | SP800-53r5.2/AC-3 | ATE_FUN.1 | 5.2.8 | T2 |
| TC-U7 | REQ-11 | v5.0.0-V14.2.1 | SP800-53r5.2/SC-8 | ATE_FUN.1 | 5.2.1 | T1 |
| TC-U8 | REQ-11 | v5.0.0-V15.3.2 | SP800-53r5.2/SC-8 | ATE_FUN.1 | 5.4.1 | T1 |
| TC-U8 | REQ-12 | v5.0.0-V15.3.2 | SP800-53r5.2/SC-8 | ATE_FUN.1 | 5.4.1 | T1 |
| TC-U8 | REQ-17 | v5.0.0-V16.5.2 | SP800-53r5.2/SC-8 | ATE_FUN.1 | 5.4.1 | T1 |
| TC-U9 | REQ-13 | v5.0.0-V13.1.3 | SP800-53r5.2/SC-5 | ATE_FUN.1 | 5.4.1 | T2 |
| TC-U10 | REQ-14 | v5.0.0-V2.2.1 | SP800-53r5.2/AU-3 | ATE_FUN.1 | 5.2.1 | T3 |
| TC-U10 | REQ-15 | - | SP800-53r5.2/CM-7 | ATE_FUN.1 | 5.2.3 | T3 |
| TC-U20 | REQ-16 | - | SP800-53r5.2/CM-7 | ATE_FUN.1 | 5.2.8 | T3 |
| TC-U11 | REQ-17 | - | SP800-53r5.2/SC-8 | ATE_FUN.1 | 5.4.1 | T1 |
| TC-U12 | REQ-18 | - | SP800-53r5.2/AC-3 | ATE_FUN.1 | 5.4.1 | T1 |
| TC-U13 | REQ-19 | v5.0.0-V14.2.1 | SP800-53r5.2/SC-8 | ATE_FUN.1 | 5.2.6 | T1 |
| TC-U13 | REQ-20 | v5.0.0-V14.2.1 | SP800-53r5.2/SC-8 | ATE_FUN.1 | 5.2.6 | T1 |
| TC-U14 | REQ-20 | - | SP800-53r5.2/CM-7 | ATE_FUN.1 | 5.2.1 | T1 |
| TC-U15 | REQ-21 | v5.0.0-V8.3.1 | SP800-53r5.2/AC-3 | ATE_FUN.1 | 5.2.8 | T1 |
| TC-U15 | REQ-22 | v5.0.0-V8.3.1 | SP800-53r5.2/AC-3 | ATE_FUN.1 | 5.2.8 | T1 |
| TC-U16 | REQ-23 | v5.0.0-V16.5.2 | SP800-53r5.2/AU-3 | ATE_FUN.1 | 5.4.1 | T1 |
| TC-U16 | REQ-24 | v5.0.0-V16.5.2 | SP800-53r5.2/AU-3 | ATE_FUN.1 | 5.4.1 | T1 |
| TC-U17 | REQ-25 | v5.0.0-V16.5.3 | SP800-53r5.2/SI-4 | ATE_FUN.1 | 5.2.1 | T1 |
| TC-U18 | REQ-26 | v5.0.0-V16.5.3 | SP800-53r5.2/SC-5 | ATE_FUN.1 | 5.4.1 | T1 |
| TC-U18 | REQ-27 | v5.0.0-V16.5.3 | SP800-53r5.2/SC-5 | ATE_FUN.1 | 5.4.1 | T1 |
| TC-U18 | REQ-28 | - | SP800-53r5.2/SI-4 | ATE_FUN.1 | 5.2.8 | T1 |
| TC-U19 | REQ-29 | v5.0.0-V16.5.3 | SP800-53r5.2/SI-4 | ATE_FUN.1 | 5.2.1 | T1 |
| TC-U19 | REQ-30 | v5.0.0-V16.5.3 | SP800-53r5.2/SI-4 | ATE_FUN.1 | 5.4.1 | T1 |

ASVS / SP 800-53 に直接対応しない行は `-` とした (空欄ではない)。
それ以外は ASVS を v5.0.0、SP 800-53 を Release 5.2、CC を
ISO/IEC 15408-3:2022 の構成要素 ID とする。

## 既存テストとの対応

本仕様は既存 Rust テストを変更しない。既存テストとの主な対応は
[approvals.rs](../ui/src/approvals.rs) と
[policy_client.rs](../ui/src/policy_client.rs) を参照。
実行結果は `evidence/` の test-run JSON に記録する。

TC-U2 / TC-U3 / TC-U4 は 2026-11-18 に状態ロジックのテストを追加し
カバー済み（14 テスト）。TC-U5 / TC-U7〜U12 もカバー済みで、
TC-U8 の 3xx 非追従・明示エラー、TC-U11/12 の非2xx エラー化を含む。
TC-U13 の scope 別フォールバックと TC-U14 のレガシー ENV 警告は
`policy_client.rs` のトークン解決テストでカバー済み。
トークン未設定時にヘッダを送らない挙動は送信側の `if let Some` ガードに依存する。
GPUI の実描画に依存する TC-U1 / TC-U6 は未検証のままとする。

TC-U15〜TC-U18 は I50-C / I52-A / I54-A / I66-C の回帰テストに対応する。
`approvals.rs` の kill switch pin・409 分類と `policy_client.rs` の
probe fail-closed・winner 取得が根拠。I66-C では `/health` 本文の前後空白が
`Err` → 停止側へ倒れる回帰を追加した。実測件数は evidence を参照。

TC-U19 は [policy_client.rs](../ui/src/policy_client.rs) の
`expired_410_reports_expired_without_winner_fetch` /
`expired_410_body_confirmation_fails_closed` /
`expired_410_unreadable_body_is_an_error` /
`expired_410_rewritten_body_is_an_error` /
`expired_410_body_pins_wire_contract` に対応する。共有本文の正典は
`jarvis-core` が持ち、core / policy / UI / mcp-gw の pin が片側変更を検出する。

### エビデンス再生成手順（親が実行）

本仕様の変更で `spec_hash` が変わるため、対応する証跡は再生成対象となる。
現行の証跡ファイル名は [evidence/index.md](../evidence/index.md) の ui 行を
参照する（仕様本文には spec_hash とファイル名を記載しない。記載するとその
記述自体が spec_hash を変えてしまうため）。
[再生成 Runbook](../docs/integration/evidence-regeneration.md) の 7 ステップに
従い、親が以下を実行する（本エージェントは証跡を変更しない）。
`test-policy-approval.md` の変更に伴う policy 証跡も同時に再生成対象である。

1. `sha256sum specs/test-ui.md` で新ハッシュを取得する。
2. `cd ui && cargo test --locked` を実測する。
3. evidence JSON の `spec_hash` と `test_basis_ref.spec_hash` の両方を更新する。
4. `test_basis_ref.requirements` に REQ-29 / REQ-30 を追加し、`results` /
   `timestamp` / `notes` を実測へ同期する（既存 REQ-1〜28 は維持）。
5. ファイル名を `test-run-YYYYMMDD-<新ハッシュ先頭12文字>.json` へ変更する。
6. `evidence/index.md` の `[run-ui]` リンクと件数を更新する。
7. `make check-evidence` で `ok=6 errors=0` を確認する。
