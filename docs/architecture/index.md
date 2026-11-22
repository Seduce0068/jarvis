# Architecture Documents

JARVIS のアーキテクチャ資料・リソース試算の索引です。
一部の資料は OCI 前提の参照資料であり、現行の実行基盤（ローカル Kubuntu + systemd）ではありません。
詳細は各本文と [ADR-0004](../../adr/0004-local-only-no-oci.md) を参照してください。

## 文書一覧

- [report.md](report.md)
  — リソース分析の正本。計算は `calc.rs`、生成物は `dist/`（編集不可）。OCI Always Free 前提の参照資料。
- [verify.rs](verify.rs)
  — 任意のローカル検証スクリプト。ツール未インストール時は skip し、CI からは呼ばれない。
- [personal-ai-architecture.md](personal-ai-architecture.md)
  — JARVIS 型パーソナル AI の構成図・目的。
- [mpe-rust-latex-architecture-report.md](mpe-rust-latex-architecture-report.md)
  — Markdown + MPE + Rust + LaTeX 数式記法の統合構成レポート。
- [opencode-v2-oci-code-server.md](opencode-v2-oci-code-server.md)
  — OpenCode V2 + code-server + OCI セルフホスト開発環境の設計・移行レポート（旧構成の参照資料）。

## 運用

- ディレクトリのルールは [AGENTS.md](AGENTS.md) を参照。
- 全体の概要は [README.md](README.md) を参照。
- 現行の残タスクは [integration/remaining-tasks.md](../integration/remaining-tasks.md) を参照。
