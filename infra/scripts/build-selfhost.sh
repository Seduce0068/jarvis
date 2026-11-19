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

# I39-C: HEAD alone is not provenance. The rev-parse above names a commit, but
# without a clean-tree gate the built binaries and the globbed
# `rust/migrations/*.sql` can still contain uncommitted edits while both
# markers record that HEAD; the reconciliation below then compares BUILD_COMMIT
# with itself and always passes. Make a clean tracked-file tree a mandatory
# pre-build gate so the pair is genuinely reproducible from HEAD.
# `git diff --quiet` covers unstaged edits to tracked files and
# `git diff --cached --quiet` covers staged edits; both fail closed here.
# --quiet prints no diff/patch, so no change contents are leaked on refusal.
if ! git -C "${REPO_DIR}" diff --quiet; then
  echo "working tree has unstaged changes at ${REPO_DIR}; refusing to build/deploy an uncommitted pair" >&2
  exit 1
fi
if ! git -C "${REPO_DIR}" diff --cached --quiet; then
  echo "index has staged changes at ${REPO_DIR}; refusing to build/deploy an uncommitted pair" >&2
  exit 1
fi

# I39-C: untracked policy -- deliberately NOT a repo-wide untracked gate.
# Generated products (target/, generated configs, local scratch files) often
# remain untracked, so a global `git status --porcelain` / `git ls-files
# --others` check would fail every build on an otherwise clean checkout. The
# narrow exception is the migration source tree: those *.sql files are globbed
# directly from the working tree into the payload, so one untracked
# `rust/migrations/*.sql` would be deployed under this HEAD's markers and break
# the one-commit guarantee. Check untracked files only there, excluding ignored
# files via --exclude-standard, and capture the result without printing it.
untracked_in_migrations="$(git -C "${MIGRATIONS_SRC}" ls-files --others --exclude-standard -- .)" || {
  echo "git ls-files failed at ${MIGRATIONS_SRC}; refusing to deploy" >&2
  exit 1
}
if [[ -n "${untracked_in_migrations}" ]]; then
  echo "untracked files in ${MIGRATIONS_SRC}; refusing to deploy an uncommitted SQL pair" >&2
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
# single-checkout provenance, plus the fail-closed rev-parse and the clean-tree
# gate above, is what makes the binary/migration pair a one-commit pair.
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

# I38-B: reconcile the destination *.sql filename set against this checkout's
# staging set before installing the new payload. `sudo install` only adds or
# overwrites; it never removes a migration that was deleted from the source.
# Such a stale file would still be applied by jarvis-migrations.service's
# pre-flight (`--source ${MIGRATIONS_DST}`) while being absent from the
# context binary's build-time `sqlx::migrate!()` set, producing a checksum /
# unknown-version mismatch and a startup failure. Compute the stale set with
# a read-only listing here (before any /opt write), fail closed on an
# unexpected directory, and delete the extras explicitly in the deploy step
# below -- before the new SQL and the markers are installed.
STALE_SQL_FILES=()
shopt -s nullglob
for dst_sql in "${MIGRATIONS_DST}"/*.sql; do
  dst_name="${dst_sql##*/}"
  keep=0
  for staged_sql in "${STAGED_SQL_FILES[@]}"; do
    if [[ "${staged_sql##*/}" == "${dst_name}" ]]; then
      keep=1
      break
    fi
  done
  if (( keep == 0 )); then
    if [[ -d "${dst_sql}" && ! -L "${dst_sql}" ]]; then
      echo "stale migration path is a directory, refusing to delete: ${dst_sql}" >&2
      exit 1
    fi
    STALE_SQL_FILES+=("${dst_sql}")
  fi
done
shopt -u nullglob

# All checks passed; deploy from the validated staging set. Payload files go
# in first and the commit markers go in LAST, so the markers act as the seal
# of a completed deploy: if an install aborts mid-way, set -e stops here, the
# "start with:" instructions below are never printed and the previous markers
# no longer describe the mixed on-disk payload. The next successful run
# re-installs the whole set and is idempotent.
#
# I40-C: close the stale-SQL `sudo rm -f` -> new-SQL `sudo install` race.
# jarvis-migrations.service is a oneshot pre-flight whose ExecStart reads SQL
# directly from ${MIGRATIONS_DST} (`--source /opt/jarvis/rust/migrations`).
# During the rm+install window below that directory is incomplete, and any
# (re)start of the unit in that window would read a partial tree; marker-only
# post-hoc detection cannot prevent that. Adopted approach: STOP the unit for
# the whole /opt write boundary, do the rm+install, write the markers LAST, and
# restore the unit afterwards. If any write fails, set -e aborts before the
# restore, so the unit stays stopped and the next run re-installs the complete
# set -- no reader can ever observe a partial tree. Old SQL is deliberately NOT
# restored on failure (a restore would add its own failure window); the whole
# deploy is already idempotent, so leaving the unit stopped and delegating to
# the next run is the safe failure policy.
# Alternative considered and NOT adopted: staging a complete tree in a sibling
# directory and switching it in with rename. `mv migrations migrations.old &&
# mv staging migrations` is two renames with a gap where `--source` does not
# exist (not truly atomic); a real exchange needs renameat2(RENAME_EXCHANGE),
# which this shell cannot call, and pointing --source at a versioned path or a
# symlink would break the unit's path contract. Stopping the reader also avoids
# the sync/durability concerns a directory swap introduces. The --source path
# is left unchanged either way.
MIGRATIONS_UNIT="jarvis-migrations.service"
mig_unit_was_active=0
if sudo systemctl is-active --quiet "${MIGRATIONS_UNIT}"; then
  mig_unit_was_active=1
fi
# Only stop a unit systemd actually knows about. On a first install this script
# generates and loads the unit later (unit loop + daemon-reload below), so a
# `stop` here would fail with "Unit not found" and abort before any write. A
# missing unit means there is no pre-flight that could read the tree mid-swap.
if sudo systemctl cat "${MIGRATIONS_UNIT}" >/dev/null 2>&1; then
  sudo systemctl stop "${MIGRATIONS_UNIT}"
fi

for bin in "${BINARIES[@]}"; do
  install -m 755 "${STAGE_BIN}/${bin}" "/opt/jarvis/bin/${bin}"
done
sudo install -d -o root -g root -m 0755 "${MIGRATIONS_DST}"
# I38-B: explicitly remove migrations that are not in this checkout, then
# install the current set and finally the markers. Keeping the delete inside
# this write boundary means a failure aborts before the markers can seal a
# mixed new/old migration tree.
if (( ${#STALE_SQL_FILES[@]} > 0 )); then
  echo "removing stale migration SQL not present in this checkout from ${MIGRATIONS_DST}:" >&2
  printf '  %s\n' "${STALE_SQL_FILES[@]}" >&2
  sudo rm -f -- "${STALE_SQL_FILES[@]}"
fi
sudo install -o root -g root -m 0644 "${STAGED_SQL_FILES[@]}" "${MIGRATIONS_DST}/"
sudo install -o root -g root -m 0644 "${bin_commit_file}" /opt/jarvis/bin/.jarvis-build-commit
sudo install -o root -g root -m 0644 "${mig_commit_file}" "${MIGRATIONS_DST}/.jarvis-source-commit"

# I40-C: the write boundary is sealed (SQL filename set complete, both markers
# written), so restore the pre-flight unit to its prior active state and let it
# apply the new SQL. On a fresh install the unit was unknown/inactive above, so
# it stays stopped and is started by the normal `systemctl enable --now ...`
# flow at the end of this script -- this script still never starts services on
# its own beyond restoring a previously active unit.
if (( mig_unit_was_active )); then
  sudo systemctl start "${MIGRATIONS_UNIT}"
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
