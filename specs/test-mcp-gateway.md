---
type: Test Specification
title: "jarvis-mcp-gw 実行経路 テスト仕様"
description: "policy-first / allowlist / 引数上限 / 承認トークン / egress fail-closed / token 定時間比較 / upstream / liveness"
tags:
  - test-spec
  - jarvis-mcp-gw
  - mcp
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
| test item | `jarvis-mcp-gw` の `execute_tool` と HTTP/MCP 入口 |
| 根拠 | [todo.md](../todo.md) P1-9, P0-2, P0-5 |
| 判定基準 | 実装の観測可能挙動と既存 Rust テストの期待値 |
| 対象外 | native handler 詳細、Cedar/Casbin のポリシ内容 |
| プロファイル | `web-api-l2-moderate` を参照した tailored |
| 基準出典 | [ASVS 5.0.0][asvs], [SP 800-53 r5.2][sp53], [CC Part 3][cc3] |

[asvs]: https://github.com/OWASP/ASVS/releases/tag/v5.0.0_release
[sp53]: https://csrc.nist.gov/pubs/sp/800/53/r5/upd1/final
[cc3]: https://commoncriteriaportal.org/cc/index.cfm

### 要求と test oracle

| ID | 要求 (合否の根拠) |
| --- | --- |
| REQ-1 | 任意のツール呼び出しは最初に policy へ `tool:<name>` で照会する |
| REQ-2 | policy  deny と通信失敗は fail-closed で PolicyDenied とする |
| REQ-3 | ツールは allowlist に載っている場合のみ実行可能 |
| REQ-4 | 登録済み upstream の tool  discovery は allowlist を拡張しない |
| REQ-5 | principal は全体とツール項目の両方で許可されている必要がある |
| REQ-6 | allowlist 欠落/不正は起動失敗とし、空の許可で起動しない |
| REQ-7 | 引数は JSON 直列化後のバイト数で `max_args_size` を判定する |
| REQ-8 | 超過引数は承認・実行前に ArgsTooLarge (HTTP 400) とする |
| REQ-9 | 設定時は `X-Jarvis-Approval-Token` を policy へ送る |
| REQ-10 | 401 / timeout / 通信失敗の承認は fail-closed で拒否する |
| REQ-11 | Approved のみ実行し、Denied/Expired/Pending は実行しない |
| REQ-12 | 承認後の policy 再検査で kill switch 発動を検出する |
| REQ-13 | HTTP は 400/403/500 を正しく使い分ける |
| REQ-14 | opt-in で caller principal を `mcp_principal` に上書きする |
| REQ-15 | `web.fetch` の egress allowlist は fail-closed。未設定・空・不正なら全 fetch を拒否する |
| REQ-16 | host パターンは大小文字を正規化し重複を除く。IP リテラルと loopback は拒否し redirect 先も検査する |
| REQ-17 | gRPC admin_token は固定長 SHA-256 の 32 バイトを早期終了なしで比較する |
| REQ-18 | 提示値・設定値が空なら比較せず Unauthenticated とし、トークン値をログに出さない |
| REQ-19 | 設定済み `[[upstreams]]` はリスナ起動前に登録し、1件でも失敗すれば起動を中止する |
| REQ-20 | 終了シグナルでリスナを即時停止し、in-flight を drain せず正常完了と主張しない |
| REQ-21 | 全 exit 経路（早期 return を含む）で tracer を shutdown する |

## Risk & tier

| risk_id | リスク | tier | 完了基準 |
| --- | --- | --- | --- |
| R-1 | policy を通らず allowlist だけで実行される | T1 | unit >= 0.90, int >= 0.80 |
| R-2 | allowlist 外のツールが upstream 経由で動く | T1 | unit >= 0.90, int >= 0.80 |
| R-3 | 未許可 principal が特権ツールを呼ぶ | T1 | unit >= 0.90 |
| R-4 | 過大引数で承認要求・実行が枠を占有する | T1 | unit >= 0.90 |
| R-5 | 承認トークン未送信で fail-closed になる/素通しする | T1 | int >= 0.80 |
| R-6 | 承認後に kill switch が発動しても実行される | T1 | unit >= 0.90, int >= 0.80 |
| R-7 | allowlist 破損時に空許可で起動する | T2 | unit >= 0.80 |
| R-8 | opt-in 既定 false の残存リスクと監査粒度の低下 | T1 | unit >= 0.90 |
| R-9 | egress allowlist 未設定・不正で外部 host へ fetch され漏洩する | T1 | unit >= 0.90 |
| R-10 | admin token 比較がタイミング依存/空値通過し制御面が乗っ取られる | T1 | unit >= 0.90 |
| R-11 | upstream 登録失敗を無視し機能欠落のまま起動する | T2 | unit >= 0.80 |
| R-12 | 終了時に in-flight を正常完了と誤主張 / tracer が未 shutdown | T2 | unit >= 0.80 |

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
| equivalence partitioning | 5.2.1 | policy 許可/拒否、承認結果の4状態 |
| boundary value analysis | 5.2.3 | max_args_size の max-1/max/max+1 |
| decision table testing | 5.2.6 | policy x allowlist x principal x 承認 |
| state transition testing | 5.2.8 | 承認待ち -> Approved/Denied/Expired |
| scenario testing | 5.2.9 | HTTP/MCP 入口から実行までの往復 |
| error guessing | 5.4.1 | allowlist 破損、kill switch 割込み |

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

### Policy-first と allowlist (REQ-1〜REQ-6)

```gherkin
Scenario: TC-P1 policy 拒否で実行しない
  Given policy が tool:shell.execute を拒否する
  When execute_tool を呼ぶ
  Then PolicyDenied を返す
  Then native handler も upstream も呼ばれない
  When policy への通信が失敗する
  Then 同じく PolicyDenied を返す
```

```gherkin
Scenario: TC-A1 allowlist が単一の情報源
  Given upstream が tool:echo を advertise している
  When allowlist に echo が無い
  Then execute_tool は NotAllowed を返す
  When allowlist に echo を追加する
  Then 登録済み upstream 経由で実行できる
```

```gherkin
Scenario: TC-A2 principal の二重チェック
  Given principal が allowlist の principals に無い
  When 許可済みツールを呼ぶ
  Then PrincipalNotAllowed を返す
  Given principal と tool の allowed_principals が不一致
  When 同じツールを呼ぶ
  Then PrincipalNotAllowed を返す
```

```gherkin
Scenario: TC-A3 設定ファイル破損時の起動失敗
  Given allowlist ファイルが存在しない
  When gateway を起動する
  Then 起動は失敗し、空の許可で稼働しない
  Given allowlist が不正 TOML
  When gateway を起動する
  Then 起動は失敗する
```

```gherkin
Scenario: TC-A4 opt-in の server-side principal
  Given require_server_side_principal=true と mcp_principal=opencode
  When 呼び出し側が principal=jarvis を申告する
  Then 実効 principal は opencode で policy/allowlist/承認/ログに使う
  Given ガードが false (既定)
  When 同じ申告で呼ぶ
  Then 従来どおり caller principal を使う
```

### 引数上限 (REQ-7, REQ-8)

```gherkin
Scenario: TC-S1 直列化バイト数の境界
  Given max_args_size=16 のツール
  When 15 バイトの引数で呼ぶ
  Then 上限チェックを通過する
  When 16 バイトちょうどで呼ぶ
  Then 上限チェックを通過する
  When 17 バイトで呼ぶ
  Then ArgsTooLarge を返す
  Then 承認要求も handler 呼び出しも発生しない
```

```gherkin
Scenario: TC-S2 HTTP での上限エラー
  Given 上限を超える JSON 引数
  When POST /v1/execute を呼ぶ
  Then 400 と {"error": ...} が返る
```

### 承認トークンと承認結果 (REQ-9〜REQ-11)

```gherkin
Scenario: TC-T1 トークン付き承認要求
  Given approval_api_token が設定されている
  When requires_approval のツールを呼ぶ
  Then X-Jarvis-Approval-Token が policy に送られる
  Then policy が受け付ける
```

```gherkin
Scenario: TC-T2 承認経路の fail-closed
  Given policy がトークン不正で 401 を返す
  When requires_approval のツールを呼ぶ
  Then ApprovalRequired を返し実行しない
  Given policy が応答しない
  When 同じツールを呼ぶ
  Then タイムアウト後に ApprovalRequired を返す
```

```gherkin
Scenario: TC-V1 承認結果の分岐
  Given 演算子が Approved を返す
  When 待機中の呼び出しが再開する
  Then ツールが実行される
  Given Denied または Expired が返る
  When 待機中の呼び出しが再開する
  Then ApprovalDenied を返し実行しない
  Given 想定外の Pending が返る
  Then ApprovalRequired を返し実行しない
```

### kill switch 再検査と入口 (REQ-12, REQ-13)

```gherkin
Scenario: TC-K1 承認後の再検査
  Given requires_approval の呼び出しが承認待ち
  When 演算子が承認している間に kill switch が発動する
  Then 再検査で PolicyDenied を返す
  Then 承認済みでもツールは実行されない
```

```gherkin
Scenario: TC-H1 入口ごとのエラー表現
  Given 拒否された呼び出し
  When POST /v1/execute を呼ぶ
  Then 403 と JSON エラーが返る
  When /mcp の tools/call を呼ぶ
  Then tool error として拒否が伝播する
```

### egress / admin token / upstream / liveness (REQ-15〜REQ-21)

```gherkin
Scenario: TC-E1 egress allowlist の fail-closed
  Given egress_allowlist が未設定または空
  When web.fetch で任意の host を呼ぶ
  Then すべての fetch が拒否される
  Given 不正な host パターンが設定されている
  When 設定を読み込む
  Then 空の許可として扱い起動後も全 fetch を拒否する
  Given 許可 host が IP リテラルまたは loopback
  When fetch を試みる
  Then 拒否される
  Given 許可 host への要求が別 host へ redirect する
  When redirect を追従する
  Then redirect 先も allowlist で検査し未許可なら拒否する
```

```gherkin
Scenario: TC-E2 admin token の定時間比較
  Given 正しい admin_token が設定されている
  When 同一トークンで RegisterMcpServer を呼ぶ
  Then 登録が成功する
  Given 提示トークンが誤るまたは空
  When 同じ RPC を呼ぶ
  Then Unauthenticated を返す
  Then 比較は固定 32 バイトを早期終了なしで行う
  Given 設定値が空
  When トークンを提示して RPC を呼ぶ
  Then 比較せず Unauthenticated としトークン値をログに出さない
```

```gherkin
Scenario: TC-E3 起動時の upstream 登録
  Given 設定に 2 つの [[upstreams]] がある
  When gateway を起動する
  Then リスナ開始前に両方が登録される
  Given 1 つの upstream 登録が失敗する
  When gateway を起動する
  Then 起動は失敗し、欠落したまま稼働しない
```

```gherkin
Scenario: TC-E4 即時停止と tracer 完了
  Given リスナが稼働中
  When SIGTERM または Ctrl-C を受ける
  Then 全 accept loop が停止し即時停止する
  Then in-flight 要求は drain せず正常完了と主張しない
  When リスナエラーまたは早期 return で終了する
  Then すべての exit 経路で tracer が shutdown される
```

## Traceability

| Scenario | Req | ASVS | 800-53 | CC | Technique | Tier |
| --- | --- | --- | --- | --- | --- | --- |
| TC-P1 | REQ-1 | v5.0.0-V8.3.1 | SP800-53r5.2/AC-3 | ATE_FUN.1 | 5.2.1 | T1 |
| TC-P1 | REQ-2 | v5.0.0-V16.5.3 | SP800-53r5.2/AC-3 | ATE_FUN.1 | 5.4.1 | T1 |
| TC-A1 | REQ-3 | v5.0.0-V8.2.2 | SP800-53r5.2/AC-3 | ATE_FUN.1 | 5.2.9 | T1 |
| TC-A1 | REQ-4 | v5.0.0-V8.3.1 | SP800-53r5.2/CM-7 | ATE_FUN.1 | 5.2.9 | T1 |
| TC-A2 | REQ-5 | v5.0.0-V8.3.3 | SP800-53r5.2/AC-6 | ATE_FUN.1 | 5.2.6 | T1 |
| TC-A3 | REQ-6 | v5.0.0-V16.5.3 | SP800-53r5.2/CM-7 | ATE_FUN.1 | 5.4.1 | T2 |
| TC-A4 | REQ-14 | v5.0.0-V8.3.3 | SP800-53r5.2/AC-3 | ATE_FUN.1 | 5.2.6 | T1 |
| TC-S1 | REQ-7 | v5.0.0-V2.2.1 | SP800-53r5.2/SC-5 | ATE_COV.2 | 5.2.3 | T1 |
| TC-S1 | REQ-8 | v5.0.0-V2.3.2 | SP800-53r5.2/SC-5 | ATE_COV.2 | 5.2.3 | T1 |
| TC-S2 | REQ-8 | v5.0.0-V16.5.1 | SP800-53r5.2/SC-5 | ATE_FUN.1 | 5.2.9 | T2 |
| TC-T1 | REQ-9 | v5.0.0-V13.2.3 | SP800-53r5.2/IA-5 | ATE_FUN.1 | 5.2.1 | T1 |
| TC-T2 | REQ-10 | v5.0.0-V16.5.3 | SP800-53r5.2/IA-5 | ATE_FUN.1 | 5.4.1 | T1 |
| TC-V1 | REQ-11 | v5.0.0-V2.3.5 | SP800-53r5.2/AC-3 | ATE_FUN.1 | 5.2.8 | T1 |
| TC-K1 | REQ-12 | v5.0.0-V8.3.2 | SP800-53r5.2/AC-3 | ATE_FUN.1 | 5.2.8 | T1 |
| TC-H1 | REQ-13 | v5.0.0-V16.5.1 | SP800-53r5.2/AC-3 | ATE_FUN.1 | 5.2.9 | T2 |
| TC-E1 | REQ-15 | v5.0.0-V16.5.3 | SP800-53r5.2/SC-7 | ATE_FUN.1 | 5.4.1 | T1 |
| TC-E1 | REQ-16 | v5.0.0-V8.3.1 | SP800-53r5.2/SC-7 | ATE_FUN.1 | 5.2.1 | T1 |
| TC-E2 | REQ-17 | v5.0.0-V13.2.3 | SP800-53r5.2/IA-5 | ATE_FUN.1 | 5.2.1 | T1 |
| TC-E2 | REQ-18 | v5.0.0-V13.2.3 | SP800-53r5.2/IA-5 | ATE_FUN.1 | 5.2.1 | T1 |
| TC-E3 | REQ-19 | v5.0.0-V16.5.3 | SP800-53r5.2/CM-7 | ATE_FUN.1 | 5.4.1 | T2 |
| TC-E4 | REQ-20 | v5.0.0-V16.5.1 | SP800-53r5.2/CM-7 | ATE_FUN.1 | 5.2.8 | T2 |
| TC-E4 | REQ-21 | v5.0.0-V16.5.1 | SP800-53r5.2/CM-7 | ATE_FUN.1 | 5.2.8 | T2 |

未適用列は `-` ではなく、全行に基準 ID を割り当てた。
ASVS は v5.0.0、SP 800-53 は Release 5.2、CC は ISO/IEC 15408-3:2022
の構成要素 ID を使用する。

## 既存テストとの対応

本仕様は既存 Rust テストを変更しない。既存テストとの主な対応は
[lib.rs](../rust/jarvis-mcp-gw/src/lib.rs),
[allowlist.rs](../rust/jarvis-mcp-gw/src/allowlist.rs),
[approval.rs](../rust/jarvis-mcp-gw/src/approval.rs),
[http.rs](../rust/jarvis-mcp-gw/src/http.rs) を参照。
実行結果は `evidence/` の test-run JSON に記録する。

TC-E1〜TC-E4 は [sandbox.rs](../rust/jarvis-mcp-gw/src/sandbox.rs)、
[grpc.rs](../rust/jarvis-mcp-gw/src/grpc.rs)、
[config.rs](../rust/jarvis-mcp-gw/src/config.rs)、
[main.rs](../rust/jarvis-mcp-gw/src/main.rs) の回帰テストに対応する。
実測件数は `evidence/` の test-run JSON を参照（仕様更新後は再実行が必要）。
