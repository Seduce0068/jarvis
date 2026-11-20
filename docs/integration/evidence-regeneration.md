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

**真正性の限界（I69-D (a)）**: `check-evidence.sh` は仕様ハッシュ、
notes / index の件数整合、Scenario 数の下限など**内部整合**のみを検証する。
`results.passed` は手入力であり、notes と index を同じ値に揃えれば
実際のテスト結果と無関係な数値でもゲートを通過する。生成器はこの
「手入力リスク」を補助的に減らすもので、ゲートの真正性保証ではない。
実測コマンドと JSON の対応は「実測と JSON の対応（検算手順）」を参照。

## 再生成手順（7ステップ）

### 1. 仕様のハッシュを取得する

```bash
sha256sum specs/test-<name>.md
```

出力は `<64hex>  specs/test-<name>.md`。エビデンスには
`sha256:<64hex>` の形式で記録する（`sha256:` プレフィックスが必須）。

### 2. 対応テストを再実行する（必須）

仕様変更に対応するテストを再実行し、passed / failed / skipped を控える。

- `test-policy-approval.md`: `cd rust && cargo test -p jarvis-policy --lib --locked`
- `test-mcp-gateway.md`: `cd rust && cargo test -p jarvis-mcp-gw --lib --locked`
- `test-context.md`: `cd rust && cargo test -p jarvis-context --lib --locked`
- `test-event.md`: `cd rust && cargo test -p jarvis-event --lib --locked`
- `test-ui.md`: `cd ui && cargo test --locked`
- `test-backup.md`: `make test-verify-backup` と `make test-backup-args`（`make safety-test` は集計対象外）

Cargo 系のターゲットは `ci/scripts/generate-evidence.sh` でテスト数を機械導出
できる。導出値はあくまで「一覧上の件数」であり、実行結果の成否は必ず
上記コマンドの実測で確認する（後述の検算手順）。

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

- `results.passed` / `results.failed`: 手順2の実測値。Cargo 系は
  generator の機械導出値と照合し、差分がある場合は実行コマンドの範囲
  （`--lib` の有無等）を見直す。厳密な整数型であり、`failed` が 0 でない
  場合はゲートが拒否する。
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
  （mcp-gw の `skipped=1` は `notes` の `1 ignored` と `passed=126` で
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
| 契約 step | strict モード。契約マーカー以降の step に現れるキーワードが証跡 `notes` にもあること（baseline 例外なし）。未同期なら `SCENARIO_STEP_UNCOVERED` |
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

### strict モード（Round 61 baseline は廃止）

Round 61 で 4 仕様へ追記された契約 step のキーワード（`BareJson` / `415` /
`413` / `400` / no-leak / `error_kind=invalid_input` / `field=body`）は、
2026-11-20 に対応 4 証跡の `notes` へ実測済みとして同期済み。
**`BASELINE_CONTRACT_STEPS` による grandfather は削除済み**で、現在は
strict モードである。仕様の契約 step に現れるキーワードは、重複追記を
含め当該証跡の `notes` に同じキーワードが無ければ
`SCENARIO_STEP_UNCOVERED` で fail-closed に拒否される。

`ci/scripts/test-legacy-guards.sh` の tamper 回帰は、コピーした仕様へ契約
step を追記し、かつコピーした証跡の `notes` から当該キーワードを外すことで、
notes 未同期を検出する（実リポジトリの仕様・証跡は変更しない）。

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
cd rust && cargo test -p jarvis-core --features http --lib --locked
cd rust && cargo test --workspace --locked
```

ハッシュだけの書き換えは禁止する。

## 証跡 results の機械導出（I69-D (a)）

`ci/scripts/generate-evidence.sh` は `cargo test --locked -- --list` と
`--list --ignored` の件数から `results` を機械導出する。対象は
`policy` / `mcp` / `context` / `event` / `ui` / `core` の 6 ターゲット。

```bash
# Kubuntu / jarvis リポジトリルートで実行
echo "== 機械導出 (context) =="
bash ci/scripts/generate-evidence.sh context
```

標準出力には `results` 断片と、それと整合する `notes` の件数表記のみを出す。
テスト名・環境変数・秘密情報・ホスト固有パスは読まない。失敗時は
値を持たないコード（`CargoListFailed` 等）のみを stderr に出す。

### 自動書き込みを行わない理由

意図的に**読み取り専用**としている。次に理由を記録する。

1. `--list` はテストを列挙するだけで実行はしない。したがって
   `passed` の「実行に成功した」という意味は実測実行でのみ確認できる。
   generator の値をそのまま書き込むと、未実行を「成功」と偽る危険がある。
2. `check-evidence.sh` はファイル名のハッシュ接頭詞、仕様ハッシュ、
   notes / index の件数、Scenario 数、契約 step キーワードも検証する。
   `results` だけを自動書き込みすると内部整合が崩れ、他項目まで
   自動同期させれば「ゲートを通るための改ざん」と外見上区別できない。
3. 件数は実行範囲（`--lib` か workspace か、ignored の扱い）で変わる。
   ターゲット名と実測コマンドの対応を運用側で明示する必要がある。

このため generator は `results` の**元になる数値**を与えるだけとし、
既存 6 証跡や `check-evidence.sh` を壊さない（ファイルを一切書かない）。
`make check-evidence` の動作は従来どおりである。

## 実測と JSON の対応（検算手順）

1. generator で機械導出値を得る（上記コマンド）。
2. 手順2の実測コマンドを実行し、`test result: ok. N passed;
   0 failed; M ignored` を確認する。ここでの成否が真正性の根拠。
3. 導出値と実測が一致したら `results` と `notes` へ反映し、
   `--check` で既存証跡との差分を確認する。

```bash
# Kubuntu / jarvis リポジトリルートで実行
echo "== 1. 機械導出 =="
bash ci/scripts/generate-evidence.sh context
echo "== 2. 実測実行 =="
(cd rust && cargo test -p jarvis-context --lib --locked)
echo "== 3. 既存証跡との照合（値は出力しない） =="
bash ci/scripts/generate-evidence.sh context --check \
  evidence/test-run-20261120-f1e67b2a5e2c.json
```

`CHECK: MISMATCH` は証跡側が古い（例: テスト追加で件数が増えた）ことを示す。
この場合だけ手順3〜6で証跡を再生成し、再度 `--check` で `MATCH` を確認する。

| generator target | 実測コマンド | 対応する証跡 |
| --- | --- | --- |
| `policy` | `cd rust && cargo test -p jarvis-policy --lib --locked` | policy-approval |
| `mcp` | `cd rust && cargo test -p jarvis-mcp-gw --lib --locked` | mcp-gateway |
| `context` | `cd rust && cargo test -p jarvis-context --lib --locked` | context |
| `event` | `cd rust && cargo test -p jarvis-event --lib --locked` | event |
| `ui` | `cd ui && cargo test --locked` | ui |
| `core` | `cd rust && cargo test -p jarvis-core --features http --lib --locked` | （予備） |

| JSON フィールド | 導出元 | 意味 |
| --- | --- | --- |
| `results.passed` | `--list` の総数 − ignored | 列挙された実行対象数（成功は実測で確認） |
| `results.failed` | 実測実行の失敗数 | ゲートは 0 を要求 |
| `results.skipped` | `--list --ignored` の件数 | `notes` の `N ignored` と一致させる |

`test-backup.md` は `make` ベースのシェル回帰であり `--list` に対応しない。
手順2の実測出力と `results` を目視で照合する（generator 対象外）。
Scenario 数・契約 step キーワード・index の件数は generator の対象外のため、
従来どおり手順4〜6で同期する。

## verify サマリの件数単位（I81-D (c)）

`infra/scripts/verify-backup.sh` の `== result: ok=<n> fail=<n> ==` は
**ペイロード／呼び出し単位の判定数**である。カウンタ単一化後は 1 ペイロード
に対する完全性チェック、フォーマット判定、ロール判定などがそれぞれ
`ok` / `fail` を 1 件ずつ加算する。旧来の「イベント数」（ペイロードや
バックアップセットの個数）とは一致しないので、`ok + fail` を
イベント件数として読まないこと。

`evidence/*.json` の `results.passed` はテスト関数の列挙数であり、
verify サマリとは単位が異なる。両者を照合する場合は
「テストケース」「ペイロード判定呼び出し」「イベント」の
どの単位で数えたかを明記する。

## secret_scan.py の CWD 非依存性（I84-D / I81-B）

`ci/scripts/secret_scan.py` は呼び出し元の CWD に依存しない。**一次防御は
`repo_root()` がスクリプト自身のパスから repo root を導出し、以後の
`git ls-files` を `git -C <root>` で実行すること**である。サブディレクトリや
別 git repo（franken CWD）から起動しても走査集合が縮まないのは、この
anchoring による。`ci/scripts/test-legacy-guards.sh` の対応回帰は、
その性質を各 CWD から実行して固定する。

`tracked_files()` の件数を `index_count()`（`git -C <root> ls-files --cached -z`）
と照合するチェックは**補助的な回帰ガード**に限られる。
両者は現行実装では同じ cached index を返すため、この照合は独立した
CWD 検出器ではない。`tracked_files()` が将来 CWD 依存／フィルタ付きの
列挙へ変更された場合に fail-closed で検出するためのものであり、
CWD 非依存を担保する一次防御ではない（混同しないこと）。

## workflow pin 照合ゲート（I87-A）

### 目的

`.github/workflows/*.yml` の `uses: owner/repo/path@vN` は可変タグである。
共通指示 §5 は参照を `v1` タグへピン留めする方針のため、`actions/checkout`
のようにフル SHA 固定へ置き換えることはしない。代わりに直近コメントへ
`refs/tags/v1 -> <40hex>` の形式で解決 SHA を記録し、
[check-workflow-pins.sh](../../ci/scripts/check-workflow-pins.sh) が
GitHub API（読み取り専用）で現行 SHA を解決して照合する。タグが付け替え
られたのに記録が更新されていない場合は `SHA_MISMATCH` で fail-closed に
拒否する。フル SHA 固定済みの `uses:`（例: `actions/checkout@11d596...`）は
immutable なので対象外とし、`@vN` 形式のタグ参照のみを検査する。

### 実行

```bash
# Kubuntu / jarvis リポジトリルートで実行
echo "== workflow pin 照合 =="
make check-workflow-pins
echo "== ci-local（check-workflow-pins とその回帰が含まれる） =="
make ci-local
```

`make ci-local` のほか、`.github/workflows/ci.yml` の `extra_gates` と
`.github/workflows/review-gate.yml` の手動ゲート（`make ci-local`）でも
実行される。

### 自動回帰（I89-B / `test-workflow-pins.sh`）

`check-workflow-pins.sh` の各分岐は手動確認に依存していたため、
[test-workflow-pins.sh](../../ci/scripts/test-workflow-pins.sh) で自動回帰する。
一時ディレクトリへゲート本体と mini workflow をコピーし、`curl` を PATH 上の
疑似レスポンス・スタブで隠すため、実ワークフロー・実 API・ネットワークには
触れない（既存の証跡・設定ファイルも書き換えない）。

```bash
# Kubuntu / jarvis リポジトリルートで実行
echo "== ピン照合ゲートの自動回帰 =="
bash ci/scripts/test-workflow-pins.sh
echo "== Makefile 経由（単体 / safety / ci-local） =="
make test-workflow-pins
make safety-test
make ci-local
```

網羅する分岐は次のとおり。いずれも終了コードと固定語彙の両方を検証する。

| 分岐 | 疑似条件 | 期待 |
| --- | --- | --- |
| lightweight tag | 解決 SHA = 記録 SHA（`type=commit`） | `OK` / 終了 0 |
| annotated tag follow | ref が `type=tag` の tag オブジェクトを指す | API を 2 回呼び出し `OK` / 終了 0 |
| 不整合 SHA | 記録値と異なる commit SHA を返す | `SHA_MISMATCH` / 終了 1 |
| 記録欠落 | 直前コメントに `refs/tags/v1 -> <40hex>` が無い | `PIN_RECORD_MISSING` / 終了 1 |
| 到達不可 rc 6 | 疑似 curl が 6 を返す | `UNVERIFIED` / 終了 0 |
| 到達不可 rc 28 | 疑似 curl が 28 を返す | `UNVERIFIED` / 終了 0 |
| `--offline` | 引数指定と `WORKFLOW_PINS_OFFLINE=1` | curl 未呼び出し・`UNVERIFIED` / 終了 0 |
| HTTP エラー | 疑似 curl が 22 を返す | `API_RESOLVE_FAILED` / 終了 1 |
| JSON 不正 | 不正なボディ | `API_RESOLVE_FAILED` / 終了 1 |
| その他 | workflow 無し・不正 uses・引数過多 | `NO_WORKFLOW_FILES` / `PIN_RECORD_MALFORMED` / `BAD_USAGE` |

SHA は合成値（例: `aaaa...`）のみで、トークンその他の秘密情報は使わない。
`make safety-test` と `make ci-local` に1行ずつ配線済み。
`.github/workflows/ci.yml` の `extra_gates` は変更していない: 既に
`make check-workflow-pins` と `make safety-test` が列挙されているため、
回帰は `safety-test` 経由で CI でも実行される。

### オフライン挙動

`WORKFLOW_PINS_OFFLINE=1` または `--offline` を指定すると API 照合を
スキップし、各エントリを `UNVERIFIED` として警告して終了コード 0 を返す。
ネットワーク到達不可（curl の DNS／接続／タイムアウト失敗、rc 6/7/28/35）も
同様に `UNVERIFIED` へ格下げする。これはローカル作業を止めないための
opt-out であり、CI は API 到達性がある前提で運用する。

```bash
# オフライン環境での実行
WORKFLOW_PINS_OFFLINE=1 bash ci/scripts/check-workflow-pins.sh
bash ci/scripts/check-workflow-pins.sh --offline
```

到達はできるが HTTP エラー（認証・レート制限・404 等）や JSON 不正の場合は
`API_RESOLVE_FAILED` で fail-closed とする。「到達できない」ことと
「API が失敗した」ことは誤って混同しないこと。

### 出力と限界

不一致・API 失敗・記録欠落は固定語彙（`SHA_MISMATCH` /
`API_RESOLVE_FAILED` / `PIN_RECORD_MISSING` / `PIN_RECORD_MALFORMED` /
`NO_WORKFLOW_FILES` / `WORKFLOW_READ_FAILED` / `BAD_USAGE`）で出力する。
SHA 値自体は診断出力に出してよいが、トークンその他の秘密情報は出力・保存
しない。`GITHUB_TOKEN` / `GH_TOKEN` が環境に存在する場合は Authorization
ヘッダにのみ使用する。

**限界**: タグの付け替えと記録 SHA の更新を同じコミットで行うと整合するため
ゲートは通過する。記録は監査用のピンであり、タグ更新の承認を自動化する
ものではない（共通指示 §5 の「互換更新はタグの付け替え」運用を前提とする）。

## 関連

- [specs/index.md](../../specs/index.md): テスト仕様の索引
- [evidence/index.md](../../evidence/index.md): 実行エビデンスの索引
- [check-evidence.sh](../../ci/scripts/check-evidence.sh): 検証ゲート本体
- [local-operations-runbook.md](local-operations-runbook.md): ローカル運用全般
