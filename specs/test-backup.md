---
type: Test Specification
title: "ローカルバックアップ テスト仕様"
description: "backup-local.sh / verify-backup.sh / install-backup-timer.sh"
tags:
  - test-spec
  - backup
  - shell
  - integrity
generated:
  by: spec-writer
  at: "2026-11-18T02:07:37+09:00"
status: draft
---

適合形態は `tailored` のみ。ASVS / SP 800-53 / Common Criteria への
適合宣言は行わない。本ファイルは仕様のみで、テストコードは含まない。

## Test basis

| 項目 | 内容 |
| --- | --- |
| test item | [backup-local.sh](../infra/scripts/backup-local.sh) 等3本 |
| 根拠 | [todo.md](../todo.md) P1-9, BK-1, BK-2, P1-8 |
| 判定基準 | 既存シェル回帰テストとスクリプトの観測可能挙動 |
| 対象外 | 実機 systemd/PG/Qdrant、リストア実行、秘密情報の値 |
| プロファイル | `web-api-l2-moderate` を参照した tailored |
| 基準出典 | [ASVS 5.0.0][asvs], [SP 800-53 r5.2][sp53], [CC Part 3][cc3] |

[asvs]: https://github.com/OWASP/ASVS/releases/tag/v5.0.0_release
[sp53]: https://csrc.nist.gov/pubs/sp/800/53/r5/upd1/final
[cc3]: https://commoncriteriaportal.org/cc/index.cfm

### 要求と test oracle

| ID | 要求 (合否の根拠) |
| --- | --- |
| REQ-1 | `backup-local.sh` は既定で dry-run で、何も書き込まない |
| REQ-2 | `--apply` は 0700 の set を作り manifest にサイズと SHA-256 を記録 |
| REQ-3 | manifest 先頭に UTC の created_at を記録する |
| REQ-4 | COMPLETE は全手順成功後のみ書き、部分 set は検証で拒否する |
| REQ-5 | created_at の無い legacy set は互換のため受け入れる |
| REQ-6 | 秘密情報 (.env/bws.env/token) はコピーしない |
| REQ-7 | retention は `--retain-days` 指定時のみ動作する |
| REQ-8 | `verify-backup.sh` はmanifest のサイズとハッシュを全件照合する |
| REQ-9 | ファイル欠損・不一致は失敗として exit 1 を返す |
| REQ-10 | 全 set が既定、 `--latest` と `--backup-dir` で対象を選ぶ |
| REQ-11 | set が無い場合はエラーで exit 1 する |
| REQ-12 | dump/snapshot の形式検査を行い、検証は読み取り専用 |
| REQ-13 | timer 導入は dry-run 既定で、`--apply` は root 必須 |
| REQ-14 | 導入後エラーは fail() で rollback し、素の exit 1 を使わない |
| REQ-15 | 導入後検査 (timer active / 次回時刻 / 手動実行) を行う |
| REQ-16 | 旧 backup/restore/deploy は fail-closed (exit 2) を維持する |
| REQ-17 | drift 検査は欠落 allowlist と古い Cedar schema を検出する |
| REQ-18 | `--retain-days` の値欠落・非整数・0 以下は exit 2 で拒否する |

## Risk & tier

| risk_id | リスク | tier | 完了基準 |
| --- | --- | --- | --- |
| R-1 | 改ざんバックアップが正常と判定される | T1 | unit >= 0.90 |
| R-2 | 部分書き込み set が完全と誤認される | T1 | unit >= 0.90 |
| R-3 | 秘密情報がバックアップへ混入する | T1 | unit >= 0.90 |
| R-4 | dry-run のつもりで実書き込みが起きる | T2 | unit >= 0.80 |
| R-5 | 既定の retention で世代が削除される | T3 | unit >= 0.60 |
| R-6 | timer 導入失敗時に旧 unit が戻らない | T2 | unit >= 0.80 |
| R-7 | 旧ランチャー復活でデータ消失/OCI 転送が起きる | T1 | unit >= 0.90 |
| R-8 | --retain-days の不正値受け付けによる誤削除 | T3 | unit >= 0.60 |

tier は `web-api-l2-moderate` の定義に従う。

- T1: 保全・秘密情報・fail-closed に関わる処理 (unit 0.90)
- T2: 運用スクリプトの主要ロジック (unit 0.80)
- T3: 補助・周辺機能 (unit 0.60)

完了基準は上記カバレッジに加え、全シナリオの oracle が満たされること。

## Design technique

29119-2:2021 8.2 の TD1 (test model) / TD2 (coverage item) /
TD3 (test case) の順で導出した。

| 技法 | 29119-4 節 | 適用箇所 |
| --- | --- | --- |
| equivalence partitioning | 5.2.1 | good/corrupt/partial/legacy set |
| boundary value analysis | 5.2.3 | manifest サイズ、retention 境界 |
| decision table testing | 5.2.6 | marker x created_at x ハッシュ結果 |
| state transition testing | 5.2.8 | dry-run -> apply -> verify |
| scenario testing | 5.2.9 | 導入 -> 手動実行 -> 検証 |
| error guessing | 5.4.1 | 部分 set、欠落 allowlist、古い schema |

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

### backup-local.sh (REQ-1〜REQ-7)

```gherkin
Scenario: TC-B1 dry-run は無書き込み
  Given 存在しない --backup-dir を指定する
  When backup-local.sh を dry-run で呼ぶ
  Then exit 0 を返す
  Then 宛先ディレクトリは作成されない
```

```gherkin
Scenario: TC-B2 manifest と完了マーカー
  Given --apply で正常にバックアップが作られる
  When manifest を読む
  Then created_at ヘッダーと name/size/sha256 行が存在する
  Then COMPLETE は全工程の後に作られる
  When 途中失敗の set を検証する
  Then 部分 set は拒否される
```

```gherkin
Scenario: TC-B3 秘密情報の非包含
  Given /etc/jarvis に .env や token ファイルがある
  When バックアップが作られる
  Then それらはコピーされない
  Then 非秘密の allowlist/patterns のみがコピーされる
```

```gherkin
Scenario: TC-B4 retention の opt-in
  Given 古い世代が存在する
  When --retain-days を指定せずに実行する
  Then 古い世代は削除されない
  When --retain-days を指定する
  Then created_at を優先して古い世代のみ削除する
```

### backup-local.sh 引数検証 (REQ-18)

```gherkin
Scenario: TC-B5 --retain-days の引数拒否
  Given 引数解析は --apply の前段で行われる
  When --retain-days の値を省略する
  Then requires a value のエラーを stderr に出し exit 2 を返す
  When --retain-days に空文字または非整数を渡す
  Then must be a positive integer のエラーを stderr に出し exit 2 を返す
  When --retain-days に 0 を渡す
  Then must be greater than zero のエラーを stderr に出し exit 2 を返す
  Then いずれも後続処理へ進まずバックアップ set を作らない
```

### verify-backup.sh (REQ-8〜REQ-12)

```gherkin
Scenario: TC-V1 完全 set と改ざん set
  Given 有効なハッシュの set と改ざんされた set
  When それぞれを検証する
  Then 完全 set は exit 0、改ざん set は exit 1
```

```gherkin
Scenario: TC-V2 部分 set と legacy set
  Given created_at 付きで COMPLETE が無い set
  When 検証する
  Then 部分実行として exit 1 を返す
  Given created_at が無い legacy set
  When 検証する
  Then 互換のため exit 0 を返す
```

```gherkin
Scenario: TC-V3 対象選択とエラー
  Given 複数の set が存在する
  When --latest で検証する
  Then 名前順で最後の set のみを検証する
  When set が無いディレクトリを指定する
  Then exit 1 を返す
```

```gherkin
Scenario: TC-V4 形式検査
  Given gzip の pg dump と tar の snapshot がある
  When 検証する
  Then gzip -t と tar -tf で読めることを確認する
  Then バックアップデータは変更されない
```

### install-backup-timer.sh とガード (REQ-13〜REQ-17)

```gherkin
Scenario: TC-I1 dry-run と root 要求
  Given --apply なしで呼び出す
  Then 導入計画のみ表示し exit 0 を返す
  Given 非 root で --apply を呼び出す
  Then exit 1 を返す
```

```gherkin
Scenario: TC-I2 rollback の静的ガード
  Given 導入後のスクリプト領域
  When 静的ガードを実行する
  Then 素の exit 1 が無く fail() を使うと判定される
  Then 3本の --help がすべて成功する
```

```gherkin
Scenario: TC-I3 導入後検証
  Given --apply で導入が完了した
  When 後検証を実行する
  Then timer active と次回時刻を確認する
  Then 手動実行と読み取り専用検証を行う
  Then 失敗時は rollback が働く
```

```gherkin
Scenario: TC-L1 レガシー fail-closed
  Given 旧 backup.sh / restore.sh / deploy.sh
  When それぞれを呼び出す
  Then すべて exit 2 と移行先ヒントを返す
```

```gherkin
Scenario: TC-L2 drift 検出
  Given 配置済み allowlist が欠落している
  When drift check を実行する
  Then MISSING を検出する
  Given 古い Cedar schema を配置する
  When drift check を実行する
  Then DIFF を検出する
```

## Traceability

| Scenario | Req | ASVS | 800-53 | CC | Technique | Tier |
| --- | --- | --- | --- | --- | --- | --- |
| TC-B1 | REQ-1 | - | SP800-53r5.2/CM-7 | ATE_FUN.1 | 5.2.8 | T2 |
| TC-B2 | REQ-2 | v5.0.0-V14.2.4 | SP800-53r5.2/SI-7 | ATE_COV.2 | 5.2.1 | T1 |
| TC-B2 | REQ-3 | v5.0.0-V14.2.4 | SP800-53r5.2/CP-9 | ATE_FUN.1 | 5.2.1 | T1 |
| TC-B2 | REQ-4 | v5.0.0-V16.5.3 | SP800-53r5.2/SI-7 | ATE_FUN.1 | 5.2.6 | T1 |
| TC-B3 | REQ-6 | v5.0.0-V13.3.2 | SP800-53r5.2/AC-6 | ATE_FUN.1 | 5.4.1 | T1 |
| TC-B4 | REQ-7 | v5.0.0-V14.2.7 | SP800-53r5.2/CP-9 | ATE_FUN.1 | 5.2.3 | T3 |
| TC-B5 | REQ-18 | - | SP800-53r5.2/CM-7 | ATE_FUN.1 | 5.2.6 | T3 |
| TC-V1 | REQ-8 | v5.0.0-V14.2.4 | SP800-53r5.2/SI-7 | ATE_COV.2 | 5.2.1 | T1 |
| TC-V1 | REQ-9 | v5.0.0-V16.5.3 | SP800-53r5.2/SI-7 | ATE_FUN.1 | 5.4.1 | T1 |
| TC-V2 | REQ-4 | v5.0.0-V16.5.3 | SP800-53r5.2/SI-7 | ATE_FUN.1 | 5.2.6 | T1 |
| TC-V2 | REQ-5 | - | SP800-53r5.2/CP-9 | ATE_FUN.1 | 5.2.1 | T2 |
| TC-V3 | REQ-10 | - | SP800-53r5.2/CP-9 | ATE_FUN.1 | 5.2.1 | T2 |
| TC-V3 | REQ-11 | v5.0.0-V16.5.3 | SP800-53r5.2/CP-9 | ATE_FUN.1 | 5.4.1 | T2 |
| TC-V4 | REQ-12 | - | SP800-53r5.2/SI-7 | ATE_FUN.1 | 5.2.9 | T2 |
| TC-I1 | REQ-13 | - | SP800-53r5.2/CM-3 | ATE_FUN.1 | 5.2.8 | T2 |
| TC-I2 | REQ-14 | v5.0.0-V16.5.3 | SP800-53r5.2/CM-3 | ATE_FUN.1 | 5.4.1 | T2 |
| TC-I3 | REQ-15 | - | SP800-53r5.2/CP-9 | ATE_FUN.1 | 5.2.9 | T1 |
| TC-L1 | REQ-16 | v5.0.0-V16.5.3 | SP800-53r5.2/CM-7 | ATE_FUN.1 | 5.4.1 | T1 |
| TC-L2 | REQ-17 | - | SP800-53r5.2/CM-3 | ATE_FUN.1 | 5.4.1 | T2 |

ASVS / SP 800-53 に直接対応しない行は `-` とした (空欄ではない)。
それ以外は ASVS を v5.0.0、SP 800-53 を Release 5.2、CC を
ISO/IEC 15408-3:2022 の構成要素 ID とする。

## 既知制約 (cross-review Low)

| ID | 制約 | 影響 |
| --- | --- | --- |
| L-1 | manifest 記載ファイルのサイズ/SHA-256 と形式のみ検証 | 未記載ファイルは検出しない |
| L-2 | manifest と COMPLETE に署名が無い | 破損検出はできるが改ざん耐性は無い |
| L-3 | 秘密走査は gzip/tar 内部を展開しない | allowlist が主制御 |

L-1: 検証対象は manifest に載ったファイルに限られ、バックアップ
セットに付け加わった未記載ファイルは検出されない。
L-2: 完全性 (integrity) の確認はできるが、真正性 (authenticity) は
保証しない。署名は本スライスの対象外 (P1-6 の判断待ち)。
L-3: [test-verify-backup.sh](../ci/scripts/test-verify-backup.sh) の
ダミー sentinel 走査はアーカイブ内部まで展開しないため、
秘密情報の非包含は backup-local.sh の明示 allowlist に依存する。

## 既存テストとの対応

回帰テストは [test-verify-backup.sh](../ci/scripts/test-verify-backup.sh)
と [test-legacy-guards.sh](../ci/scripts/test-legacy-guards.sh) にあり、
`make test-verify-backup` (23 checks) と `make test-backup-args` (6 checks)、
合計 29 checks で実行する。
実行結果は `evidence/` の test-run JSON に記録する。

TC-B3 (秘密情報の非包含) と TC-V4 (形式検査) は task-24 の
回帰テスト追加でカバー済み。TC-B4 (retention) と TC-I3 (導入後検証) は
回帰テストで直接カバーされていない。実機 PG/Qdrant/systemd を伴う
検証は対象外とし、未検証として明示する。
