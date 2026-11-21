# JARVIS Makefile — 共通操作

.PHONY: help bootstrap-server bootstrap-server-dry bootstrap-server-minimal bootstrap-server-minimal-dry bootstrap-termux bootstrap-termux-boot doctor phone-status test test-restore-local lint clean deploy deploy-dry deploy-mcp-gw ui-build ui-run ui-test backup-preflight ci-local secret-scan safety-test install-backup-timer install-verify-backup verify-backup test-verify-backup test-backup-args test-workflow-pins timer-status check-shell check-workflow-pins check-toml check-dependency-audit print-cargo-audit-version check-evidence check-evidence-counts init-config

# 既定値
CARGO_AUDIT_VERSION ?= 0.23.0
JARVIS_ROOT ?= $(HOME)/jarvis
HOST ?= jarvis-host
# Portable bash lookup: Termux has no /bin/bash, Ubuntu has /bin/bash or /usr/bin/bash.
SHELL := $(shell command -v bash 2>/dev/null || echo /bin/sh)

help: ## ヘルプ表示
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-20s\033[0m %s\n", $$1, $$2}'

# I84-A / I85-C: rust-script 依存の exact pin 方針
# 下記ターゲットが呼ぶ rust-script は、各 .rs 先頭の `/// ```cargo` メタブロックに
# 直接依存を宣言する。bootstrap / deploy / backup / restore は特権・本番接触の
# 可能性があるため、再現性と監査性の観点から直接依存は `=x.y.z` の exact pin へ
# 統一する（推移依存まで固定する場合は rust-script 側のロック運用を別途調査して
# 採用する）。
# 根拠バージョンは rust/Cargo.lock の解決版とする（対象スクリプトは rust/scripts/
# 配下であり、ui/Cargo.lock は serde_json / tokio 等が異なるため混用しない）。
# 現状（I85-C）: ロックに載っている依存は pin 済み。`clap` と `hostname` は
# rust/Cargo.lock / ui/Cargo.lock のいずれにも無く、リポジトリ内に利用バージョンの
# 根拠が無いため pin を保留する（推測で数値を書かない）。これらを workspace ロック
# に載せるか、rust-script 実行時に生成されるロックを取得してから pin すること。
# rust-script 本体のバージョン固定方針: インストールは
# `cargo install rust-script --version 0.36.0 --locked` のように exact version を
# 明示し、`rust-script --version` が一致することを手動ゲートで確認する。更新は
# 意図的な変更として記録し、各メタブロックの pin と同時に見直す（ローカルの
# インストール／キャッシュ上の解決版は 0.36.0）。
# 補足: `make ci-local` は rust-script を実行せず（cargo ゲート + シェル/
# Python ゲートのみ）、rust-script 自体のバージョンも CI で固定されていないため、
# この drift は現状 CI ではなく手動の特権操作経路に留まる。
# 更新時はこのコメントと各スクリプトのメタブロックを同時に見直すこと。

bootstrap-server: ## サーバー(Linux/VPS)をブートストラップ
	@rust-script scripts/bootstrap/bootstrap.rs --target server --source-root . --root $(JARVIS_ROOT)

bootstrap-server-dry: ## サーバーブートストラップ (ドライラン)
	@rust-script scripts/bootstrap/bootstrap.rs --target server --source-root . --dry-run --root $(JARVIS_ROOT)

bootstrap-server-minimal: ## サーバーブートストラップ (opencode/android-mcp 導入をスキップ)
	@rust-script scripts/bootstrap/bootstrap.rs --target server --source-root . --skip-opencode --skip-android-mcp --root $(JARVIS_ROOT)

bootstrap-server-minimal-dry: ## サーバーブートストラップのドライラン (opencode/android-mcp スキップ)
	@rust-script scripts/bootstrap/bootstrap.rs --target server --source-root . --dry-run --skip-opencode --skip-android-mcp --root $(JARVIS_ROOT)

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
	@cd ui && cargo fmt -- --check && cargo clippy --all-targets --locked -- -D warnings -D clippy::undocumented_unsafe_blocks && cargo test --locked
	@python3 ci/scripts/secret_scan.py
	@bash ci/scripts/check-shell.sh
	@bash ci/scripts/check-workflow-pins.sh
	@bash ci/scripts/test-workflow-pins.sh
	@bash ci/scripts/check-toml.sh
	@bash ci/scripts/check-dependency-audit.sh
	@bash ci/scripts/check-evidence.sh
	@$(MAKE) check-evidence-counts
	@bash ci/scripts/test-legacy-guards.sh
	@bash ci/scripts/test-review-run.sh
	@bash ci/scripts/test-verify-backup.sh
	@bash ci/scripts/test-backup-args.sh
	@bash infra/scripts/test-restore-local.sh
	@echo "ci-local: all gates passed"

secret-scan: ## 追跡ファイルの資格情報パターンを走査（値は出力しない）
	@python3 ci/scripts/secret_scan.py

# DEPENDENCY_AUDIT_FETCH が設定されていれば監査スクリプトへ透伝する（CI は 1 で fetch 実行）。
safety-test: ## レガシー fail-closed とバックアップ検証ロジックを検証
	@bash ci/scripts/test-legacy-guards.sh
	@DEPENDENCY_AUDIT_FETCH=$${DEPENDENCY_AUDIT_FETCH:-} bash ci/scripts/check-dependency-audit.sh
	@bash ci/scripts/test-workflow-pins.sh
	@bash ci/scripts/test-verify-backup.sh
	@bash ci/scripts/test-backup-args.sh
	@bash infra/scripts/test-restore-local.sh

timer-status: ## バックアップ timer の状態を表示（読み取り専用）
	@echo "== backup timer status =="
	@systemctl is-active jarvis-backup.timer 2>/dev/null || echo "inactive/not-installed"
	@systemctl is-enabled jarvis-backup.timer 2>/dev/null || echo "not-enabled"
	@systemctl list-timers jarvis-backup.timer --no-pager 2>/dev/null || true
	@echo "== apply command =="
	@echo "sudo bash infra/scripts/install-backup-timer.sh --apply"

check-evidence: ## テスト仕様とエビデンスのハッシュ一致を検証
	@bash ci/scripts/check-evidence.sh

# 各証跡 JSON の results と実 cargo suite の導出件数を照合（MATCH/MISMATCH のみ出力）。
# backup は generate-evidence ターゲットが無いため --check 件数照合の対象外。
# 証跡ファイル名は evidence/index.md の [run-<name>]: 定義から解決する（双重管理を避ける）。
check-evidence-counts: ## 証跡 JSON の results と実 suite の導出件数を照合
	@for name in policy mcp context event ui; do \
	  file=$$(sed -n "s/^\[run-$$name\]: //p" evidence/index.md); \
	  if [ -z "$$file" ] || [ ! -f "evidence/$$file" ]; then \
	    echo "FAIL: EVIDENCE_REF_MISSING $$name" >&2; exit 1; \
	  fi; \
	  bash ci/scripts/generate-evidence.sh "$$name" --check "evidence/$$file" || exit 1; \
	done

check-shell: ## 全 tracked *.sh の静的解析（bash -n / shellcheck）
	@bash ci/scripts/check-shell.sh

check-workflow-pins: ## workflow の @vN ピンを記録 SHA と GitHub API で照合（オフラインは未検証警告・fail-closed）
	@bash ci/scripts/check-workflow-pins.sh

test-workflow-pins: ## ピン照合ゲートの回帰（mini workflow と疑似 API・ネットワーク不要）
	@bash ci/scripts/test-workflow-pins.sh

check-toml: ## 全 tracked *.toml の構文検証（tomllib/tomli、値は出力しない・fail-closed）
	@bash ci/scripts/check-toml.sh

# I94-A: cargo-audit が未導入の場合は AUDIT_TOOL_MISSING で fail-closed となる。
# ローカルでは自動 install しない（fail-closed 契約を維持するため）。未導入時は
# CARGO_AUDIT_VERSION（既定 0.23.0）で手動導入し、`cargo audit --version` が
# 一致することを確認する。CI は print-cargo-audit-version で同じ値を参照する。
#   cargo install cargo-audit --version $(CARGO_AUDIT_VERSION) --locked
print-cargo-audit-version: ## CI install step が参照する固定バージョンを出力
	@printf '%s\n' '$(CARGO_AUDIT_VERSION)'

check-dependency-audit: ## rust/ui 両 Cargo.lock の既知脆弱性を監査（cargo-audit 必須・fail-closed）
	@bash ci/scripts/check-dependency-audit.sh

test-verify-backup: ## verify-backup.sh の合成データ回帰テスト
	@bash ci/scripts/test-verify-backup.sh

test-backup-args: ## backup-local.sh の --retain-days 引数検証を検証（dry-run・失敗時はコードのみ）
	@bash ci/scripts/test-backup-args.sh

test-restore-local: ## restore-local.sh の合成フィクスチャ回帰テスト（既定 dry-run・本番非接触・承認不要）
	@bash infra/scripts/test-restore-local.sh

install-backup-timer: ## ローカルバックアップ timer を導入（既定は dry-run）
	@bash infra/scripts/install-backup-timer.sh

install-verify-backup: ## /opt の verify-backup.sh と共有 lib を更新（root 必須・明示呼び出し）
	@install -o root -g root -m 0755 infra/scripts/verify-backup.sh /opt/jarvis/scripts/verify-backup.sh
	@install -d -o root -g root -m 0755 /opt/jarvis/scripts/lib
	@install -o root -g root -m 0644 infra/scripts/lib/payload-descriptors.sh /opt/jarvis/scripts/lib/payload-descriptors.sh

verify-backup: ## バックアップの整合性を検証（読み取り専用・作業コピーを使用）
	@sudo -u postgres bash infra/scripts/verify-backup.sh

backup-preflight: ## バックアップ前提の読み取り専用チェック（BK-1 安全ゲート）
	@bash infra/scripts/backup-preflight.sh

deploy-mcp-gw: ## jarvis-mcp-gw をローカルへ安全デプロイ（乾燥実行が既定）
	@bash infra/scripts/deploy-mcp-gw-safe.sh

ui-build: ## GPUI 承認コンソールをビルド（独立クレート、UI 検証は make ui-test・CI 対象）
	@cd ui && cargo build --release --locked

ui-run: ## GPUI 承認コンソールを起動
	@cd ui && cargo run --locked

ui-test: ## GPUI 承認コンソールの検証（fmt/clippy/test）
	@cd ui && cargo fmt -- --check && cargo clippy --all-targets --locked -- -D warnings -D clippy::undocumented_unsafe_blocks && cargo test --locked

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
	@cd rust && cargo test --workspace --locked
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
