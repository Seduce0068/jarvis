# ADR-0003: web.fetch ネイティブ実装（SSRF対策付き）

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

- **配備済みの `/etc/jarvis/mcp_allowlist.toml` にも `web.fetch` が既に存在**し、
  `requires_approval = false`（`check-deploy-drift.sh`で確認）。
  したがって**新バイナリのデプロイで（allowlist同期なしでも）有効化される**。
- データ持ち出し（URLに秘密を埋めて外部へGET）を防ぐには、
  `requires_approval = true` に変更するか、宛先ドメインのallowlistを導入する。
- 承認フローが未実装の間は `requires_approval = true` が安全（呼び出しは拒否される）。

## 代替案

- upstream MCPサーバ（Firecrawl等）の登録: 依存と外部送信が増えるため却下。
- 実装しない: web系ツールが恒久的に400を返すため却下。

## 影響

- 既存の`web.search`は未実装のまま（呼ぶと400 `no native handler`）。
- デプロイ前に allowlist の `requires_approval` をレビューすること。
