---
type: ADR
title: "ADR-0003: web.fetch ネイティブ実装"
description: "web.fetch の SSRF 対策付きネイティブ実装決定"
tags:
  - adr
  - jarvis
  - web-fetch
generated:
  by: "DeepSeek Harness (lead agent)"
  at: "2026-11-17T23:33:40+09:00"
---

- Status: Accepted
- Date: 2026-11-09

## 文脈

ADR-0002で「読み取り系ツールはゲートウェイ内のRust実装で提供する」方針を採用。
`web.fetch` はallowlistとCasbinポリシーに登録済みだが、実装が無くupstream MCPサーバ登録待ちだった。

## 決定

`web.fetch` を `jarvis-mcp-gw` のネイティブハンドラとして実装する。

- 対象: HTTP/HTTPS GET のみ。URL内の認証情報は拒否。
- SSRF対策: ホストをリテラルIP/DNS解決した上で**公開IPのみ**許可
  （loopback/private/link-local/ULA/multicast/0.0.0.0/100.64/10 等を拒否）。
- リダイレクト: 自動追従せず手動で最大3回。**各ホップで公開IPを再検証**。
- サイズ: 既定256KiB・上限1MiB。content-typeはテキスト系のみ。
- タイムアウト: allowlist値を1〜15秒にclamp。

## セキュリティ上の注意

- **配備済みの `/etc/jarvis/mcp_allowlist.toml` では `web.fetch` は
  `requires_approval = true` で登録済み**（`check-deploy-drift.sh` で確認）。
  新バイナリのデプロイ時も allowlist の同期状態を確認すること。
- データ持ち出し（URLに秘密を埋めて外部へGET）を防ぐため、
  `requires_approval = true` による人間承認が必須。宛先ドメインの
  allowlist は実装済み（egress allowlist）。TOML の `egress_allowlist`
  または `JARVIS_MCP_GW_EGRESS_ALLOWLIST` でホストパターンを設定し、
  未設定・空・不正値は `DenyAll`（全宛先拒否・fail-closed）となる。
  詳細は [tool-matrix.md](../docs/integration/tool-matrix.md) を参照。
- （履歴）承認フロー実装前は `requires_approval = true` の呼び出しは拒否
  されていた。MCP-1 の承認フロー実装後は人間承認を待つ挙動へ変更された
  （下記 2026-11-15 追記を参照）。

## 代替案

- upstream MCPサーバ（Firecrawl等）の登録: 依存と外部送信が増えるため却下。
- 実装しない: web系ツールが恒久的に400を返すため却下。

## 影響

> 以下の `web.search` 記述は本 ADR 決定時点の状態であり、現行の挙動ではない。
> 現在は `mcp_allowlist.toml` に掲載された upstream MCP 経由で実行される
> （`gateway.rs` の汎用ツール呼び出し経路）。

- 既存の`web.search`は未実装のまま（呼ぶと400 `no native handler`）。
- デプロイ前に allowlist の `requires_approval` をレビューすること。

## 補足: 承認必須ツールの見え方（履歴）

> 以下の記述は 2026-11-15 の MCP-1 承認フロー実装以前の暫定挙動であり、
> 現行の挙動は下部の追記を参照のこと。

`requires_approval = true` のツールは `AllowList::tools_for_principal` のフィルタにより
**MCPの`tools/list`に表示されない**（呼び出しも`ApprovalRequired`で403）。
したがって承認必須化後は、エージェントからは「存在しないツール」として扱われる。

### 2026-11-15 追記（MCP-1 承認フロー実装）

上記の「非表示」は承認フロー未実装時の暫定挙動だった。MCP-1 の承認フロー実装
（gateway → jarvis-policy HTTP 承認要求 → オペレータ決定 → 実行、失敗時 fail-closed）
に伴い、承認必須ツールも `tools/list` に表示され、呼び出しは人間承認を待つ挙動に変更した。
本 ADR の決定（`web.fetch` のネイティブ実装・SSRF 対策）は変更なし。
