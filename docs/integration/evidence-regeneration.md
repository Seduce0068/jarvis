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
- `test-backup.md`: `make test-verify-backup` と `make safety-test`

### 3. evidence JSON の spec_hash を更新する

- トップレベルの `spec_hash`
- `test_basis_ref.spec_hash`（**両方必須**）

両方を新しい `sha256:<64hex>` に揃える。片方だけの更新は
`NESTED_HASH_MISMATCH` としてゲートに拒否される。

### 4. results / timestamp / notes を実測へ更新する

- `results.passed` / `results.failed`: 手順2の実測値。厳密な整数型であり、
  `failed` が 0 でない場合はゲートが拒否する。
- `timestamp`: RFC 3339（タイムゾーン付き、例 `2026-11-18T06:20:42+09:00`）。
- `notes`: 検証した内容と未測定項目（カバレッジ等）を簡潔に記載する。
  秘密情報や実機固有の値は書かない。
- `environment.runtime`: 実行コマンドが変わった場合のみ更新する。

### 5. ファイル名の接頭詞を変更する

`evidence/test-run-YYYYMMDD-<hash先頭12文字>.json` に従い、日付を実行日、
接頭詞を新しい `sha256:` 値の先頭12文字へ変更する（旧ファイルは
リネームして残さない）。

注: `check-evidence.sh` は現状ファイル名接頭詞を直接検証しないが、
成果物の追跡可能性を保つためこの規約に従う。

### 6. 両 index を更新する

- `specs/index.md`: 仕様の説明・要件の変更を反映する（仕様のファイル名自体は
  不変）。
- `evidence/index.md`: ファイル名（参照定義 `[run-xxx]: test-run-*.json`
  を含む）と件数（passed / failed）を更新する。
- 両 index は成果物を網羅し、リンク切れがないこと。ゲートが双方向で検証する。

### 7. `make check-evidence` で検証する

```bash
make check-evidence
```

`ok=<n> errors=0` なら成功。失敗時はエラーコードのみが出力され、
問題の値は表示されない（例: `SPEC_HASH_MISMATCH`,
`NESTED_HASH_MISMATCH`, `FAILED_TESTS`, `*_NOT_IN_INDEX`,
`INDEX_BROKEN_LINK`）。

## ゲートが検証する項目

| 検証対象 | 内容 |
| --- | --- |
| `spec_hash` | トップレベルが仕様ファイルの sha256 と一致 |
| `test_basis_ref.spec_hash` | トップレベルと一致（nested hash） |
| `results` | `failed=0` / `passed>0` / 厳密な整数型 |
| index | 両 index が全成果物をリンクし、リンク切れなし |
| `timestamp` / `signature` | RFC 3339 の実在日時 / `self-attested` |
| 仕様構造 | 必須4セクション、7列 Traceability、Document map、`tailored` |

## 関連

- [specs/index.md](../../specs/index.md): テスト仕様の索引
- [evidence/index.md](../../evidence/index.md): 実行エビデンスの索引
- [check-evidence.sh](../../ci/scripts/check-evidence.sh): 検証ゲート本体
- [local-operations-runbook.md](local-operations-runbook.md): ローカル運用全般
