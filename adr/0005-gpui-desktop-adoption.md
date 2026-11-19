---
type: Architecture-Decision
title: "ADR-0005: GPUI desktop adoption for the human approval console"
description: "JARVIS の人間承認コンソールに GPUI（Zed 由来の gpui-pre スナップショット）を採用する決定。Rank: ネイティブデスクトップ UI を rust/ ワークスペース外の独立クレートとして追加する。"
tags:
  - adr
  - jarvis
  - gpui
  - desktop-ui
  - approval
status: accepted
deciders:
  - "Sovereign AI Agent"
date: "2026-11-16"
generated:
  by: "DeepSeek Harness (lead agent)"
  at: "2026-11-18T06:34:00+09:00"
---

## Context

MCP-1 の承認フロー実装により、`jarvis-mcp-gw` は `requires_approval = true` の
ツール呼び出しを `jarvis-policy` の HTTP 承認キューへ転送し、オペレータの
決定を待つようになった（ADR-0002 のランタイム非依存化の一環）。
しかし承認操作はこれまで `curl` による HTTP 叩きのみで、保留中の要求を
一覧し、_CONTEXT を確認し、承認/却下をワンクリックで行うデスクトップ UI が無かった。

UI 候補の評価にあたり、2026-11-16 時点で以下を確認した。

- 本リポジトリには UI クレートが存在せず、`GPUI` の記述も無かった。
- ローカル Kubuntu 2026.04 + Wayland (`wayland-0`) 上で
  `gpui-pre 0.3.12` + `gpui-pre-platform 0.3.12` の
  hello-world が **コンパイル・起動**できることを実機検証済み
  （`cargo build` 成功、プロセス 27 秒生存、Wayland ソケット接続を確認）。
- crates.io の `gpui` クレートは Zed 公式スナップショットの
  `gpui-pre` として公開されており、ライセンスは Apache-2.0。
  プラットフォーム初期化には `gpui-pre-platform` が必要
  （`application()` エントリ・ポイント）。
- 公式 `gpui` クレート 0.2.2 は Linux で `xattr 0.2.3` の
  `libc::ENOATTR` 参照エラーによりビルド不能だった。

## Decision

JARVIS のデスクトップ UI として **GPUI** を採用し、第一段階として
`jarvis-mcp-gw` 承認キューのオペレータ・コンソールを実装する。

- クレート: `ui/`（`jarvis-ui`）。**`rust/Cargo.toml` のワークスペースには
  含めない**。CI の `cargo check --workspace --target aarch64-unknown-linux-gnu`
  はネイティブ・グラフィック・スタックのクロスコンパイルを要求するため、
  GPUI を既存ワークスペースへ入れると CI を壊す。独立ワークスペースとし、
  GPUI 固有のビルド要件をホスト限定に閉じ込める。
- 依存: `gpui-pre` / `gpui-pre-platform` 0.3.12（Zed スナップショット）、
  HTTP は `ureq`（ブロッキング、GPUI のバックグラウンド Executor 上で実行）。
  `reqwest` は Tokio リアクタを要求し GPUI の Executor では動作しないため不採用。
- 通信: `JARVIS_POLICY_HTTP_URL`（既定 `http://127.0.0.1:8081`）のみ。
  3 秒ごとに `GET /v1/approvals` で保留中を取得し、
  `POST /v1/approval/{id}/approve|deny` で決定する。特権操作・外部送信は行わない。
- スコープ: 第一段階は承認キューの表示と承認/却下ボタンのみ。
  設定・監査・記憶の可視化は将来の拡張。

## Rationale

- GPUI は Rust ネイティブで Zed の実プロダクションで使われており、
  パフォーマンスとメンテナンス性が実証済み。
- 既存の Rust sidecar 群と同一言語で、型定義を共有できる
  （承認リクエストの JSON スキーマを Rust 側で検証）。
- 本ユーザー環境（Kubuntu + Wayland）で実機ビルド・起動を確認済み。
- Electron/Tauri 等の Web 技術スタックは依存サイズとセキュリティ表面が
  大きく、ローカル限定の軽量コンソールには過剰。

### 検討した代替案

| 案 | メリット | デメリット | 判断 |
| ---- | -------- | ---------- | ---- |
| `curl` 運用継続 | 依存ゼロ | 一覧性・操作性が低い、監査が困難 | 却下 |
| Web UI (Tauri/Electron) | クロスプラットフォーム | 依存過大、サンドボックス要在 | 却下 |
| egui / iced | 軽量 | JARVIS の Rust エコシステムとの親和性で GPUI に劣る | 保留 |
| GPUI (`gpui-pre`) | Rust ネイティブ・実績・実機確認済み | 公式 `gpui` 未提供、クロスビルド不可 | **採用** |

## Consequences

### 正の影響

- 保留中の承認要求を一覧し、引数 (`context`) を確認してから
  ワンクリックで承認/却下できる（MCP-1 のヒューマン・ループを完成）。
- 既存の Rust 型・HTTP API 契約を再利用でき、重複実装が少ない。

### 負の影響・リスク

- `gpui-pre` は Zed のコミット固定スナップショットであり、
  アップストリーム追従にはバージョン更新が必要。
- グラフィック sysroot を要するため aarch64 クロスビルド不可。
  CI では既存 Rust ワークスペースのみを検証し、UI はホスト限定ビルドとする。
- `ureq` のブロッキング呼び出しは GPUI バックグラウンド・スレッドを占有する。
  現状のポーリング間隔 3 秒では実用上問題ないが、要求数増加時は
  同時実行数の見直しが必要。

### 対応が必要な事項

- [x] `ui/` 独立クレートの作成と実機ビルド検証
- [x] 承認キュー UI（3 秒ポーリング・承認/却下ボタン）
- [x] `cargo fmt` / `cargo clippy -- -D warnings` / `cargo test`（5 tests）
- [x] 実機 Wayland 起動確認（プロセス生存・ダークテーマ描画）
- [x] Makefile に `ui-build` / `ui-run` ターゲットを追加
- [ ] 本番運用時の自動起動（systemd user unit 等）は別判断

## 関連資料

- ADR-0002: Runtime-agnostic architecture（ランタイム非依存化）
- ADR-0003: web.fetch ネイティブ実装（MCP ツール契約）
- `docs/integration/remaining-tasks.md`（MCP-1 承認フロー）
- `ui/README.md`（ビルド・起動手順）
- GPUI: <https://gpui.rs> / <https://github.com/zed-industries/zed>
