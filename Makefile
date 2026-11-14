# JARVIS Makefile — 共通操作

.PHONY: help bootstrap-server bootstrap-termux doctor test lint clean deploy deploy-dry deploy-mcp-gw

# 既定値
JARVIS_ROOT ?= $(HOME)/jarvis
HOST ?= jarvis-host
# Portable bash lookup: Termux has no /bin/bash, Ubuntu has /bin/bash or /usr/bin/bash.
SHELL := $(shell command -v bash 2>/dev/null || echo /bin/sh)

help: ## ヘルプ表示
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-20s\033[0m %s\n", $$1, $$2}'

bootstrap-server: ## サーバー(Linux/VPS)をブートストラップ
	@rust-script scripts/bootstrap/bootstrap.rs --target server --source-root . --root $(JARVIS_ROOT)

bootstrap-server-dry: ## サーバーブートストラップ (ドライラン)
	@rust-script scripts/bootstrap/bootstrap.rs --target server --source-root . --dry-run --root $(JARVIS_ROOT)

bootstrap-termux: ## Termux(Android)をブートストラップ
	@rust-script scripts/bootstrap/bootstrap.rs --target termux --source-root . --root $(JARVIS_ROOT)

bootstrap-termux-boot: ## Termuxブートストラップ + setup_boot.sh実行
	@rust-script scripts/bootstrap/bootstrap.rs --target termux --source-root . --setup-boot --root $(JARVIS_ROOT)

doctor: ## 環境診断実行 (サーバー)
	@JARVIS_ROOT=$(JARVIS_ROOT) rust-script scripts/doctor/jarvis-doctor.rs

phone-status: ## 端末ステータス取得 (Termux)
	@rust-script scripts/utils/phone-status-json.rs

deploy: ## (レガシー) リモートホストへデプロイ (HOST=jarvis-host)
	@rust-script rust/scripts/deploy.rs --rust-dir rust $(HOST) deploy

deploy-dry: ## (レガシー) リモートデプロイ内容の確認 (dry-run, HOST=jarvis-host)
	@rust-script rust/scripts/deploy.rs --rust-dir rust $(HOST) deploy --dry-run

deploy-mcp-gw: ## jarvis-mcp-gw をローカルへ安全デプロイ（乾燥実行が既定）
	@bash infra/scripts/deploy-mcp-gw-safe.sh

lint: ## Rust script の整形・起動確認
	@rustfmt --check rust/scripts/*.rs scripts/doctor/*.rs scripts/utils/*.rs scripts/bootstrap/*.rs config/*.rs docs/architecture/verify.rs docs/architecture/calc.rs
	@rust-script scripts/bootstrap/bootstrap.rs --help >/dev/null
	@rust-script config/git-utf8.rs --help >/dev/null
	@rust-script docs/architecture/verify.rs --help >/dev/null
	@rust-script rust/scripts/deploy.rs --help >/dev/null
	@rust-script rust/scripts/backup.rs --help >/dev/null
	@rust-script rust/scripts/restore.rs --help >/dev/null
	@rust-script scripts/doctor/jarvis-doctor.rs --help >/dev/null
	@rust-script scripts/utils/phone-status-json.rs --help >/dev/null
	@rust-script config/shell-locale.rs >/dev/null

test: ## Rust workspace と rust-script のテスト
	@cd rust && cargo test --workspace
	@rust-script --test rust/scripts/restore.rs
	@TERMUX_VERSION=test rust-script scripts/bootstrap/bootstrap.rs --target termux --dry-run --skip-android-mcp --source-root . --root "$(JARVIS_ROOT)"

clean: ## 生成物削除 (注意: 設定ファイルも消える)
	@echo "This will remove: bin/ state/ logs/ repos/ config/*.env policies/*.json"
	@read -p "Continue? [y/N] " ans && [ "$$ans" = "y" ]
	@rm -rf $(JARVIS_ROOT)/bin $(JARVIS_ROOT)/state $(JARVIS_ROOT)/logs $(JARVIS_ROOT)/repos
	@rm -f $(JARVIS_ROOT)/config/*.env $(JARVIS_ROOT)/policies/*.json

# 開発用: テンプレートから設定ファイルを生成 (既存ファイルは上書きしない)
init-config: ## テンプレートから設定ファイルを初期生成
	@mkdir -p $(JARVIS_ROOT)/config $(JARVIS_ROOT)/policies
	@[ -f $(JARVIS_ROOT)/config/jarvis.env ] || cp config/templates/jarvis.env.example $(JARVIS_ROOT)/config/jarvis.env && chmod 600 $(JARVIS_ROOT)/config/jarvis.env
	@[ -f $(JARVIS_ROOT)/config/mobile.env ] || cp config/templates/mobile.env.example $(JARVIS_ROOT)/config/mobile.env && chmod 600 $(JARVIS_ROOT)/config/mobile.env
	@echo "Config initialized at $(JARVIS_ROOT)"
