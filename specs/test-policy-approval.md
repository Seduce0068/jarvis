---
type: Test Specification
title: "jarvis-policy 承認フロー テスト仕様"
description: "kill switch / トークン認証 / キュー上限 / 重複 ID の動的試験設計"
tags:
  - test-spec
  - jarvis-policy
  - approval
  - security
generated:
  by: spec-writer
  at: "2026-11-18T01:13:47+09:00"
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
| REQ-3 | 発動中は check=false、承認要求は Denied、決定は 409 |
| REQ-4 | 状態は再起動で復元され、破損時は発動扱い (fail-closed) |
| REQ-5 | トークン設定時、承認ルートはヘッダ検証で 401/200 を分岐 |
| REQ-6 | トークンは承認ルートのみ、health/check は認証対象外 |
| REQ-7 | `max_pending` 超過はストア層で Denied 互換、HTTP では 503 を返す |
| REQ-8 | 上限境界は cap-1 受付、cap 受付、cap+1 拒否 |
| REQ-9 | 同一 ID の再登録は元の保留を守り Denied する |
| REQ-10 | 二重決定 409、未知名 404、不正 UUID 400、競合は決定優先 |
| REQ-11 | RwLock poison / persist 失敗で arm し、解除失敗を返す |

## Risk & tier

| risk_id | リスク | tier | 完了基準 |
| --- | --- | --- | --- |
| R-1 | 権限なき発動/解除で全自動処理を停止・再開される | T1 | unit >= 0.90, int >= 0.80 |
| R-2 | 発動中の承認通過で非常停止が無効化される | T1 | unit >= 0.90, int >= 0.80 |
| R-3 | 状態ファイル破損でサイレントに解除される | T1 | unit >= 0.90 |
| R-4 | トークン欠落/誤値で承認 API が素通しになる | T1 | int >= 0.80 |
| R-5 | キュー無制限成長と ID 上書きによる決定奪取 | T1 | unit >= 0.90 |
| R-6 | 上限境界の off-by-one で fail-open する | T2 | unit >= 0.80 |
| R-7 | 決定競合で誤応答・副作用が残る | T2 | unit >= 0.80 |
| R-8 | A-M18: poison/永続化失敗が見えず解除扱いになる | T1 | unit >= 0.90 |

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
| equivalence partitioning | 5.2.1 | トークン正常/欠落/誤値、kill switch 状態 |
| boundary value analysis | 5.2.3 | pending 0/cap-1/cap/cap+1、timeout 前後 |
| decision table testing | 5.2.6 | kill switch x トークン x ID 状態 |
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

### Kill switch (REQ-1〜REQ-4)

```gherkin
Scenario: TC-K1 管理者トークンによる発動
  Given kill switch が未発動
  When 誤トークンで activate を呼ぶ
  Then 401 を返し status.activated は false
  When 正当トークンで activate を呼ぶ
  Then 200 と activated=true の status が返る
```

```gherkin
Scenario: TC-K2 発動中の fail-closed
  Given kill switch が発動中
  When /v1/check と承認要求を呼ぶ
  Then allowed=false と Denied が返る
  When approve と deny を呼ぶ
  Then いずれも 409 KillSwitchActivated を返す
  When 正当トークンで deactivate を呼ぶ
  Then 以降の承認決定が再有効化される
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

## Traceability

| Scenario | Req | ASVS | 800-53 | CC | Technique | Tier |
| --- | --- | --- | --- | --- | --- | --- |
| TC-K1 | REQ-1 | v5.0.0-V8.3.1 | SP800-53r5.2/AC-3 | ATE_FUN.1 | 5.2.6 | T1 |
| TC-K1 | REQ-2 | v5.0.0-V16.5.3 | SP800-53r5.2/AC-3 | ATE_FUN.1 | 5.4.1 | T1 |
| TC-K2 | REQ-3 | v5.0.0-V8.3.2 | SP800-53r5.2/AC-3 | ATE_FUN.1 | 5.2.8 | T1 |
| TC-K3 | REQ-4 | v5.0.0-V16.5.3 | SP800-53r5.2/AC-3 | ATE_FUN.1 | 5.4.1 | T1 |
| TC-K4 | REQ-11 | v5.0.0-V16.5.3 | SP800-53r5.2/AC-3 | ATE_FUN.1 | 5.4.1 | T1 |
| TC-A1 | REQ-5 | v5.0.0-V8.3.1 | SP800-53r5.2/IA-5 | ATE_FUN.1 | 5.2.1 | T1 |
| TC-A2 | REQ-6 | v5.0.0-V8.1.1 | SP800-53r5.2/AC-6 | ATE_FUN.1 | 5.2.1 | T2 |
| TC-Q1 | REQ-7 | v5.0.0-V2.4.1 | SP800-53r5.2/SC-5 | ATE_COV.2 | 5.2.9 | T1 |
| TC-Q2 | REQ-8 | v5.0.0-V2.3.2 | SP800-53r5.2/SC-5 | ATE_COV.2 | 5.2.3 | T2 |
| TC-D1 | REQ-9 | v5.0.0-V15.4.2 | SP800-53r5.2/AC-3 | ATE_FUN.1 | 5.2.8 | T1 |
| TC-D2 | REQ-10 | v5.0.0-V2.3.4 | SP800-53r5.2/AC-3 | ATE_FUN.1 | 5.4.1 | T2 |

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
回帰テストでカバー済み（49 テスト）。
実行結果は `evidence/` の test-run JSON に記録する。
