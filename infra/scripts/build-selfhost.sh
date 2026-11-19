#!/usr/bin/env bash
# Build and deploy the JARVIS Rust sidecars on the self-hosted host.
#
# Run as the JARVIS user (not root):
#   bash build-selfhost.sh
#
# Expects the repository at ~/jarvis and setup-selfhost.sh to have run.
set -euo pipefail

REPO_DIR="${REPO_DIR:-$HOME/jarvis}"
JARVIS_USER="${JARVIS_USER:-$(id -un)}"

if [[ ! -d "${REPO_DIR}/rust" ]]; then
  echo "repository not found at ${REPO_DIR}" >&2
  exit 1
fi

# I35-A: one-commit distribution guarantee for migrations.
# `sqlx::migrate!()` in jarvis-context embeds rust/migrations into the binary
# at build time, while jarvis-migrations.service applies the same SQL from
# /opt/jarvis/rust/migrations as a pre-flight. sqlx records a per-migration
# checksum in _sqlx_migrations, so a binary/source mismatch makes context fail
# at startup ("migration ... was previously applied but has been modified").
# Every artifact here is built from this single checkout: never combine a
# binary from another commit with these migration sources (or vice versa).
MIGRATIONS_SRC="${REPO_DIR}/rust/migrations"
if [[ ! -d "${MIGRATIONS_SRC}" ]]; then
  echo "migration sources not found at ${MIGRATIONS_SRC}" >&2
  exit 1
fi
# I36-A: a fresh install must always have SQL to apply. An empty source tree
# would leave jarvis-migrations.service --source pointing at no migrations,
# so fail fast instead of deploying a pre-flight unit that can do nothing.
shopt -s nullglob
MIGRATION_SQL_FILES=("${MIGRATIONS_SRC}"/*.sql)
shopt -u nullglob
if (( ${#MIGRATION_SQL_FILES[@]} == 0 )); then
  echo "no *.sql migration files found in ${MIGRATIONS_SRC}" >&2
  exit 1
fi
# I37-A: resolve the commit that produced this build with fail-closed
# semantics. The former `git ... || echo unknown` fallback made the
# reconciliation guard pass trivially when git failed (both sides "unknown"),
# so a binary/migration pair could be deployed with no real commit evidence.
# An unusable commit is now fatal and is detected before any payload is staged
# or installed.
if ! BUILD_COMMIT="$(git -C "${REPO_DIR}" rev-parse HEAD 2>/dev/null)"; then
  echo "git rev-parse HEAD failed at ${REPO_DIR}; refusing to deploy" >&2
  exit 1
fi
if [[ -z "${BUILD_COMMIT}" || "${BUILD_COMMIT}" == "unknown" ]]; then
  echo "git rev-parse HEAD returned an unusable commit ('${BUILD_COMMIT}'); refusing to deploy" >&2
  exit 1
fi

cd "${REPO_DIR}/rust"
cargo build --release --workspace

# I37-A: stage every payload locally, record both markers, then run ALL
# fallible checks before writing anything under /opt. The old code installed
# the four binaries and the migration SQL first and only then ran the guard,
# so a mismatch exited 1 while leaving new binaries paired with old migration
# sources -- exactly the "half-updated pair" its comment claimed to prevent.
STAGE_DIR="$(mktemp -d)"
trap 'rm -rf "${STAGE_DIR}"' EXIT
STAGE_BIN="${STAGE_DIR}/bin"
STAGE_MIG="${STAGE_DIR}/migrations"
mkdir -p "${STAGE_BIN}" "${STAGE_MIG}"

BINARIES=(jarvis-policy jarvis-mcp-gw jarvis-event jarvis-context)
for bin in "${BINARIES[@]}"; do
  if [[ ! -s "target/release/${bin}" ]]; then
    echo "built binary missing or empty: target/release/${bin}" >&2
    exit 1
  fi
  install -m 755 "target/release/${bin}" "${STAGE_BIN}/${bin}"
done
install -m 0644 "${MIGRATION_SQL_FILES[@]}" "${STAGE_MIG}/"

# Record the build commit immediately after the build, before any binary
# install. Both markers are derived from the single HEAD resolved above; that
# single-checkout provenance, plus the fail-closed rev-parse, is what makes
# the binary/migration pair a one-commit pair.
bin_commit_file="${STAGE_BIN}/.jarvis-build-commit"
mig_commit_file="${STAGE_MIG}/.jarvis-source-commit"
printf '%s\n' "${BUILD_COMMIT}" > "${bin_commit_file}"
printf '%s\n' "${BUILD_COMMIT}" > "${mig_commit_file}"

# Reconciliation boundary: verify the staged payloads and both markers. A
# mismatch, a missing/empty marker or an unusable commit exits here with zero
# binaries and zero migration files installed under /opt.
for marker in "${bin_commit_file}" "${mig_commit_file}"; do
  if [[ ! -s "${marker}" ]]; then
    echo "build marker missing or empty at ${marker}" >&2
    exit 1
  fi
  staged_commit="$(<"${marker}")"
  if [[ "${staged_commit}" != "${BUILD_COMMIT}" ]]; then
    echo "staged commit ${staged_commit} in ${marker} != ${BUILD_COMMIT}; refusing to deploy a mixed pair" >&2
    exit 1
  fi
done
shopt -s nullglob
STAGED_SQL_FILES=("${STAGE_MIG}"/*.sql)
shopt -u nullglob
if (( ${#STAGED_SQL_FILES[@]} != ${#MIGRATION_SQL_FILES[@]} )); then
  echo "staged SQL count ${#STAGED_SQL_FILES[@]} != source SQL count ${#MIGRATION_SQL_FILES[@]} in ${MIGRATIONS_SRC}" >&2
  exit 1
fi

MIGRATIONS_DST="/opt/jarvis/rust/migrations"

# All checks passed; deploy from the validated staging set. Payload files go
# in first and the commit markers go in LAST, so the markers act as the seal
# of a completed deploy: if an install aborts mid-way, set -e stops here, the
# "start with:" instructions below are never printed and the previous markers
# no longer describe the mixed on-disk payload. The next successful run
# re-installs the whole set and is idempotent. (A true atomic swap of the bin
# and migrations trees is not attempted here because their install paths are
# referenced directly by the systemd units.)
for bin in "${BINARIES[@]}"; do
  install -m 755 "${STAGE_BIN}/${bin}" "/opt/jarvis/bin/${bin}"
done
sudo install -d -o root -g root -m 0755 "${MIGRATIONS_DST}"
sudo install -o root -g root -m 0644 "${STAGED_SQL_FILES[@]}" "${MIGRATIONS_DST}/"
sudo install -o root -g root -m 0644 "${bin_commit_file}" /opt/jarvis/bin/.jarvis-build-commit
sudo install -o root -g root -m 0644 "${mig_commit_file}" "${MIGRATIONS_DST}/.jarvis-source-commit"

sudo mkdir -p /etc/jarvis
# Copy the *contents* of infra/config/policy/ directly into /etc/jarvis/policy/.
# A plain `cp -r <src> /etc/jarvis/` nests <src>/policy inside an existing
# /etc/jarvis/policy, leaving the stale config effective (todo.md 2026-11-19).
POLICY_SRC="${REPO_DIR}/infra/config/policy"
POLICY_DST="/etc/jarvis/policy"
if [[ -e "${POLICY_DST}" || -L "${POLICY_DST}" ]]; then
  policy_backup="${POLICY_DST}.bak-$(date +%Y%m%d%H%M%S)"
  sudo mv "${POLICY_DST}" "${policy_backup}"
  echo "backed up existing policy dir to ${policy_backup}"
fi
sudo mkdir -p "${POLICY_DST}"
sudo cp -r "${POLICY_SRC}/." "${POLICY_DST}/"
sudo cp "${REPO_DIR}/infra/config/pii_patterns.toml" /etc/jarvis/
sudo cp "${REPO_DIR}/infra/config/mcp_allowlist.toml" /etc/jarvis/

# P0-6: otelcol.service は /etc/jarvis/otelcol-config.yaml を読み込むため、
# repo テンプレートを配布経路に含めないと repo/実機設定が drift する。
# 既存ファイルは .bak-<ts> へ退避してから root:root 0644 で配置する。
OTEL_SRC="${REPO_DIR}/infra/config/otelcol-config.yaml"
OTEL_DST="/etc/jarvis/otelcol-config.yaml"
if [[ -e "${OTEL_DST}" || -L "${OTEL_DST}" ]]; then
  otel_backup="${OTEL_DST}.bak-$(date +%Y%m%d%H%M%S)"
  sudo mv "${OTEL_DST}" "${otel_backup}"
  echo "backed up existing otelcol config to ${otel_backup}"
fi
sudo install -o root -g root -m 0644 "${OTEL_SRC}" "${OTEL_DST}"

# R3-AU3: policy / mcp-gw / event / context は認可境界を担う正当なサービス。
# 各 unit テンプレートが宣言する User=/Group=（専用ユーザー）を正とし、単一の
# JARVIS_USER へ一括置換しない。一括置換すると policy / event / context が
# mcp-gw と同じ実行主体になり、認可サービスの権限分離が崩れる (todo.md R3-AU3)。
# jarvis-migrations.service も User=jarvis を宣言する共有 migration 実行 unit
# なので、同じく置換対象外にする（bws 経由で URL を注入、unit に秘密情報なし）。
# JARVIS_USER はテンプレート側の置換プレースホルダ User=jarvis と
# /home/jarvis パスの解決にだけ使う（opencode / code-server / qdrant / otelcol
# の既存 deploy 手順は従来どおり維持）。
#
# jarvis-migrations.service は Type=oneshot / RemainAfterExit の共有 migration 実行 unit。
# policy / context が After + Wants で引くため個別 enable は不要だが、ExecStart が
# `/usr/local/bin/cargo sqlx migrate run` を使うので sqlx-cli (cargo-sqlx) が必須。
# 未導入なら unit は failed になる。ただし systemd の Wants= は弱い依存であり、
# 失敗を依存元へ伝播しない（fail-open）。policy / context は起動しうるため、
# migration の成否は `systemctl is-active jarvis-migrations` で個別に確認する。
# 真に fail-closed にするなら Requires=+After または ExecStartPre の CHECK が必要だが、
# 今回は記述訂正のみ（選択肢は docs/integration/remaining-tasks.md の MIG 節）。
# 導入手順は docs/integration/backup-restore-runbook.md の MIG 節を参照。
JARVIS_SERVICE_UNITS=(
  jarvis-migrations.service
  jarvis-policy.service
  jarvis-mcp-gw.service
  jarvis-event.service
  jarvis-context.service
)

for unit in \
  "${REPO_DIR}"/infra/systemd/jarvis-migrations.service \
  "${REPO_DIR}"/infra/systemd/jarvis-policy.service \
  "${REPO_DIR}"/infra/systemd/jarvis-mcp-gw.service \
  "${REPO_DIR}"/infra/systemd/jarvis-event.service \
  "${REPO_DIR}"/infra/systemd/jarvis-context.service \
  "${REPO_DIR}"/infra/systemd/qdrant.service \
  "${REPO_DIR}"/infra/systemd/opencode.service \
  "${REPO_DIR}"/infra/systemd/code-server.service \
  "${REPO_DIR}"/infra/systemd/otelcol.service; do
  name="$(basename "${unit}")"
  sed_args=()
  if [[ " ${JARVIS_SERVICE_UNITS[*]} " != *" ${name} "* ]]; then
    # 認可サービス・migration 以外のみ、従来どおり jarvis -> JARVIS_USER へ置換。
    sed_args+=(
      -e "s/^User=jarvis$/User=${JARVIS_USER}/"
      -e "s/^Group=jarvis$/Group=${JARVIS_USER}/"
    )
  fi
  # 認可サービス・migration は User=/Group= をテンプレートの宣言どおり保持する。
  sed_args+=(
    -e "s|/home/jarvis|/home/${JARVIS_USER}|g"
    -e "/^EnvironmentFile=-\/etc\/jarvis\/op.env/d"
    -e "/^Environment=PASSWORD_FILE=/d"
    -e "s/^ProtectHome=yes$/ProtectHome=read-only/"
    -e "s|^ExecStart=/usr/bin/op run --env-file=/etc/jarvis/otel.env -- |ExecStart=|"
  )
  sed "${sed_args[@]}" "${unit}" \
    | sudo tee "/etc/systemd/system/${name}" >/dev/null
done

sudo systemctl daemon-reload

echo "binaries and units installed; create /etc/jarvis/bws.env before starting services"
echo "migrations: install sqlx-cli (cargo-sqlx) on the service PATH before starting policy/context; Wants= will NOT stop them if the migration unit fails"
echo "migration sources: deployed ${MIGRATIONS_SRC} -> ${MIGRATIONS_DST} from commit ${BUILD_COMMIT} (same as binaries); verify with 'systemctl is-active jarvis-migrations'"
echo "start with:"
echo "  sudo systemctl enable --now qdrant opencode jarvis-policy jarvis-mcp-gw jarvis-event jarvis-context"
echo "jarvis-migrations.service is pulled in via Wants= from policy/context (no separate enable needed; Wants= does not propagate a failed unit to its dependants)"
