---
type: Reference
title: "JARVIS Approval Console (GPUI)"
description: "jarvis-mcp-gw の承認キューを操作するデスクトップ・コンソール（ui/ クレート）のビルド・起動手順。"
tags:
  - jarvis
  - gpui
  - approval
  - desktop-ui
---

## 概要

`ui/` は `jarvis-mcp-gw` の MCP-1 承認フローに対応するオペレータ用
デスクトップ・コンソールです。`jarvis-policy` の HTTP API を 3 秒ごとに
ポーリングし、保留中のツール呼び出しを一覧表示して承認/却下できます。

**重要**: `rust/Cargo.toml` のワークスペースには含まれません。
CI の aarch64 クロスビルドを壊さないための独立クレートです
（[ADR-0005](../adr/0005-gpui-desktop-adoption.md)）。

## 必要なもの

- Rust 1.85+（edition 2024）
- Linux: `fontconfig`, `libxkbcommon`, `wayland-client`, `wayland-cursor`
  （Kubuntu 2026.04 では標準で揃っています）
- Wayland または X11 セッション

## ビルド

```bash
cd ~/ドキュメント/作業用/jarvis/jarvis/ui
cargo build            # 開発ビルド
cargo build --release  # リリースビルド
cargo test             # 5 テスト
cargo clippy --all-targets -- -D warnings
```

## 起動

```bash
cd ~/ドキュメント/作業用/jarvis/jarvis/ui
cargo run
```

既定で `http://127.0.0.1:8081`（jarvis-policy の HTTP エンドポイント）へ
接続します。変更する場合は環境変数を指定します。

```bash
JARVIS_POLICY_HTTP_URL=http://127.0.0.1:8081 cargo run
```

## 使い方

1. コンソールを起動すると「JARVIS 承認コンソール」ウィンドウが開きます。
2. 保留中の要求が 3 秒ごとに自動更新されます（件数はヘッダに表示）。
3. 各行に要求元 (`principal`)、リソース (`tool:<name>`)、期限、
   引数 (`context`) は切り詰めず全量を表示します（承認前にコマンド全体を確認）。
4. **承認** または **却下** ボタンを押すと、jarvis-policy へ決定が送信され、
   直ちに一覧が再読み込みされます。
5. jarvis-policy に接続できない場合は赤いエラー・バナーを表示し、
   最後に取得した一覧を保持します（古いキューを空と誤認しないため）。

## セキュリティ

- 接続先は `JARVIS_POLICY_HTTP_URL` のみ（既定は loopback）。
- 特権操作・外部送信・秘密情報の読み取りは行いません。
- 決定時の `approved_by` は既定で `koppe`（オペレータ）です。

## 実サービスでの動作確認（2026-11-17）

モックではなく実機の `jarvis-policy`（`127.0.0.1:8081`）に対して確認済み:

```bash
# テスト用の保留要求を作る（即座にブロックされるため background で実行）
id="$(python3 -c 'import uuid; print(uuid.uuid4())')"
now=$(date +%s); exp=$((now+120))
base=http://127.0.0.1:8081/v1/approval
body="{\"id\":\"$id\",\"principal\":\"opencode\",\"action\":\"execute\""
body="$body,\"resource\":\"tool:filesystem.read\",\"context\":{\"path\":\"README.md\"}"
body="$body,\"requested_at\":$now,\"expires_at\":$exp}"
curl -s -X POST "$base" -H 'content-type: application/json' -d "$body" &
# コンソール起動（保留一覧を3秒ごとに取得）
JARVIS_POLICY_HTTP_URL=http://127.0.0.1:8081 cargo run
# 検証後は必ず片付ける（pending を 0 に戻す）
curl -s -X POST "http://127.0.0.1:8081/v1/approval/$id/deny" \
  -H 'content-type: application/json' -d '{"reason":"E2E test cleanup"}'
```

実機確認結果: コンソールは生存し、保留一覧を取得。テスト要求を却下して
`pending` は 0 に戻した（状態を汚していない）。

## 関連

- [ADR-0005: GPUI desktop adoption](../adr/0005-gpui-desktop-adoption.md)
- [MCP ツール有効性マトリクス](../docs/integration/tool-matrix.md)
- [残タスク (MCP-1)](../docs/integration/remaining-tasks.md)
