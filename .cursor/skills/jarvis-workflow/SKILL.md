---
name: jarvis-workflow
description: jarvisリポジトリ専用ワークフロー。個人ユーティリティ/ワークスペースでの開発・ドキュメント・自動化タスクを支援。jarvis固有の規約・フック・スキル連携を制御する。
disable-model-invocation: false
---

# jarvis Workflow

## 概要

このスキルは `jarvis` リポジトリ専用のワークフロー自動化を提供します。

- **リポジトリ**: 個人ユーティリティ / ワークスペース
- **主要技術**: TypeScript, Rust, Markdown, Shell
- **自動化**: hooks.json + .cursor/rules/ + サブエージェント連携

## クイックスタート

### 開発フロー

```bash
# 1. 変更前: 関連スキル自動適用確認
# pre-taskフックで自動判定

# 2. 実装: コーディング規約準拠
# coding-standards.mdc が常時適用

# 3. 編集後: 自動検証
# post-editフックで lint/format/check 実行

# 4. コミット前: 最終チェック
# pre-commitフックで markdownlint 実行
```

### よく使うコマンド

```bash
# Markdown品質チェック
npx markdownlint **/*.md

# Rustフォーマット/リント
cargo fmt --check && cargo clippy -- -D warnings

# ルール/スキル変更後の再読み込み
# （自動的に次回タスクで反映）
```

## スキル連携マトリクス

| タスク | 主スキル | 補助スキル/エージェント |
|-------|---------|----------------------|
| Azureインフラ設計 | `azure-enterprise-infra-planner` | `azure-kubernetes`, `azure-rbac` |
| Azureデプロイ準備 | `azure-prepare` | `azure-validate`, `azure-deploy` |
| AIモデルデプロイ | `deploy-model`, `preset`, `customize` | `microsoft-foundry`, `finetuning` |
| コードレビュー | `review-security` + `arch-reviewer` + `ops-reviewer` | `cross-review` |
| ドキュメント生成 | `doc-create` | `tech-selection` (ADR時) |
| 技術選定 | `tech-selection` / `ai-selection` | - |
| テスト設計 | `test-spec` | - |
| ユーザースクリプト | `userscript-validator` | - |

## フック連携

`hooks.json` 参照。主要イベント：

- `pre-task`: キーワードマッチでスキル/サブエージェント自動起動
- `post-edit`: ファイル種別ごとの自動検証
- `pre-commit`: マークダウン最終チェック

## 設定ファイル位置

```
jarvis/
├── .cursor/
│   ├── rules/
│   │   ├── skill-auto-apply.mdc    # スキル自動適用ルール
│   │   └── coding-standards.mdc    # コーディング規約
│   └── skills/
│       └── jarvis-workflow/        # このスキル
└── hooks.json                      # イベント駆動フック
```

## カスタマイズ

### 新しいキーワードトリガー追加

`hooks.json` の `pre-task` にマッチャー追加：

```json
{
  "matcher": "新キーワード|関連語",
  "command": "skill スキル名 または subagent エージェント名",
  "description": "説明"
}
```

### 常時適用ルール追加

`.cursor/rules/新規ルール.mdc` 作成：

```yaml
---
description: ルール説明
alwaysApply: true
---
```

### プロジェクト固有スキル追加

`.cursor/skills/新スキル名/SKILL.md` 作成（`create-skill` スキル参考）

## トラブルシューティング

| 症状 | 確認事項 | 対処 |
|------|---------|------|
| スキルが適用されない | `description` にキーワード含むか | キーワード追加 or 明示的呼び出し |
| フックが動かない | `hooks.json` 構文 / マッチャーパターン | JSON検証 / 正規表現確認 |
| ルール競合 | `alwaysApply: true` 複数存在 | 優先順位整理 / 統合 |

## 更新履歴

- 2026-10-09: 初版作成（スキル自動適用・フック・規約統合）
