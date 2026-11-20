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

# R4-D: persistent marker for a mask that THIS script applies to
# jarvis-migrations.service. It must live on persistent storage: a power loss
# clears /run and /tmp but leaves the /etc/systemd/system mask symlink behind.
# A surviving marker therefore means "previous run crashed between mask and
# unmask", never "administrator mask". Content carries PID/token for diagnosis.
MASK_MARKER_DIR="/etc/jarvis"
MASK_MARKER="${MASK_MARKER_DIR}/.selfhost-migrations-mask.marker"

if [[ ! -d "${REPO_DIR}/rust" ]]; then
  echo "repository not found at ${REPO_DIR}" >&2
  exit 1
fi

# I35-A: one-commit distribution guarantee for migrations.
# `sqlx::migrate!()` in jarvis-context embeds rust/migrations into the binary
# at build time, while jarvis-migrations.service applies the same SQL from
# /opt/jarvis/rust/migrations as a pre-flight. The current shared set is
# 001..005: 001_initial, 002_pending_embeddings, 003_audit_decided_by,
# 004_pending_embeddings_retry, 005_pending_embeddings_dead_letter. The set
# embedded in the binary and the *.sql files globbed here must be from the
# same commit. sqlx records a per-migration checksum in _sqlx_migrations, so
# a binary/source mismatch makes context fail at startup ("migration ... was
# previously applied but has been modified"). Every artifact here is built
# from this single checkout: never combine a binary from another commit with
# these migration sources (or vice versa).
# MAINTENANCE: when adding a migration (e.g. 006), update the "through NNN"
# range in ALL unit comments (jarvis-migrations.service, jarvis-policy.service,
# jarvis-context.service), this distribution guard, and the Runbook
# (docs/integration/remaining-tasks.md) IN THE SAME COMMIT. Leaving any of
# them stale makes reviewers trust a range that no longer matches the tree.
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

# I41-C: the --exclude-standard check above deliberately ignores files matched
# by .gitignore, but the glob at the top of this file does NOT: an `*.sql` that
# is both ignored and untracked is still globbed into MIGRATION_SQL_FILES and
# would be installed under BUILD_COMMIT while being absent from HEAD (checksum
# mismatch / unaudited DDL). Adopted policy: positively assert that EVERY
# globbed payload file is tracked in the index, instead of relying on an
# untracked-file listing. Together with the clean-tree gates above (index ==
# HEAD and worktree == index), an index match means the globbed file's content
# is exactly HEAD's content, so the payload set and the reconciliation set are
# the same set. This closes the ignored-file gap without a repo-wide untracked
# gate. --error-unmatch fails closed; both streams are discarded so no file
# name or diff content is printed on refusal.
for sql_file in "${MIGRATION_SQL_FILES[@]}"; do
  if ! git -C "${MIGRATIONS_SRC}" ls-files --error-unmatch -- "${sql_file##*/}" >/dev/null 2>&1; then
    echo "migration SQL in the payload is not tracked in ${REPO_DIR}; refusing to deploy an uncommitted SQL pair" >&2
    exit 1
  fi
done

# R4-B: close the assume-unchanged / skip-worktree bypass. Every clean-tree
# gate above is defined in terms of `git diff`, which deliberately ignores
# index entries flagged assume-unchanged (CE_VALID) or skip-worktree. A tracked
# migration SQL can therefore be rewritten on disk while `git diff --quiet`,
# `git diff --cached --quiet`, `ls-files --error-unmatch` and `ls-files
# --others` all stay green, then be globbed into the payload and published
# under this HEAD's BUILD_COMMIT marker. Two independent layers:
#   (a) reject those index bits themselves on the migration SQL set;
#   (b) compare every globbed file's raw content hash with its HEAD blob,
#       which never consults the index.
# No refusal path prints a file name or any diff content.
SQL_REL_PATHS=()
for sql_file in "${MIGRATION_SQL_FILES[@]}"; do
  SQL_REL_PATHS+=("${sql_file#"${REPO_DIR}/"}")
done

# (a) `git ls-files -v` tags: H = normal cached, h = assume-unchanged,
# S = skip-worktree (some versions emit lowercase s). Only H is acceptable.
sql_index_flags="$(git -C "${REPO_DIR}" ls-files -v -- "${SQL_REL_PATHS[@]}")" || {
  echo "git ls-files -v failed at ${REPO_DIR}; refusing to deploy" >&2
  exit 1
}
while IFS= read -r sql_flag_line; do
  [[ -n "${sql_flag_line}" ]] || continue
  case "${sql_flag_line:0:1}" in
    h|s|S)
      echo "migration SQL carries an assume-unchanged/skip-worktree index bit; refusing to deploy" >&2
      exit 1
      ;;
  esac
done <<< "${sql_index_flags}"

# (b) Authoritative byte-for-byte check against HEAD. `git hash-object --stdin`
# hashes the raw stream with no path attributes / CRLF filter, so no filter can
# disguise tampered content. With pipefail, a failing cat-file (path absent
# from HEAD) fails the whole assignment even though hash-object --stdin would
# otherwise succeed by hashing empty input. Both streams are suppressed so no
# file name or diff content is leaked on refusal.
for sql_file in "${MIGRATION_SQL_FILES[@]}"; do
  sql_rel="${sql_file#"${REPO_DIR}/"}"
  if ! head_sql_hash="$(git -C "${REPO_DIR}" cat-file blob "HEAD:${sql_rel}" 2>/dev/null | git hash-object --stdin 2>/dev/null)"; then
    echo "git cat-file could not read migration SQL from HEAD; refusing to deploy" >&2
    exit 1
  fi
  if [[ -z "${head_sql_hash}" ]]; then
    echo "git cat-file returned no migration SQL content from HEAD; refusing to deploy" >&2
    exit 1
  fi
  if ! wt_sql_hash="$(git hash-object --stdin < "${sql_file}" 2>/dev/null)"; then
    echo "could not hash migration SQL payload; refusing to deploy" >&2
    exit 1
  fi
  if [[ "${wt_sql_hash}" != "${head_sql_hash}" ]]; then
    echo "migration SQL payload differs from HEAD content; refusing to deploy an uncommitted SQL pair" >&2
    exit 1
  fi
done

cd "${REPO_DIR}/rust"
cargo build --release --workspace

# I37-A: stage every payload locally, record both markers, then run ALL
# fallible checks before writing anything under /opt. The old code installed
# the four binaries and the migration SQL first and only then ran the guard,
# so a mismatch exited 1 while leaving new binaries paired with old migration
# sources -- exactly the "half-updated pair" its comment claimed to prevent.
STAGE_DIR="$(mktemp -d)"
# I42-B(a): the deploy window masks jarvis-migrations.service. Always unmask
# on exit so an aborted deploy never leaves a silently masked unit behind;
# the flag is only set when *we* applied the mask (never unmask an admin mask).
cleanup_selfhost() {
  rm -rf "${STAGE_DIR}"
  # I54-B: env 配置セクションに入った後、原子的配置・即時検証が完了する前に失敗
  # した場合は退避 .bak を元位置へ復元し、unit のリテラル既定
  # （egress=example.com）が実効化する窓を閉じる。成功時（mcp_gw_env_done=1）は
  # 何もしない。退避の mv 自体が失敗した場合は元ファイルがまだ元位置にあるため
  # 削除せず、退避に成功した場合のみ .bak から復元する（fail-safe）。
  if [[ "${mcp_gw_env_entered:-0}" -eq 1 && "${mcp_gw_env_done:-0}" -eq 0 ]]; then
    if [[ -n "${mcp_gw_env_tmp:-}" && -e "${mcp_gw_env_tmp}" ]]; then
      sudo rm -f "${mcp_gw_env_tmp}" 2>/dev/null || true
    fi
    if [[ "${mcp_gw_env_backup_made:-0}" -eq 1 ]]; then
      if [[ -n "${mcp_gw_env_backup:-}" && -f "${mcp_gw_env_backup}" ]]; then
        sudo install -o root -g root -m 0600 "${mcp_gw_env_backup}" "${MCP_GW_ENV_DST}" 2>/dev/null || true
      fi
      # 退避设置有無を示すフラグは立ったが .bak が無い場合は元位置を触らない。
    else
      # I56-A (採用方針 A): 退避が無くても env は削除しない。repo の deny-all
      # テンプレート（空 egress + 絶対 BASE_DIR）を root:root 0600 で書き戻し、
      # unit の example.com リテラルが実効化する fail-open 窓を閉じる。
      sudo install -o root -g root -m 0600 "${REPO_DIR}/infra/config/mcp_gateway.env" "${MCP_GW_ENV_DST}" 2>/dev/null || true
    fi
  fi
  if [[ "${mig_unit_masked:-0}" -eq 1 ]]; then
    sudo systemctl unmask "${MIGRATIONS_UNIT:-jarvis-migrations.service}" >/dev/null 2>&1 || true
    # R4-D: remove our marker together with the unmask so a clean exit is not
    # misread as a crash residue on the next run. Best-effort like the unmask.
    sudo rm -f "${MASK_MARKER}" 2>/dev/null || true
  fi
}
trap cleanup_selfhost EXIT
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
# I42-B(b): the binary marker is a 2-part record -- line 1 is the commit,
# following lines are "<sha256>  <binary-name>" in sha256sum format. It is
# published next to the built binaries in rust/target/release/ as well, so
# deploy-mcp-gw-safe.sh (which deploys that exact file) can verify provenance
# with no best-effort fallback. target/ is gitignored, so this write does not
# dirty the tracked tree checked by the gates above.
bin_commit_file="${STAGE_BIN}/.jarvis-build-commit"
mig_commit_file="${STAGE_MIG}/.jarvis-source-commit"
{
  printf '%s\n' "${BUILD_COMMIT}"
  for bin in "${BINARIES[@]}"; do
    bin_sha="$(sha256sum "target/release/${bin}" | cut -d' ' -f1)"
    if [[ -z "${bin_sha}" ]]; then
      echo "sha256sum produced no digest for target/release/${bin}" >&2
      exit 1
    fi
    printf '%s  %s\n' "${bin_sha}" "${bin}"
  done
} > "${bin_commit_file}"
printf '%s\n' "${BUILD_COMMIT}" > "${mig_commit_file}"
install -m 0644 "${bin_commit_file}" "target/release/.jarvis-build-commit"

# Reconciliation boundary: verify the staged payloads and both markers. A
# mismatch, a missing/empty marker or an unusable commit exits here with zero
# binaries and zero migration files installed under /opt.
for marker in "${bin_commit_file}" "${mig_commit_file}"; do
  if [[ ! -s "${marker}" ]]; then
    echo "build marker missing or empty at ${marker}" >&2
    exit 1
  fi
  # Only line 1 is the commit; the binary marker may carry extra sha256 lines.
  IFS= read -r staged_commit < "${marker}" || true
  if [[ "${staged_commit}" != "${BUILD_COMMIT}" ]]; then
    echo "staged commit ${staged_commit} in ${marker} != ${BUILD_COMMIT}; refusing to deploy a mixed pair" >&2
    exit 1
  fi
done
# The sha256 records that deploy-mcp-gw-safe.sh relies on must all be present.
for bin in "${BINARIES[@]}"; do
  if ! grep -Eq "^[0-9a-f]{64}  ${bin}$" "${bin_commit_file}"; then
    echo "missing sha256 record for ${bin} in ${bin_commit_file}" >&2
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

MIGRATIONS_ROOT="/opt/jarvis/rust"
MIGRATIONS_DST="${MIGRATIONS_ROOT}/migrations"
# I42-B(a): adopted policy -- complete, immutable version trees plus an atomic
# symlink swap at the fixed --source path. jarvis-migrations.service keeps
# reading `--source /opt/jarvis/rust/migrations`; that path becomes a symlink
# to a version directory, so any reader that opens it observes either the
# complete old tree or the complete new tree, never a partial one. Unlike the
# former stop/rm/install/start window this does not depend on the reader being
# stopped. In addition the unit is masked for the whole write boundary so a
# policy/context Wants= (or Restart=on-failure) cannot spawn a reader at all.
# Alternative considered and NOT adopted: exchanging two real directories.
# `mv migrations migrations.old && mv staging migrations` has a gap where the
# --source path does not exist, and a true exchange needs
# renameat2(RENAME_EXCHANGE), which this shell cannot call. Versioned trees +
# one `mv -T` on the symlink keep the path contract and give a single atomic
# rename(2) on the stable path.
MIGRATIONS_VERSIONS="${MIGRATIONS_ROOT}/migrations.d"
MIGRATION_TS="$(date +%Y%m%d%H%M%S)"
MIGRATION_VERSION_DIR="${MIGRATIONS_VERSIONS}/${BUILD_COMMIT}-${MIGRATION_TS}-$$"
MIGRATION_LINK_TMP="${MIGRATIONS_ROOT}/.migrations.next-${MIGRATION_TS}-$$"
# I45-D: the binary build marker is published through the SAME atomic symlink
# boundary as the migrations. /opt/jarvis/bin/.jarvis-build-commit becomes a
# symlink to the stable --source path below, so a single rename(2) of
# MIGRATIONS_DST flips both the SQL tree and the binary marker together.
BIN_MARKER_LINK="/opt/jarvis/bin/.jarvis-build-commit"
BIN_MARKER_LINK_TMP="/opt/jarvis/bin/.jarvis-build-commit.next-${MIGRATION_TS}-$$"

# The I38-B stale-SQL property is preserved structurally: every version tree
# is created fresh from the verified STAGED_SQL_FILES set, so an SQL file
# deleted from the checkout cannot survive the swap. The former in-place
# stale-SQL scan/rm is no longer needed -- and would only mutate the old,
# still-live tree.

# All checks passed; deploy from the validated staging set. Payload files go
# in first and the commit markers go in LAST, so the markers act as the seal
# of a completed deploy: if an install aborts mid-way, set -e stops here, the
# "start with:" instructions below are never printed and the previous markers
# no longer describe the mixed on-disk payload. The next successful run
# re-installs the whole set and is idempotent.
# I45-D: both markers are now sealed into the version tree before publication;
# /opt/jarvis/bin/.jarvis-build-commit is a symlink through the same atomic
# MIGRATIONS_DST swap, so the two markers never straddle the deploy boundary.
#
# I40-C / I42-B(a): the old in-place rm+install exposed a partial SQL tree.
# We now build a complete version directory, seal it with the marker, and
# publish it with a single atomic symlink rename; a reader can never see a
# partial tree even without stopping it. The unit is additionally masked for
# the write boundary so Wants= / Restart=on-failure cannot start a reader.
# If any write fails, set -e aborts BEFORE the symlink swap, so the live
# --source path still points at the previous complete tree.
MIGRATIONS_UNIT="jarvis-migrations.service"
mig_unit_was_active=0
if sudo systemctl is-active --quiet "${MIGRATIONS_UNIT}" 2>/dev/null; then
  mig_unit_was_active=1
fi
# R4-D: crash recovery BEFORE classifying the mask. A marker left by a prior
# run means that run applied the mask and died (SIGKILL/power loss) before its
# unmask, so the current mask is our residue -- not an admin decision. Unmask
# it now and fall through to the normal flow below, which re-applies and
# re-marks our own mask for this run. Fail closed if the unmask itself fails:
# keep the marker for the next attempt and abort rather than misclassify a
# still-present mask as an admin mask (which would silently corrupt unit
# writes via tee -> /dev/null).
sudo mkdir -p "${MASK_MARKER_DIR}"
if [[ -e "${MASK_MARKER}" ]]; then
  echo "stale selfhost mask marker found; unmasking residual ${MIGRATIONS_UNIT}" >&2
  if sudo systemctl unmask "${MIGRATIONS_UNIT}" >/dev/null 2>&1; then
    sudo rm -f "${MASK_MARKER}"
  else
    echo "failed to unmask residual ${MIGRATIONS_UNIT}; refusing to continue" >&2
    exit 1
  fi
fi
mig_unit_was_masked=0
# An admin mask is trusted only when no selfhost marker is present.
if [[ "$(sudo systemctl is-enabled "${MIGRATIONS_UNIT}" 2>/dev/null || true)" == "masked" ]]; then
  mig_unit_was_masked=1
fi
mig_unit_masked=0
# Only stop a unit systemd actually knows about. On a first install this script
# generates and loads the unit later (unit loop + daemon-reload below), so a
# `stop` here would fail with "Unit not found" and abort before any write. A
# missing unit means there is no pre-flight that could read the tree mid-swap.
if sudo systemctl cat "${MIGRATIONS_UNIT}" >/dev/null 2>&1; then
  sudo systemctl stop "${MIGRATIONS_UNIT}"
fi
# Mask right after the stop: without this, policy/context (Restart=on-failure)
# pulling in jarvis-migrations.service via Wants= could restart a pre-flight
# reader during the window. An admin-applied mask (no marker) is never undone.
if (( mig_unit_was_masked == 0 )); then
  # R4-D: write the marker BEFORE applying the mask. If SIGKILL/power loss
  # landed between `mask` and the marker write, a mask with no marker would be
  # misread as an admin mask next run and persist forever. The reverse order
  # is safe: a marker with no mask only makes the next run do a harmless,
  # idempotent unmask. Set the flag first so the EXIT trap cleans up even if
  # either operation fails.
  mig_unit_masked=1
  printf 'pid=%s token=%s unit=%s\n' "$$" "$(date +%s)-${RANDOM}" "${MIGRATIONS_UNIT}" \
    | sudo tee "${MASK_MARKER}" >/dev/null
  sudo systemctl mask "${MIGRATIONS_UNIT}" >/dev/null
fi

for bin in "${BINARIES[@]}"; do
  sudo install -o root -g root -m 0755 "${STAGE_BIN}/${bin}" "/opt/jarvis/bin/${bin}"
done
# Build the complete new SQL tree in a version directory (staging -> deploy).
sudo install -d -o root -g root -m 0755 "${MIGRATIONS_VERSIONS}"
sudo install -d -o root -g root -m 0755 "${MIGRATION_VERSION_DIR}"
sudo install -o root -g root -m 0644 "${STAGED_SQL_FILES[@]}" "${MIGRATION_VERSION_DIR}/"
# Marker LAST inside this version tree: it is sealed only once the source
# marker exists, before the tree is published.
sudo install -o root -g root -m 0644 "${mig_commit_file}" "${MIGRATION_VERSION_DIR}/.jarvis-source-commit"

# I45-D: the binary build marker now lives in the SAME sealed version tree, so
# it is published atomically with the SQL sources by the single MIGRATIONS_DST
# swap below. Previously it was installed into /opt/jarvis/bin/ only AFTER the
# swap, exposing a window where the migration side reported the new commit while
# the bin side still reported the old one. Keeping both markers in one tree also
# makes a later prune-tripwire failure harmless: by then both markers already
# resolve to the new commit, so there is no stale-bin-marker half-applied state.
sudo install -o root -g root -m 0644 "${bin_commit_file}" "${MIGRATION_VERSION_DIR}/.jarvis-build-commit"

# I45-D: (re)point /opt/jarvis/bin/.jarvis-build-commit at the stable --source
# path BEFORE the swap. The symlink always targets MIGRATIONS_DST (never this
# version dir), so it never needs rewriting on later deploys and its resolved
# content flips exactly when the atomic swap below runs. A legacy regular-file
# marker is replaced here via ln -sfn + mv -Tf (one atomic rename in the same
# directory); until the swap the old tree's marker (or, on the first upgrade
# from a legacy layout, a briefly dangling target) is seen -- a missing marker
# is fail-closed and never a wrong-commit mismatch.
sudo ln -sfn "${MIGRATIONS_DST}/.jarvis-build-commit" "${BIN_MARKER_LINK_TMP}"
sudo mv -Tf "${BIN_MARKER_LINK_TMP}" "${BIN_MARKER_LINK}"

# Publish with one atomic rename(2). `ln -s` + `mv -T` replaces the symlink at
# the stable --source path atomically, so readers see a complete tree either
# way. A legacy real directory is moved aside once (the mask makes that one
# non-atomic step safe); subsequent runs always hit the symlink branch.
sudo ln -sfn "${MIGRATION_VERSION_DIR}" "${MIGRATION_LINK_TMP}"
if [[ -d "${MIGRATIONS_DST}" && ! -L "${MIGRATIONS_DST}" ]]; then
  sudo mv "${MIGRATIONS_DST}" "${MIGRATIONS_ROOT}/migrations.legacy-${MIGRATION_TS}"
fi
sudo mv -Tf "${MIGRATION_LINK_TMP}" "${MIGRATIONS_DST}"
# I45-D: no separate post-swap marker install here. The binary marker is sealed
# into the version tree above and published by the atomic swap, so both markers
# cross the deploy boundary at the same instant as the SQL sources.

# Keep the 3 newest version trees as immutable snapshots for manual rollback.
#
# I43-A: harden the prune so it can never delete the tree the --source
# symlink currently publishes. The old
#   find ... -printf '%f\n' | sort | head -n -3
# ordered candidates by *filename*. A version dir is named
# ${BUILD_COMMIT}-${MIGRATION_TS}-$$, so the SHA prefix dominated that lexical
# order: after deploying a different commit the active tree could sort into
# the "oldest 3" and `rm -rf` removed it immediately after `mv -Tf`, leaving
# /opt/jarvis/rust/migrations as a dangling symlink. Three independent layers:
#   (A) order by mtime, not by name;
#   (B) explicitly exclude the resolved --source symlink target;
#   (C) re-verify each candidate is not that target before rm and fail closed
#       (abort the whole prune) if the invariant is ever violated.
# No other deploy behaviour changes: the atomic symlink swap, mask/unmask
# window, clean-tree gate, provenance markers, privilege model and the 3-tree
# rollback snapshots are all preserved.

# Resolve the tree published by the symlink. This runs after the atomic swap,
# so it names the tree we just deployed (not the previous one). Fail closed on
# a resolution error: pruning without knowing the live target is unsafe.
active_migrations_version=""
if [[ -L "${MIGRATIONS_DST}" ]]; then
  active_target="$(sudo readlink -f -- "${MIGRATIONS_DST}")" || {
    echo "readlink -f failed on ${MIGRATIONS_DST}; refusing to prune version trees" >&2
    exit 1
  }
  active_migrations_version="$(basename -- "${active_target}")"
  if [[ -z "${active_migrations_version}" ]]; then
    echo "could not resolve a version name from ${MIGRATIONS_DST}; refusing to prune" >&2
    exit 1
  fi
fi

# (A) mtime order, oldest first. `%T@` is the modification epoch
# (seconds.nanoseconds), `%f` the bare dir name; `-k1,1n` compares the epoch
# numerically and `-k2,2` breaks same-second ties deterministically by name.
# `head -n -3` keeps the 3 newest snapshots; `cut` drops the timestamp column
# so only directory names remain. LC_ALL=C makes numeric parsing deterministic.
mapfile -t prune_versions < <(
  sudo find "${MIGRATIONS_VERSIONS}" -mindepth 1 -maxdepth 1 -type d -printf '%T@ %f\n' \
    | LC_ALL=C sort -k1,1n -k2,2 \
    | head -n -3 \
    | cut -d' ' -f2-
)

# (C) fail-closed tripwire over the whole candidate set. If the active tree is
# ever selected for pruning (e.g. mtime skew, or a future refactor reintroducing
# lexical order), delete nothing and abort rather than risk a dangling symlink.
# The EXIT trap still unmasks the unit if this script applied the mask.
if (( ${#prune_versions[@]} > 0 )); then
  for pv in "${prune_versions[@]}"; do
    if [[ -n "${active_migrations_version}" && "${pv}" == "${active_migrations_version}" ]]; then
      echo "refusing to prune: ${pv} is the current symlink target of ${MIGRATIONS_DST}" >&2
      exit 1
    fi
  done
fi

# (B) explicit exclusion: build the final deletion list without the active
# tree. Redundant with (C) on purpose (defence in depth) but keeps the intent
# local: the live tree is never even an `rm` candidate.
prune_candidates=()
for pv in "${prune_versions[@]}"; do
  if [[ -n "${active_migrations_version}" && "${pv}" == "${active_migrations_version}" ]]; then
    continue
  fi
  prune_candidates+=("${pv}")
done

for old_version in "${prune_candidates[@]}"; do
  sudo rm -rf -- "${MIGRATIONS_VERSIONS}/${old_version}"
done

# Unmask BEFORE the unit-generation loop rewrites the unit file below: a
# lingering mask symlink at /etc/systemd/system/${name} would make `tee`
# follow it to /dev/null and silently discard the generated unit. R4-D: drop
# our marker at the same point, since the write boundary is now closed. An
# admin mask (no marker, mig_unit_masked == 0) is left untouched here; the
# fail-closed assert in the unit loop below catches it instead.
if (( mig_unit_masked )); then
  sudo systemctl unmask "${MIGRATIONS_UNIT}" >/dev/null
  mig_unit_masked=0
  sudo rm -f "${MASK_MARKER}"
fi
# The write boundary is sealed, so restore the pre-flight unit to its prior
# active state and let it apply the new SQL. On a fresh install the unit was
# unknown/inactive above, so it stays stopped and is started by the normal
# `systemctl enable --now ...` flow at the end of this script -- this script
# still never starts services on its own beyond restoring a previously active
# unit.
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

# I52-B / I54-B: jarvis-mcp-gw.service は EnvironmentFile=-/etc/jarvis/jarvis-mcp-gw.env
# を optional で読む。この配布経路が無いと unit のリテラル既定値
# （egress=example.com / 開発用 BASE_DIR）が実効化し、deny-all のつもりが
# example.com 許可になる fail-open を起こす。旧コードの mv(退避) -> install(配置) は
# 非アトミックで、mv 成功後に install が失敗すると set -e で中止されるだけで env が
# 欠落していた。I54-B では一時パスへ root:root 0600 で install してから同一ディレクトリ
# の mv -f で原子的に置換し、失敗時は EXIT trap が .bak を元位置へ復元する。退避
# ファイルも元パーミッション（例 0644）を継承しないよう明示 0600 にする。配置直後に
# は drift 検査（--env-only）を即時実行し、DANGER なら導入失敗として復元させる。
MCP_GW_ENV_SRC="${REPO_DIR}/infra/config/mcp_gateway.env"
MCP_GW_ENV_DST="/etc/jarvis/jarvis-mcp-gw.env"
# I54-B: 以降の失敗を EXIT trap が拾えるよう、退避 mv の前に entered を立てる。
mcp_gw_env_entered=1
mcp_gw_env_done=0
mcp_gw_env_backup=""
mcp_gw_env_backup_made=0
mcp_gw_env_tmp=""
if [[ -e "${MCP_GW_ENV_DST}" || -L "${MCP_GW_ENV_DST}" ]]; then
  mcp_gw_env_backup="${MCP_GW_ENV_DST}.bak-$(date +%Y%m%d%H%M%S)"
  sudo mv "${MCP_GW_ENV_DST}" "${mcp_gw_env_backup}"
  # I54-B: mv は元パーミッションを継承するため明示的に 0600 へ。
  sudo chmod 0600 "${mcp_gw_env_backup}"
  mcp_gw_env_backup_made=1
  echo "backed up existing mcp-gw env to ${mcp_gw_env_backup}"
fi
# 同一ディレクトリの一時ファイルへ staged してから mv -f で原子的に置換する。
mcp_gw_env_tmp="$(sudo mktemp "${MCP_GW_ENV_DST}.tmp.XXXXXX")" || {
  echo "failed to create temp file for mcp-gw env" >&2
  exit 1
}
if ! sudo install -o root -g root -m 0600 "${MCP_GW_ENV_SRC}" "${mcp_gw_env_tmp}"; then
  echo "failed to stage mcp-gw env to temp file" >&2
  exit 1
fi
if ! sudo mv -f "${mcp_gw_env_tmp}" "${MCP_GW_ENV_DST}"; then
  echo "failed to atomically install mcp-gw env" >&2
  exit 1
fi
mcp_gw_env_tmp=""
# I54-B: 配置直後の即時 drift 検査。--env-only でサービス起動状態に依存せず
# env 自体（最終割り当ての BASE_DIR / egress キー）を検証する。DANGER なら
# 導入失敗として EXIT trap に .bak の復元を任せる。
if ! JARVIS_DRIFT_ENV="${MCP_GW_ENV_DST}" bash "${REPO_DIR}/infra/scripts/check-deploy-drift.sh" --env-only; then
  echo "post-install drift check(DANGER) for mcp-gw env; rolling back" >&2
  exit 1
fi
mcp_gw_env_done=1

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
  # R4-D: fail-closed unit-write guard. `tee` follows a mask symlink
  # (/etc/systemd/system/<name> -> /dev/null), silently discarding the
  # generated unit while the old one stays masked/stale and the script still
  # reports success. Never write through such a destination: abort loudly.
  unit_dst="/etc/systemd/system/${name}"
  if [[ -L "${unit_dst}" ]]; then
    unit_link_target="$(sudo readlink -f -- "${unit_dst}" 2>/dev/null || true)"
    if [[ -z "${unit_link_target}" ]]; then
      unit_link_target="$(sudo readlink -- "${unit_dst}" 2>/dev/null || true)"
    fi
    if [[ "${unit_link_target}" == "/dev/null" ]]; then
      echo "refusing to write ${unit_dst}: destination is a mask symlink to /dev/null; unmask the unit first" >&2
      exit 1
    fi
  fi
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
