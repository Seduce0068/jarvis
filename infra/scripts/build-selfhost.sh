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
BUILD_COMMIT="$(git -C "${REPO_DIR}" rev-parse HEAD 2>/dev/null || echo unknown)"

cd "${REPO_DIR}/rust"
cargo build --release --workspace

for bin in jarvis-policy jarvis-mcp-gw jarvis-event jarvis-context; do
  install -m 755 "target/release/${bin}" "/opt/jarvis/bin/${bin}"
done

# I35-A / I36-A: record the commit that produced the binaries, then distribute
# the migration sources from this SAME checkout. Previously a missing marker
# only produced a warning (fail-open), so a fresh install could end up with
# jarvis-migrations.service --source pointing at a directory with no SQL.
# Now the source directory, its *.sql files and the marker are installed here;
# every missing input is fatal, so a fresh install always has usable SQL.
bin_commit_file="/opt/jarvis/bin/.jarvis-build-commit"
printf '%s\n' "${BUILD_COMMIT}" | sudo tee "${bin_commit_file}" >/dev/null

MIGRATIONS_DST="/opt/jarvis/rust/migrations"
sudo install -d -o root -g root -m 0755 "${MIGRATIONS_DST}"
sudo install -o root -g root -m 0644 "${MIGRATION_SQL_FILES[@]}" "${MIGRATIONS_DST}/"
mig_commit_file="${MIGRATIONS_DST}/.jarvis-source-commit"
printf '%s\n' "${BUILD_COMMIT}" | sudo tee "${mig_commit_file}" >/dev/null

# Fail-fast verification (replaces the former warn-only branch): the marker
# must exist, be non-empty and match the binary commit before units are
# installed, so a half-updated binary/source pair is never left behind.
if [[ ! -s "${mig_commit_file}" ]]; then
  echo "migration source marker missing or empty at ${mig_commit_file}" >&2
  exit 1
fi
mig_commit="$(<"${mig_commit_file}")"
if [[ "${mig_commit}" != "${BUILD_COMMIT}" ]]; then
  echo "migration source commit ${mig_commit} != binary commit ${BUILD_COMMIT}; deploy both from the same commit" >&2
  exit 1
fi

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
