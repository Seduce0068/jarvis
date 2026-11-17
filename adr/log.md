# ADR Change Log

ADR ディレクトリに変更を加えたコミットの記録。日付見出しは新しい順。

## 2026-11-18

- [`0002-runtime-agnostic.md`](0002-runtime-agnostic.md): PII の保存時扱いを
  「記録（未決定）」節として追記（`eafa5f7`）。PII フィルタは `build_context`
  （LLM 送信前）にのみ適用し、`store_memory` は `content` / `metadata` を
  生のまま保存する現行挙動を記録。新たな設計決定ではない。
- [`0001-unified-architecture.md`](0001-unified-architecture.md): 比較テーブルへ
  現行方針の注記を追加（`960baf5`）。IaC は ADR-0004 で非適用、
  秘密情報は `bws run`、バックアップはローカル保存、
  Audit Log は PostgreSQL へそれぞれ置換。

## 2026-11-17

- `0003-native-web-fetch.md` / `0004-local-only-no-oci.md`: OKF frontmatter を
  付与（`b9fe815`）。
- `0002-runtime-agnostic.md`: `status` を `proposed` から `accepted` へ変更（実装・検証済みのため）。
  Status rationale 節を追加。（`39198c3`）
- `0005-gpui-desktop-adoption.md`: 新規追加（GPUI 承認コンソールの採用、実機検証済み）。
  accepted 化（`e9d0b37`）は 2026-11-16 節を参照。
- `0001-unified-architecture.md`: 2026-11-17 時点の解消状況を注記（checkbox と決定文は変更せず）。
  （`b0d7906`）
- `index.md` / `log.md`: 共通指示の索引・更新履歴ルールに従い追加。（`32e147c`）

## 2026-11-16

- [`0005-gpui-desktop-adoption.md`](0005-gpui-desktop-adoption.md): `status` を
  `proposed` から `accepted` へ変更（GPUI 採用は明示的な決定のため、`e9d0b37`）。

## 2026-11-15

- `0004-local-only-no-oci.md`: 新規追加（OCI 非使用・ローカル運用の決定）。
- `0001-unified-architecture.md`: ADR-0004 による infra 部分の置換を注記。

## 2026-11-09

- `0003-native-web-fetch.md`: 新規追加（`web.fetch` ネイティブ実装）。
- `0002-runtime-agnostic.md`: 新規追加（ランタイム非依存化）。

## 2026-10-09

- `0001-unified-architecture.md`: 初版（統合アーキテクチャ）。
