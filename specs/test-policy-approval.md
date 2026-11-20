---
type: Test Specification
title: "jarvis-policy 承認フロー テスト仕様"
description: "kill switch / API-admin トークン分離 / 自己承認拒否 / キュー上限 / 重複 ID の動的試験設計"
tags:
  - test-spec
  - jarvis-policy
  - approval
  - security
generated:
  by: spec-writer
  at: "2026-11-20T03:22:00+09:00"
status: draft
---

適合形態は `tailored` のみ。ASVS / SP 800-53 / Common Criteria への
適合宣言は行わない。本ファイルは仕様のみで、テストコードは含まない。

## Test basis

| 項目 | 内容 |
| --- | --- |
| test item | `jarvis-policy` の承認ストア・kill switch・HTTP 承認ルート |
| 根拠 | [todo.md](../todo.md) P1-9 (R4-SPEC-001〜007), P0-4 |
| 判定基準 | 実装の観測可能挙動と既存 Rust テストの期待値 |
| 対象外 | Cedar/Casbin のポリシ内容、UI、監査ログ永続化 |
| プロファイル | `web-api-l2-moderate` を参照した tailored |
| 基準出典 | [ASVS 5.0.0][asvs], [SP 800-53 r5.2][sp53], [CC Part 3][cc3] |

[asvs]: https://github.com/OWASP/ASVS/releases/tag/v5.0.0_release
[sp53]: https://csrc.nist.gov/pubs/sp/800/53/r5/upd1/final
[cc3]: https://commoncriteriaportal.org/cc/index.cfm

### 要求と test oracle

| ID | 要求 (合否の根拠) |
| --- | --- |
| REQ-1 | kill switch の発動は管理者トークン一致時のみ成功する |
| REQ-2 | 不一致トークンでは 401 を返し、状態を変更しない |
| REQ-3 | コミット前に発動中は check=false、承認要求は Denied、決定は 409（コミット中は REQ-15） |
| REQ-4 | 状態は再起動で復元され、破損時は発動扱い (fail-closed) |
| REQ-5 | トークン設定時、承認ルートはヘッダ検証で 401/200 を分岐 |
| REQ-6 | トークンは承認ルートのみ、health/check は認証対象外 |
| REQ-7 | `max_pending` 超過はストア層で Denied 互換、HTTP では 503 を返す |
| REQ-8 | 上限境界は cap-1 受付、cap 受付、cap+1 拒否 |
| REQ-9 | 同一 ID の再登録は元の保留を守り Denied する |
| REQ-10 | 二重決定 409、未知名 404、不正 UUID 400、期限切れは 410 Gone + 固定本文 `APPROVAL_EXPIRED_BODY`、競合は決定優先 |
| REQ-11 | RwLock poison / persist 失敗で arm し、解除失敗を返す |
| REQ-12 | 承認トークンは API scope と admin scope に分離され、approve/deny は admin トークンでのみ成功する |
| REQ-13 | API トークンのみの呼び出しは request/list/status で成功し、approve/deny では 401 となり自己承認が拒否される |
| REQ-14 | admin トークン未設定時は API トークンへフォールバックし、起動時に警告を出す（職務分離は無効） |
| REQ-15 | コミット中の kill switch 発動時は 200/Approved を返すが、実行は mcp-gw 再判定で fail-closed 拒否 |
| REQ-16 | 503: kill switch は `HEALTH_KILL_SWITCH_BODY`、policy 不在は `HEALTH_NO_POLICY_BODY`。kill switch 優先 |
| REQ-17 | 正常時は 200 + `HEALTH_OK_BODY` を返し `reason` キーを含めない |
| REQ-18 | 未知/前後空白/余分・重複キーの本文は emit せず、監視側は完全一致以外を不完全 probe として fail-closed に倒す |

`/health` の契約は `jarvis-core` の共有固定本文 3 種類に閉じる。emit 側は
kill switch 発動を `HEALTH_KILL_SWITCH_BODY`、policy source 不在を
`HEALTH_NO_POLICY_BODY`（いずれも 503）、正常を `HEALTH_OK_BODY`（200）で返す。
kill switch が policy source 不在より優先する。監視側（UI の probe）は
**trim なしの完全バイト一致**でのみ分類し、未知本文・前後空白・余分／重複
キー・読取不能・`200` / `503` 以外の status は不完全 probe として fail-closed
（`Err`）へ倒す。core / policy / UI の pin が片側変更を検出する。

## Risk & tier

| risk_id | リスク | tier | 完了基準 |
| --- | --- | --- | --- |
| R-1 | 権限なき発動/解除で全自動処理を停止・再開される | T1 | unit >= 0.90, int >= 0.80 |
| R-2 | 発動中の承認通過 / health 本文 drift で非常停止が無効化される | T1 | unit >= 0.90, int >= 0.80 |
| R-3 | 状態ファイル破損でサイレントに解除される | T1 | unit >= 0.90 |
| R-4 | トークン欠落/誤値で承認 API が素通しになる | T1 | int >= 0.80 |
| R-5 | キュー無制限成長と ID 上書きによる決定奪取 | T1 | unit >= 0.90 |
| R-6 | 上限境界の off-by-one で fail-open する | T2 | unit >= 0.80 |
| R-7 | 決定競合で誤応答・副作用が残る | T2 | unit >= 0.80 |
| R-8 | A-M18: poison/永続化失敗が見えず解除扱いになる | T1 | unit >= 0.90 |
| R-9 | API トークン保持者が自分の要求を承認し職務分離が効かない | T1 | int >= 0.80 |
| R-10 | コミット中の発動で決定を未公開化し、再試行が AlreadyDecided で失敗して UI/DB が不整合になる | T1 | unit >= 0.90, int >= 0.80 |

tier は `web-api-l2-moderate` の定義に従う。

- T1: 認証・認可に関わる API (unit 0.90 / integration 0.80)
- T2: 主要業務ロジック (unit 0.80 / integration 0.60)
- T3: 補助・周辺機能 (unit 0.60 / integration 0.40)

T3 は本スライスでは未使用だが、tier 尺度 T1-T3 の定義として保持する。
完了基準は上記カバレッジに加え、全シナリオの oracle が満たされること。

## Design technique

29119-2:2021 8.2 の TD1 (test model) / TD2 (coverage item) /
TD3 (test case) の順で導出した。

| 技法 | 29119-4 節 | 適用箇所 |
| --- | --- | --- |
| equivalence partitioning | 5.2.1 | トークン正常/欠落/誤値、kill switch 状態、health 3 本文 |
| boundary value analysis | 5.2.3 | pending 0/cap-1/cap/cap+1、timeout 前後 |
| decision table testing | 5.2.6 | kill switch x トークン x ID 状態、scope(API/admin) x トークン x 決定種別、commit gate x 発動タイミング |
| state transition testing | 5.2.8 | pending -> approved/denied/expired |
| scenario testing | 5.2.9 | HTTP 往復 (request -> list -> decide) |
| error guessing | 5.4.1 | 破損 state file、二重決定、同一 ID |

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

### Kill switch (REQ-1〜REQ-4, REQ-15)

```gherkin
Scenario: TC-K1 管理者トークンによる発動
  Given kill switch が未発動
  When 誤トークンで activate を呼ぶ
  Then 401 を返し status.activated は false
  When 正当トークンで activate を呼ぶ
  Then 200 と activated=true の status が返る
```

```gherkin
Scenario: TC-K2 発動中の fail-closed (決定のコミットより前に発動)
  Given kill switch がコミット前に発動中
  When /v1/check と承認要求を呼ぶ
  Then allowed=false と Denied が返る
  When approve と deny を呼ぶ
  Then いずれも 409 KillSwitchActivated を返す
  When 正当トークンで deactivate を呼ぶ
  Then 以降の承認決定が再有効化される
```

```gherkin
Scenario: TC-K5 コミット中の kill switch 発動 (mid-commit, REQ-15)
  Given 承認要求 ID=X が pending
  And commit gate が永続化の直前で authority を一度だけ再判定する
  When 永続化コミット中に kill switch が発動する（gate 通過後）
  Then 決定はコミットされて公開され HTTP 200 + Approved が返る
  And approval_status(ID=X) は Approved を返す
  When mcp-gw がツール実行前に post-approval 再判定を行う
  Then kill switch が発動中なら実行は fail-closed で拒否される
  When 正当トークンで deactivate を呼ぶ
  Then 以降のツール実行は再判定で許可され得る
```

```gherkin
Scenario: TC-K3 状態復元と破損時 fail-closed
  Given 発動済み state file が存在する
  When プロセスを再起動する
  Then activated=true と reason が復元される
  Given state file が不正 JSON または読取不能
  When プロセスを起動する
  Then activated=true となり state file は上書きされない
```

```gherkin
Scenario: TC-K4 poison と persist 失敗の fail-closed (A-M18)
  Given reason / activated_by の RwLock が poison している
  When status() を呼ぶ
  Then poison 理由で arm され activated=true を返す
  Given state file への書き込みが失敗する
  When activate() を呼ぶ
  Then arm されたまま activated=true を維持する
  When 正当トークンで deactivate() を呼ぶ
  Then "remains activated" エラーを返す
```

### トークン認証 (REQ-5, REQ-6)

```gherkin
Scenario: TC-A1 承認ルートのトークン検証
  Given approval_api_token が設定されている
  When ヘッダなしで /v1/approvals を呼ぶ
  Then 401 を返す
  When 誤トークンで呼ぶ
  Then 401 を返す
  When 正当トークンで呼ぶ
  Then 200 と pending 一覧が返る
  # I61-A: BareJson の JSON 入力受け取り契約（request/approve/deny 共通）
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
Scenario: TC-A2 認証スコープの境界
  Given approval_api_token が設定されている
  When /health と /v1/check を呼ぶ
  Then いずれもトークンなしで 200 を返す
  Given approval_api_token が未設定
  When 承認ルートを呼ぶ
  Then 従来の loopback 動作が維持される
```

### トークン scope 分離と自己承認拒否 (REQ-12〜REQ-14)

```gherkin
Scenario: TC-A3 API / admin の scope 分離
  Given approval_api_token と approval_admin_token が異なる値で設定されている
  When API トークンで /v1/approvals を呼ぶ
  Then 200 と pending 一覧が返る
  When admin トークンで /v1/approvals を呼ぶ
  Then 401 を返す（admin トークンは API scope を開かない）
  When API トークンで approve と deny を呼ぶ
  Then いずれも 401 を返す
  When admin トークンで approve を呼ぶ
  Then 200 となり承認要求は Approved になる
```

```gherkin
Scenario: TC-A4 自己承認の拒否
  Given API トークンで発行した保留要求が存在する
  When 同じ API トークンでその要求の approve を呼ぶ
  Then 401 を返し、要求は pending のまま決定されない
  When admin トークンで approve を呼ぶ
  Then 200 となり approved_by は admin 側の値になる
```

```gherkin
Scenario: TC-A5 admin トークン未設定時の互換挙動
  Given approval_admin_token が未設定
  When approve/deny を API トークンで呼ぶ
  Then 従来どおり 200 で決定できる
  Then 起動時にフォールバックと職務分離が無効である旨の警告が出る
```

### キュー上限 (REQ-7, REQ-8)

```gherkin
Scenario: TC-Q1 上限超過の fail-closed
  Given max_pending=1 で先頭要求が pending
  When 別 ID で新規要求を登録する
  Then ストア層 request() は Denied(reason="approval queue full") を返す
  Then try_request() は ApprovalError::QueueFull を返す
  Then HTTP /v1/approval は 503 を返す（通常 deny は 200 decision を維持）
  Then pending 数は 1 のまま増加しない
```

```gherkin
Scenario: TC-Q2 上限境界と枠解放
  Given max_pending=N のストア
  When pending 数が N-1, N で要求する
  Then いずれも登録される
  When pending 数が N+1 で要求する
  Then Denied を返す
  When 決定済みエントリが残る
  Then 決定済みは pending 枠を占有しない
```

### 重複 ID と決定競合 (REQ-9, REQ-10)

```gherkin
Scenario: TC-D1 同一 ID の再登録
  Given ID=X の要求が pending
  When 同一 ID=X で再登録する
  Then Denied(reason="duplicate approval id") を返す
  Then 元の待機チャネルは保持される
  When 演算子が ID=X を approve する
  Then 元の要求が Approved を受け取る
```

```gherkin
Scenario: TC-D2 決定の排他とエラー応答
  Given ID=X が決定済み
  When 同じ ID を再決定する
  Then 409 AlreadyDecided を返す
  When 未知名で status を呼ぶ
  Then 404 NotFound を返す
  When 不正 UUID で approve を呼ぶ
  Then 400 BadRequest を返す
  Given timeout と決定が競合する
  Then 決定値が Expired より優先される
```

```gherkin
  # I103-D: Round 93 契約マーカー
Scenario: TC-D3 期限切れ要求の 410 Gone (REQ-10)
  Given 決定期限を過ぎた保留要求 ID=X が pending で残っている
  When 期限切れの ID=X を approve または deny する
  Then 409 ではなく 410 Gone を返す
  And 本文は固定本文 APPROVAL_EXPIRED_BODY (`approval request expired`) で 409 本文と一致しない
  And 決定はスタンプされず watch 通知も出ず status は pending のまま
  When 期限切れと kill switch 発動が競合する
  Then kill switch の 409 が優先され halt が残留する
```

### /health の固定本文契約 (REQ-16〜REQ-18)

```gherkin
Scenario: TC-H1 /health の 200 / 503 固定本文
  Given policy source と kill switch が通常状態
  When /health をトークンなしで呼ぶ
  Then status は 200 で本文は HEALTH_OK_BODY と完全バイト一致し reason キーは無い
  Given kill switch が発動中
  When /health を呼ぶ
  Then status は 503 で本文は HEALTH_KILL_SWITCH_BODY と完全バイト一致する
  Given kill switch を解除し policy source を読めない
  When /health を呼ぶ
  Then status は 503 で本文は HEALTH_NO_POLICY_BODY と完全バイト一致する
  Given kill switch 発動と policy source 不在が同時に起きる
  When /health を呼ぶ
  Then kill switch 本文が優先される

Scenario: TC-H2 未知本文の fail-closed
  Given /health が前後空白つき、余分／重複キー、または未知の本文を返す
  When 監視側が probe を分類する
  Then 完全バイト一致ではないため不完全 probe として fail-closed に倒れる
  Given 本文が読取不能または status が 200/503 以外
  When 同じく分類する
  Then 正常 (active=false) として扱われず停止側 (active=true) へ倒れる
```

## Traceability

| Scenario | Req | ASVS | 800-53 | CC | Technique | Tier |
| --- | --- | --- | --- | --- | --- | --- |
| TC-K1 | REQ-1 | v5.0.0-V8.3.1 | SP800-53r5.2/AC-3 | ATE_FUN.1 | 5.2.6 | T1 |
| TC-K1 | REQ-2 | v5.0.0-V16.5.3 | SP800-53r5.2/AC-3 | ATE_FUN.1 | 5.4.1 | T1 |
| TC-K2 | REQ-3 | v5.0.0-V8.3.2 | SP800-53r5.2/AC-3 | ATE_FUN.1 | 5.2.8 | T1 |
| TC-K3 | REQ-4 | v5.0.0-V16.5.3 | SP800-53r5.2/AC-3 | ATE_FUN.1 | 5.4.1 | T1 |
| TC-K4 | REQ-11 | v5.0.0-V16.5.3 | SP800-53r5.2/AC-3 | ATE_FUN.1 | 5.4.1 | T1 |
| TC-K5 | REQ-15 | v5.0.0-V8.3.2 | SP800-53r5.2/AC-3 | ATE_FUN.1 | 5.2.8 | T1 |
| TC-A1 | REQ-5 | v5.0.0-V8.3.1 | SP800-53r5.2/IA-5 | ATE_FUN.1 | 5.2.1 | T1 |
| TC-A2 | REQ-6 | v5.0.0-V8.1.1 | SP800-53r5.2/AC-6 | ATE_FUN.1 | 5.2.1 | T2 |
| TC-A3 | REQ-12 | v5.0.0-V8.3.3 | SP800-53r5.2/AC-3 | ATE_FUN.1 | 5.2.9 | T1 |
| TC-A4 | REQ-13 | v5.0.0-V8.3.1 | SP800-53r5.2/AC-3 | ATE_FUN.1 | 5.2.9 | T1 |
| TC-A5 | REQ-14 | v5.0.0-V16.5.3 | SP800-53r5.2/IA-5 | ATE_FUN.1 | 5.2.1 | T2 |
| TC-Q1 | REQ-7 | v5.0.0-V2.4.1 | SP800-53r5.2/SC-5 | ATE_COV.2 | 5.2.9 | T1 |
| TC-Q2 | REQ-8 | v5.0.0-V2.3.2 | SP800-53r5.2/SC-5 | ATE_COV.2 | 5.2.3 | T2 |
| TC-D1 | REQ-9 | v5.0.0-V15.4.2 | SP800-53r5.2/AC-3 | ATE_FUN.1 | 5.2.8 | T1 |
| TC-D2 | REQ-10 | v5.0.0-V2.3.4 | SP800-53r5.2/AC-3 | ATE_FUN.1 | 5.4.1 | T2 |
| TC-D3 | REQ-10 | v5.0.0-V2.3.4 | SP800-53r5.2/AC-3 | ATE_FUN.1 | 5.2.8 | T2 |
| TC-H1 | REQ-16 | v5.0.0-V16.5.3 | SP800-53r5.2/SI-4 | ATE_FUN.1 | 5.2.6 | T1 |
| TC-H1 | REQ-17 | v5.0.0-V8.1.1 | SP800-53r5.2/CM-7 | ATE_FUN.1 | 5.2.1 | T1 |
| TC-H2 | REQ-18 | v5.0.0-V16.5.3 | SP800-53r5.2/SI-4 | ATE_FUN.1 | 5.4.1 | T1 |

未適用列は `-` ではなく、全行に基準 ID を割り当てた。
ASVS は v5.0.0、SP 800-53 は Release 5.2、CC は ISO/IEC 15408-3:2022
の構成要素 ID を使用する。

## 既存テストとの対応

既存テストは保持しつつ回帰テストを追加した。主な対応は
[approval.rs](../rust/jarvis-policy/src/approval.rs),
[kill_switch.rs](../rust/jarvis-policy/src/kill_switch.rs),
[http.rs](../rust/jarvis-policy/src/http.rs),
[lib.rs](../rust/jarvis-policy/src/lib.rs) を参照。
A-M18 (TC-K4) は poison 検知 / persist 失敗 / deactivate エラー伝播の
回帰テストでカバー済み（49 テスト、エビデンス生成時点のスナップショット）。
TC-A3〜A5 は `admin_token_splits_decision_authority_from_api_token` /
`api_token_still_approves_when_admin_token_is_unset` /
`blank_configured_admin_token_cannot_be_bypassed_with_empty_header`
でカバーする。実行結果は `evidence/` の test-run JSON に記録する。

I46-A (Round 44) の commit_gate 1 回化は
[lib.rs](../rust/jarvis-policy/src/lib.rs) の
`kill_switch_during_commit_is_not_published` で回帰確認できる。
同テストの期待値は既に `Approved` へ更新済みだが、名前は旧契約
（未公開）のままで期待値と逆である。lib.rs は本タスクの担当外のため
改名は行わず、`kill_switch_during_commit_is_published_but_execution_is_fail_closed`
への変更を提案する。TC-K5 の UI 側は
[approvals.rs](../ui/src/approvals.rs) の
`status_for_decision_with_kill_switch` と
[policy_client.rs](../ui/src/policy_client.rs) の health プローブでカバーする。

文中のテスト件数（本ファイルおよび他 spec の「14 テスト」等）は
エビデンス生成時点のスナップショットであり、現時点の実測値ではない。
実測は `cargo test` の出力で確認し、エビデンス JSON の再生成は
本タスクの範囲外（別途実施）とする。

### 期限切れ 410 の根拠 (TC-D3 / REQ-10)

既存 TC-D2（REQ-10、決定エラー応答）の分岐として、期限切れを 410 Gone
（固定本文 `APPROVAL_EXPIRED_BODY`）に分離する TC-D3 を追加した。
根拠となる既存テスト関数は次のとおりで、新規テスト関数の追加は不要。

- [http.rs](../rust/jarvis-policy/src/http.rs) の
  `expired_approval_maps_to_gone_without_reusing_409_bodies`
  — `ApprovalError::Expired` が 410 Gone へ写像され、固定本文が
  `ALREADY_DECIDED` / `KILL_SWITCH` の 409 本文と不一致であることを検証。
- [approval.rs](../rust/jarvis-policy/src/approval.rs) の
  `expired_decide_reports_expired_without_stamping_or_committing`
  — 期限切れ決定がスタンプ・永続化・watch 通知を行わないことを検証。
- [jarvis-core/src/lib.rs](../rust/jarvis-core/src/lib.rs) の
  `approval_expired_body_pins_the_wire_contract`
  — 固定本文 `approval request expired` の正典をピン留め。

### /health 固定本文の根拠 (TC-H1 / TC-H2 / REQ-16〜REQ-18)

[http.rs](../rust/jarvis-policy/src/http.rs) の `health()` は kill switch →
policy source 不在 → 正常の順に判定し、3 種類の共有固定本文のみを返す。
根拠となる既存テストは次のとおりで、新規テスト関数の追加は不要。

- [http.rs](../rust/jarvis-policy/src/http.rs) の
  `health_wire_bodies_match_core_constants`
  — 3 本文が `jarvis-core` の共有定数とバイト単位で一致し、実際の `/health`
  応答が kill switch / policy 不在 / 正常でそれぞれ固定本文になることを検証。
- [http.rs](../rust/jarvis-policy/src/http.rs) の
  `health_wire_vocabulary_matches_core_constants` / `health_reports_ok_without_reason_when_service_is_healthy`
  — 語彙と正常時 200 の契約を検証。
- [policy_client.rs](../ui/src/policy_client.rs) の health probe テスト
  — 完全一致以外（前後空白・未知本文・読取不能）が fail-closed へ倒れることを
  検証（UI 仕様 REQ-26 / REQ-27 を参照）。

### エビデンス再生成（完了済み）

現行証跡は `evidence/test-run-20261120-f32635cab7e6.json`
（`spec_hash` `sha256:f32635cab7e689de95b3a77fe9ec2eababff62a8c9bbda0707091d7cc4d48968`）。
再生成は完了済みであり、存在しない旧ファイル名を「再生成手順」として残さないこと。

本タスクで TC-D3 に `I103-D` 契約マーカーを追加したため、親が
[再生成 Runbook](../docs/integration/evidence-regeneration.md) に従って
証跡を再生成する（ファイル名・ハッシュ・`notes` を更新）。REQ-16〜18 の
`requirements` 同期と `test-ui.md` の UI 証跡再生成も同 Runbook に従う。
`notes` には 410 Gone / `APPROVAL_EXPIRED_BODY` を明記すること。
本エージェントは証跡を変更しない。以降の再生成は同 Runbook に従う。
