# JARVIS Makefile — 共通操作

.PHONY: help bootstrap-server bootstrap-termux doctor test lint clean deploy deploy-dry deploy-mcp-gw ui-build ui-run ui-test backup-preflight ci-local secret-scan safety-test install-backup-timer install-verify-backup verify-backup test-verify-backup timer-status check-shell check-evidence

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

deploy: ## 無効化（ADR-0004: OCI/リモートデプロイは使用しない）
	@echo "ERROR: 'make deploy' is disabled (ADR-0004: no OCI/remote deploy)." >&2
	@echo "Use 'make deploy-mcp-gw' (local, dry-run by default) instead." >&2
	@exit 1

deploy-dry: ## (レガシー参照) リモートデプロイ内容の dry-run 確認
	@rust-script rust/scripts/deploy.rs --rust-dir rust $(HOST) deploy --dry-run

ci-local: ## 主要ゲートのローカル代替（CI の aarch64 クロス/Docker 依存ゲートは除外）
	@cd rust && cargo fmt --all -- --check
	@cd rust && cargo check --all-targets --locked
	@cd rust && cargo test --all-targets --locked
	@cd rust && cargo clippy --all-targets --locked -- -D warnings -D clippy::undocumented_unsafe_blocks
	@cd ui && cargo fmt -- --check && cargo clippy --all-targets -- -D warnings && cargo test
	@python3 ci/scripts/secret_scan.py
	@bash ci/scripts/check-shell.sh
	@bash ci/scripts/check-evidence.sh
	@bash ci/scripts/test-legacy-guards.sh
	@bash ci/scripts/test-review-run.sh
	@bash ci/scripts/test-verify-backup.sh
	@echo "ci-local: all gates passed"

secret-scan: ## 追跡ファイルの資格情報パターンを走査（値は出力しない）
	@python3 ci/scripts/secret_scan.py

safety-test: ## レガシー fail-closed とバックアップ検証ロジックを検証
	@bash ci/scripts/test-legacy-guards.sh
	@bash ci/scripts/test-verify-backup.sh

timer-status: ## バックアップ timer の状態を表示（読み取り専用）
	@echo "== backup timer status =="
	@systemctl is-active jarvis-backup.timer 2>/dev/null || echo "inactive/not-installed"
	@systemctl is-enabled jarvis-backup.timer 2>/dev/null || echo "not-enabled"
	@systemctl list-timers jarvis-backup.timer --no-pager 2>/dev/null || true
	@echo "== apply command =="
	@echo "sudo bash infra/scripts/install-backup-timer.sh --apply"

check-evidence: ## テスト仕様とエビデンスのハッシュ一致を検証
	@bash ci/scripts/check-evidence.sh

check-shell: ## 全 tracked *.sh の静的解析（bash -n / shellcheck）
	@bash ci/scripts/check-shell.sh

test-verify-backup: ## verify-backup.sh の合成データ回帰テスト
	@bash ci/scripts/test-verify-backup.sh

install-backup-timer: ## ローカルバックアップ timer を導入（既定は dry-run）
	@bash infra/scripts/install-backup-timer.sh

install-verify-backup: ## /opt の verify-backup.sh を更新（root 必須・明示呼び出し）
	@install -o root -g root -m 0755 infra/scripts/verify-backup.sh /opt/jarvis/scripts/verify-backup.sh

verify-backup: ## バックアップの整合性を検証（読み取り専用・作業コピーを使用）
	@sudo -u postgres bash infra/scripts/verify-backup.sh

backup-preflight: ## バックアップ前提の読み取り専用チェック（BK-1 安全ゲート）
	@bash infra/scripts/backup-preflight.sh

deploy-mcp-gw: ## jarvis-mcp-gw をローカルへ安全デプロイ（乾燥実行が既定）
	@bash infra/scripts/deploy-mcp-gw-safe.sh

ui-build: ## GPUI 承認コンソールをビルド（独立クレート、CI 対象外）
	@cd ui && cargo build --release

ui-run: ## GPUI 承認コンソールを起動
	@cd ui && cargo run

ui-test: ## GPUI 承認コンソールの検証（fmt/clippy/test）
	@cd ui && cargo fmt -- --check && cargo clippy --all-targets -- -D warnings && cargo test

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
	@root='$(JARVIS_ROOT)'; \
	home="$$HOME"; \
	while [ "$$root" != "/" ] && [ "$${root%/}" != "$$root" ]; do root="$${root%/}"; done; \
	while [ "$$home" != "/" ] && [ "$${home%/}" != "$$home" ]; do home="$${home%/}"; done; \
	if [ -z "$$root" ]; then \
		echo "ERROR: JARVIS_ROOT is empty; refusing clean." >&2; \
		exit 1; \
	fi; \
	case "$$root" in \
		/*) ;; \
		*) echo "ERROR: JARVIS_ROOT must be an absolute path (got: $$root); refusing clean." >&2; exit 1 ;; \
	esac; \
	case "$$root" in \
		/|.|..|"$$home") echo "ERROR: JARVIS_ROOT must not be /, ., .., or HOME (got: $$root); refusing clean." >&2; exit 1 ;; \
	esac; \
	echo "JARVIS_ROOT safety check passed: $$root"
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
