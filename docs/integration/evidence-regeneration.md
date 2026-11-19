---
type: Playbook
title: "テストエビデンス再生成 Runbook"
description: "テスト仕様変更時に evidence JSON を実測へ同期する手順。ハッシュのみの書き換えは禁止。"
tags:
  - jarvis
  - testing
  - evidence
  - runbook
generated:
  by: "DeepSeek Harness (lead agent)"
  at: "2026-11-18T06:20:42+09:00"
---

## 概要

`specs/test-*.md` を変更した場合は、対応する `evidence/test-run-*.json` を
実測に基づいて再生成する。[check-evidence.sh](../../ci/scripts/check-evidence.sh)
が仕様ハッシュ・結果・索引の整合を検証する（`make check-evidence`。
`make ci-local` にも含まれる）。

**禁止事項**: ハッシュだけを書き換えてテストを再実行しないこと。
ハッシュは仕様変更の結果であってテスト結果の証拠ではない。必ず手順2で
対応テストを再実行し、その実測値を記録する。

## 再生成手順（7ステップ）

### 1. 仕様のハッシュを取得する

```bash
sha256sum specs/test-<name>.md
```

出力は `<64hex>  specs/test-<name>.md`。エビデンスには
`sha256:<64hex>` の形式で記録する（`sha256:` プレフィックスが必須）。

### 2. 対応テストを再実行する（必須）

仕様変更に対応するテストを再実行し、passed / failed / skipped を控える。

- `test-policy-approval.md`: `cd rust && cargo test -p jarvis-policy --lib`
- `test-mcp-gateway.md`: `cd rust && cargo test -p jarvis-mcp-gw --lib`
- `test-context.md`: `cd rust && cargo test -p jarvis-context --lib`
- `test-event.md`: `cd rust && cargo test -p jarvis-event --lib`
- `test-ui.md`: `cd ui && cargo test`
- `test-backup.md`: `make test-verify-backup` と `make test-backup-args`（`make safety-test` は集計対象外）

### 3. evidence JSON の spec_hash を更新する

- トップレベルの `spec_hash`
- `test_basis_ref.spec_hash`（**両方必須**）

両方を新しい `sha256:<64hex>` に揃える。片方だけの更新は
`NESTED_HASH_MISMATCH` としてゲートに拒否される。

`test_basis_ref.requirements` は spec の `REQ-\d+` 定義集合と
Traceability の Req 列集合の両方と一致させる。要件の追加・削除や
未定義 Req の参照は `REQ_SET_MISMATCH` で拒否される。同一 spec を
参照する証跡が複数ある場合は**すべて**の `requirements` が照合され、
片方だけを改ざんした場合も拒否される。

### 4. results / timestamp / notes を実測へ更新する

- `results.passed` / `results.failed`: 手順2の実測値。厳密な整数型であり、
  `failed` が 0 でない場合はゲートが拒否する。
- `timestamp`: RFC 3339（タイムゾーン付き、例 `2026-11-18T06:20:42+09:00`）。
- `notes`: 検証した内容と未測定項目（カバレッジ等）を簡潔に記載する。
  秘密情報や実機固有の値は書かない。件数表記（`N passed` 等）は `results` と
  一致させる。スキップがある場合は `N ignored` のように数値付きで明記し、
  `N` を `results.skipped` と一致させる。`no tests were ignored` のような
  文字列だけでは `SKIPPED_TESTS` で拒否される。
- 仕様へ契約 step（`BareJson` / `415` / `413` / `400` / no-leak /
  `error_kind=invalid_input` / `field=body`）を追加・変更した場合は、
  対応テストの実測に基づき同じキーワードを `notes` にも明記する。
  仕様だけを更新してキーワードが無い場合は `SCENARIO_STEP_UNCOVERED` で
  拒否される（詳細は「契約 step のカバレッジ」を参照）。
- `results.skipped`: 正の値にする場合は `notes` へ数値付きの `N ignored` が必要。
  ただしスキップは実行済みに数えず、Scenario カバレッジの分子には入らない
  （mcp-gw の `skipped=1` は `notes` の `1 ignored` と `passed=124` で
  カバレッジを満たす）。
- `document_type`: 設定する場合は `test-execution-log` / `test-result` /
  `test-summary-report` のいずれか。
- `environment.runtime`: 実行コマンドが変わった場合のみ更新する。

### 5. ファイル名の接頭詞を変更する

`evidence/test-run-YYYYMMDD-<hash先頭12文字>.json` に従い、日付を実行日、
接頭詞を新しい `sha256:` 値の先頭12文字へ変更する（旧ファイルは
リネームして残さない）。

注: `check-evidence.sh` はファイル名末尾のハッシュ接頭詞（12文字）を
`spec_hash` と照合し、不一致なら `HASH_FILENAME_MISMATCH` で拒否する。

### 6. 両 index を更新する

- `specs/index.md`: 仕様の説明・要件の変更を反映する（仕様のファイル名自体は
  不変）。
- `evidence/index.md`: ファイル名（参照定義 `[run-xxx]: test-run-*.json`
  を含む）と件数（passed / failed）を更新する。各エントリには最低限
  `N passed` と `N failed` を明記すること。参照スタイル `[text][run-xxx]` と
  インライン `[text](test-run-*.json)` は等価に解決される。
- 両 index は成果物を網羅し、リンク切れがないこと。ゲートが双方向で検証する。
  件数表記を削除したエントリは `INDEX_RESULT_MISSING` で拒否される。

### 7. `make check-evidence` で検証する

```bash
make check-evidence
```

`ok=<n> errors=0` なら成功。失敗時はエラーコードのみが出力され、
問題の値は表示されない。現行実装が発行する主なコードは次のとおり。

| 区分 | エラーコード |
| --- | --- |
| JSON 形式 | `INVALID_JSON`, `MISSING:*` |
| ハッシュ | `BAD_HASH_FORMAT`, `HASH_FILENAME_MISMATCH`, `SPEC_HASH_MISMATCH`, `NESTED_HASH_MISMATCH` |
| 仕様参照 | `NO_SPEC_PATH`, `SPEC_PATH_NOT_FOUND`, `SPEC_UNREADABLE` |
| 時刻・署名・種別 | `BAD_TIMESTAMP`, `BAD_SIGNATURE`, `BAD_DOCUMENT_TYPE` |
| 結果 | `FAILED_TESTS`, `NO_PASSING_TESTS`, `BAD_RESULTS_TYPE` |
| notes | `BAD_NOTES_TYPE`, `SKIPPED_TESTS`, `NOTES_*_MISMATCH`, `NOTES_COUNT_MISMATCH` |
| 要件 | `REQ_SET_MISMATCH` |
| シナリオ | `SCENARIO_COUNT_MISMATCH` |
| 契約 step | `SCENARIO_STEP_UNCOVERED` |
| 仕様構造 | `MISSING_EVIDENCE`, `MISSING_SECTION`, `MISSING_TRACEABILITY`, `MISSING_INFO_ITEM`, `MISSING_CONFORMANCE` |
| 索引 | `INDEX_NOT_FOUND`, `*_NOT_IN_INDEX`, `INDEX_BROKEN_LINK` |
| 索引の件数 | `INDEX_RESULT_MISSING`, `INDEX_RESULT_MISMATCH` |

## ゲートが検証する項目

| 検証対象 | 内容 |
| --- | --- |
| `spec_hash` | トップレベルが仕様ファイルの sha256 と一致（不一致は `SPEC_HASH_MISMATCH`、形式不正は `BAD_HASH_FORMAT`） |
| ファイル名末尾 | ハッシュ接頭詞（12文字）が `spec_hash` と一致（不一致は `HASH_FILENAME_MISMATCH`） |
| `test_basis_ref.spec_path` | 実在する仕様ファイル（未指定は `NO_SPEC_PATH`、欠落は `SPEC_PATH_NOT_FOUND`） |
| `test_basis_ref.spec_hash` | トップレベルと一致（nested hash、不一致は `NESTED_HASH_MISMATCH`） |
| `requirements` | 同一 spec を参照する**全証跡**の要件集合＝spec の `REQ-\d+` 定義集合＝Traceability の Req 列集合（不一致は `REQ_SET_MISMATCH`。エラーには対象証跡のパスを示すが要件値は出力しない） |
| `results` | `failed=0` / `passed>0` / 厳密な非負整数型（型不正は `BAD_RESULTS_TYPE`） |
| `results.skipped` | 正の値は `notes` に数値付き `N ignored`（`N` = `results.skipped`）が必要（`SKIPPED_TESTS`） |
| `notes` | 文字列型（`BAD_NOTES_TYPE`）。`passed` / `failed` / `ignored` / `skipped` と見出し件数が `results` と一致（`NOTES_*_MISMATCH`, `NOTES_COUNT_MISMATCH`） |
| `Scenario` | 仕様の `Scenario:` が1以上で、実行合計（passed + failed のみ、skipped は除外）がその数以上（`SCENARIO_COUNT_MISMATCH`）。`ignored` 明記済みのスキップでもカバレッジは満たせない |
| 契約 step | 契約マーカー以降の step に現れるキーワードが証跡 `notes` にもあること。baseline 超過分が未同期なら `SCENARIO_STEP_UNCOVERED` |
| index | 両 index が全成果物をリンクし、リンク切れなし |
| index のエントリ | 参照定義・インラインリンクを等価に解決し、`N passed` / `N failed` が必須（欠落は `INDEX_RESULT_MISSING`） |
| index の件数 | 見出し件数は参照 JSON の `results` と一致（`INDEX_RESULT_MISMATCH`） |
| `timestamp` / `signature` | RFC 3339 の実在日時（`BAD_TIMESTAMP`）/ `self-attested`（`BAD_SIGNATURE`） |
| `document_type` | 設定時は `test-execution-log` / `test-result` / `test-summary-report` のいずれか（`BAD_DOCUMENT_TYPE`） |
| 仕様構造 | 必須4セクション、7列 Traceability、Document map、6情報項目、`tailored`（`MISSING_SECTION` 等） |

## 契約 step のカバレッジ（I65-C）

### 採用方針

既存シナリオへ契約 step を追記しても `Scenario:` 行数は増えないため、
`executed >= scenario_count` の下限ゲートだけでは「仕様のみの改変」を
検出できない。本リポジトリでは、**仕様内の契約 step キーワードを検出し、
対応証跡の `notes` に当該キーワードが無ければ
`SCENARIO_STEP_UNCOVERED` で fail-closed にする**方針を採用する。
「新規 step 追加時に Scenario を新設して件数ゲートを上げる運用」は採らない。

検出対象は gherkin ブロック内の契約マーカー（`# I61-A: BareJson ...`）以降の
step に現れる `BareJson` / `415` / `413` / `400` / no-leak /
`error_kind=invalid_input` / `field=body`。契約グループ外で status に言及する
通常 step（例: `Then 400 と ... が返る`）は誤検知を避けるため対象外とする。
`Counter` で出現回数を数えるため、既存 step の重複追記も新規 step と
みなされ、notes 未同期なら拒否される。

### Round 61 の legacy baseline（解消待ち）

Round 61 で 4 仕様へ追記された契約 step は、対応する 4 証跡の `notes` に
キーワードが同期されていない（2026-11-20 時点で確認）。既存 6 証跡を通す
ため、これら既知の出現回数だけを `check-evidence.sh` の
`BASELINE_CONTRACT_STEPS` で grandfather している。

| 仕様 | 対応証跡の `notes` の状態 |
| --- | --- |
| `test-context.md` | `BareJson` / `415` / `413` / `400` / no-leak / 固定語彙のいずれも未記載 |
| `test-event.md` | 同上（「非漏洩検証」はあるが no-leak 契約キーワードとしては未同期） |
| `test-policy-approval.md` | 同上 |
| `test-mcp-gateway.md` | 同上 |

baseline を超える新規・重複 step は notes 同期が必須となり、未同期なら
`SCENARIO_STEP_UNCOVERED` で拒否される。`ci/scripts/test-legacy-guards.sh`
に、契約 step を重複追記したコピーが拒否される tamper 回帰を追加済み。

**残作業（本担当の担当外）**: 上記 4 証跡の `notes` へ実測済みキーワードを
追記したうえで `BASELINE_CONTRACT_STEPS` を削除し、strict モードへ移行する。
証跡 JSON と仕様ファイルは本担当の編集範囲外のため未変更である。

## JSON リジェクションの status 回帰（I61-A）

`jarvis_core::BareJson` は axum の `JsonRejection` を種別別の空本文 status へ
変換する。型不正・構文エラーは従来どおり 400 のままである。

| 種別 | status | クライアント側の対処 |
| --- | --- | --- |
| `MissingJsonContentType` | 415 | `Content-Type: application/json` を付ける |
| `BytesRejection`（既定 2 MiB 超過） | 413 | 本文を小さくする |
| `JsonDataError` / `JsonSyntaxError` | 400 | JSON の型・構文を直す |

回帰は `jarvis-core` の `bare_json_missing_content_type_is_415` /
`bare_json_oversized_body_is_413` /
`bare_json_type_and_syntax_errors_stay_400` で検証する。拒否ログは固定語彙
（`invalid_input` / `body`）の DEBUG レコードのみで、本文や axum の rejection
テキストは出力しない。context の I57-A ログ回帰はサブスクライバを DEBUG に
設定したうえで、同じ固定語彙が残ることを検証する。

仕様・証跡を更新した場合は上記の再生成手順に従い、次の実測値を証跡へ記録する。

```bash
cd rust && cargo test -p jarvis-core --features http --lib
cd rust && cargo test --workspace
```

ハッシュだけの書き換えは禁止する。

## 関連

- [specs/index.md](../../specs/index.md): テスト仕様の索引
- [evidence/index.md](../../evidence/index.md): 実行エビデンスの索引
- [check-evidence.sh](../../ci/scripts/check-evidence.sh): 検証ゲート本体
- [local-operations-runbook.md](local-operations-runbook.md): ローカル運用全般
