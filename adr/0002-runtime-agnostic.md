---
type: Architecture-Decision
title: "ADR-0002: Runtime-agnostic JARVIS (ACP/MCP contracts)"
description: OpenCode固定をやめ、エージェントランタイムとIDEをACP/MCPの契約で交換可能にする
tags: [adr, jarvis, architecture, acp, mcp, dsh]
status: accepted
date: "2026-11-09"
accepted_at: "2026-11-17"
---

## Context
ADR-0001 は OpenCode V2 を公式 Agent Runtime、code-server を IDE と位置づけた。
しかしコアの価値（ポリシー承認・ツール許可・記憶・イベント正規化）はランタイム非依存であり、
DSH・Zed・将来のエージェントへ入れ替えるたびに jarvis を改修するのは持続可能でない。
実際、jarvis の Rust サイドカーはほぼ汎用で、結合は主に systemd・設定・イベント正規化にある。

## Decision
JARVIS を「ランタイム非依存のポリシー・記憶・イベント基盤」と定義し、以下を契約とする。

1. **MCP（Streamable HTTP `/mcp`）**: エージェント→ツールの唯一の入口。
   すべてのツール呼び出しは `jarvis-mcp-gw` を通り、allowlist と `jarvis-policy` の承認を受ける。
2. **ACP（Agent Client Protocol）**: エディタ→エージェント。ランタイム（DSH / OpenCode 等）が ACP サーバを提供。
3. **HTTP webhook**: 外部イベント→`jarvis-event`。形式はランタイム非依存の正規化イベント。
4. **gRPC/HTTP**: jarvis 内部（policy / context）の呼び出し契約。

### 交換可能
- エージェントランタイム: OpenCode（初期実装） / DSH（第二実装） / generic CLI
- IDE・クライアント: Zed（ACP） / DSH Web / code-server（任意） / Muse（MCPクライアント）
- LLMプロバイダ（各ランタイム側で設定）

### 固定（コア）
- 承認・許可リスト・サンドボックス・監査（jarvis-policy / jarvis-mcp-gw）
- 記憶（jarvis-context: PostgreSQL + Qdrant）
- イベント正規化（jarvis-event）

## Consequences
- OpenCode / code-server は `profiles/` 配下の交換可能な実装へ降格する。
- DSH は `acp` プロファイル（ACPサーバ）と MCPクライアントで接続できる。
- `jarvis-event/src/opencode.rs` は固有アダプタとして分離し、汎用イベントへ写像する。
- `jarvis-mcp-gw` の sandbox fallback（`opencode` CLI 依存）は撤去済み（2026-11-15 実装）。
  実行バックエンドは「ネイティブ Rust ハンドラ（読み取り系）+ 上流 MCP サーバ登録
  （`register_mcp_server`）」のランタイム非依存構成で置き換え、未実装ツールは
  明示的なエラーを返す。`JARVIS_MCP_GW_SANDBOX_TYPE` の設定値は互換性のため
  解析のみに残す（実行経路には影響しない）。
- ポリシー迂回を避けるため、ランタイムのネイティブツール利用は明示的に制限する。

## Migration (incremental)
1. 契約の文書化（本ADR）
2. DSH統合: ACP起動・MCPクライアント接続・イベントアダプタ
3. code-serverのオプション化、Zed(ACP)対応
4. sandbox fallbackの汎用化（2026-11-15 完了: opencode CLI 依存の撤去）
5. 契約テスト（MCP tools/call、ACP session、doctor）

## Status rationale (2026-11-17)

ユーザーが GPUI 採用を明示的に決定し（ADR-0005）、ランタイム非依存化の
主要項目が実装・検証済みになったため `accepted` へ変更した。

- MCP 契約: `tools/list` / `tools/call` を smoke (12 checks) と drift で検証。
- 承認契約: gateway → policy HTTP → オペレータ決定 → fail-closed を E2E 検証。
- 実行バックエンド: `opencode` CLI fallback を撤去し、ネイティブハンドラと
  上流 MCP サーバ登録に統一。
- レビュー実行: `ci/scripts/review-run.sh` で `JARVIS_REVIEW_RUNNER` による
  バックエンド切替を契約テスト済み。
- 残課題: ACP session / doctor の自動契約テストは未実装（段階移行の範囲外）。

## 記録（未決定）: PII の保存時扱い（2026-11-18）

本節は現行実装の挙動を記録するもので、新たな設計決定ではない。
Decision / Consequences および frontmatter の `status` は変更しない。

- PII フィルタは LLM 送信前の `build_context` にのみ適用する。
- `store_memory` は `content` / `metadata` を生のまま
  PostgreSQL / Qdrant へ保存する。
- 根拠: 保存時にマスキングすると復元不能になり、
  検索品質も落ちるため。
- リスク: DB / Qdrant への直接アクセスで生 PII が読めること。
- 将来判断条件: 保存時暗号化・フィールド単位マスキング・
  保持期限を導入する場合に再評価する。

根拠コード: [`builder.rs`](../rust/jarvis-context/src/builder.rs)
