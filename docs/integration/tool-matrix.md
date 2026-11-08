# jarvis MCP ツール有効性マトリクス（2026-11-09）

`infra/config/mcp_allowlist.toml`・`policy/casbin_policy.csv`・`jarvis-mcp-gw`実装の突き合わせ。
**デプロイ後に実際に動作するのは「ネイティブ実装済み」かつ「policy/allowlistで許可」のものだけ**。

## 実装状況

| ツール | allowlist | 承認 | ネイティブ実装 | デプロイ後の挙動 |
|---|---|---|---|---|
| `filesystem.read` | あり | 不要 | **✓** | 動作（workspace配下に限定） |
| `filesystem.list` | あり | 不要 | **✓** | 動作 |
| `git.status` | あり | 不要 | **✓** | 動作 |
| `git.diff` | あり | 不要 | **✓** | 動作 |
| `web.fetch` | あり | **要変更**（現在false） | **✓** | **デプロイで有効化**（SSRF対策済み・ADR-0003） |
| `web.search` | あり | 不要 | ✗ | 400 `no native handler`（実装/upstream登録待ち） |
| `filesystem.write` | あり | 要 | ✗ | 承認フロー未実装のため実質拒否 |
| `shell.execute` | あり | 要 | ✗ | 承認フロー未実装のため実質拒否 |
| `git.commit` | あり | 要 | ✗ | 同上 |
| `github.create_pr` | あり | 要 | ✗ | 同上 |
| `homeassistant.get_state` | あり | 不要 | ✗ | 400 `no native handler` |
| `homeassistant.call_service` | あり | 要 | ✗ | 承認待ち |
| `android.get_device_info` | あり | 不要 | ✗ | 400 `no native handler` |
| `android.send_notification` | あり | 要 | ✗ | 承認待ち |

## 設定上のドリフト（要整理）

1. **allowlistの`sandbox_type`は無視される**
   - `AllowedTool`構造体に`sandbox_type`が無く、TOMLの値は読まれない（`sandbox_type = "firejail"`等は死んだ設定）。
   - 実際のサンドボックス種別は`McpGatewayConfig.sandbox_type`（既定`None`）。
   - 対応: キーを削除するか、`AllowedTool`へ追加してツール単位ルーティングを実装。
2. **未実装ツールのallowlistエントリ**
   - ポリシー上は許可されているため、呼ぶと「400 no native handler」になる（セキュリティ上の実害はないが混乱の元）。
   - 対応: 実装/upstream登録まで`requires_approval = true`にするか、allowlistから外す。
3. **`web.fetch`の承認**
   - 現状`requires_approval = false`。デプロイ前に`web_fetch_approval.patch`で`true`へ（推奨）。
   - 承認フロー実装後は、宛先ドメイン制限（egress allowlist）も検討。

## デプロイ前チェックリスト

- [ ] `web_fetch_approval.patch` 適用（`requires_approval = true`）
- [ ] 未実装ツールの扱いを決定（残す/外す/承認必須化）
- [ ] `sandbox_type`キーの整理方針を決定
- [ ] `jarvis-mcp-gw`リリースバイナリのSHA256記録
- [ ] デプロイ後に `/mcp` の `tools/list` で意図したツールだけ出ることを確認
