# Architecture Decision Records

JARVIS の設計決定 Records の索引です。
内容の根拠は各本文を参照してください（索引は候補選択の材料であり保証ではありません）。

## ADR 一覧

- [0001-unified-architecture.md](0001-unified-architecture.md)
  — 統合アーキテクチャ。OpenCode V2 を Agent Runtime と定義。**accepted**（ infra 部分は ADR-0004 で置換）。
- [0002-runtime-agnostic.md](0002-runtime-agnostic.md)
  — ACP/MCP 契約によるランタイム非依存化。**accepted**。
- [0003-native-web-fetch.md](0003-native-web-fetch.md)
  — `web.fetch` のネイティブ実装と SSRF 対策。**Accepted**。
- [0004-local-only-no-oci.md](0004-local-only-no-oci.md)
  — OCI 非使用・ローカル Kubuntu/systemd 運用。**Accepted**。
- [0005-gpui-desktop-adoption.md](0005-gpui-desktop-adoption.md)
  — GPUI 承認コンソールの採用。**accepted**。

## テンプレート

- [template.md](../docs/adr/template.md) — 新規 ADR の雛形（OKF frontmatter + `##` 開始）。

## 更新履歴

変更を加えたコミットでは [log.md](log.md) に日付付きで追記する。
