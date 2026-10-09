# JARVIS Makefile — 共通操作

.PHONY: help bootstrap-server bootstrap-termux doctor test lint clean

# 既定値
JARVIS_ROOT ?= $(HOME)/jarvis
SHELL := /bin/bash

help: ## ヘルプ表示
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-20s\033[0m %s\n", $$1, $$2}'

bootstrap-server: ## サーバー(Linux/OCI/VPS)をブートストラップ
	@chmod +x scripts/bootstrap/server.sh
	@scripts/bootstrap/server.sh --root $(JARVIS_ROOT)

bootstrap-server-dry: ## サーバーブートストラップ (ドライラン)
	@chmod +x scripts/bootstrap/server.sh
	@scripts/bootstrap/server.sh --dry-run --root $(JARVIS_ROOT)

bootstrap-termux: ## Termux(Android)をブートストラップ
	@chmod +x scripts/bootstrap/termux.sh
	@scripts/bootstrap/termux.sh --root $(JARVIS_ROOT)

bootstrap-termux-boot: ## Termuxブートストラップ + setup_boot.sh実行
	@chmod +x scripts/bootstrap/termux.sh
	@scripts/bootstrap/termux.sh --setup-boot --root $(JARVIS_ROOT)

doctor: ## 環境診断実行 (サーバー)
	@chmod +x scripts/doctor/jarvis-doctor.sh
	@JARVIS_ROOT=$(JARVIS_ROOT) scripts/doctor/jarvis-doctor.sh

phone-status: ## 端末ステータス取得 (Termux)
	@chmod +x scripts/utils/phone-status-json.sh
	@scripts/utils/phone-status-json.sh

lint: ## シェルスクリプトの構文チェック
	@command -v shellcheck >/dev/null 2>&1 || (echo "shellcheck not installed"; exit 1)
	@shellcheck scripts/**/*.sh

test: ## テスト実行 (プレースホルダ)
	@echo "No tests configured yet"
	@exit 0

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
	@[ -f $(JARVIS_ROOT)/policies/tool-policy.json ] || cp config/templates/tool-policy.json $(JARVIS_ROOT)/policies/tool-policy.json
	@echo "Config initialized at $(JARVIS_ROOT)"