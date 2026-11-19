#!/usr/bin/env bash
# Local-only JARVIS backup (ADR-0004; BK-1/BK-2 redesign).
# Runs as the postgres OS user so `pg_dump -d jarvis` works via peer auth.
# No OCI, rclone, 1Password, or secret values are read. Default is dry-run.
# The jarvis git repo is archived by default (--skip-git opts out); only
# committed, tracked HEAD content is read, never untracked secrets or .git/config.
set -euo pipefail

backup_dir="/var/lib/postgresql/jarvis-backups"
apply=0
retain_days=""
skip_db=0
skip_qdrant=0
skip_git=0
repo_dir=""

usage() {
  cat <<'USAGE'
Usage: backup-local.sh [--backup-dir DIR] [--apply] [--retain-days N]
                       [--skip-db] [--skip-qdrant] [--skip-git] [--repo-dir DIR]
  Default is a non-destructive dry run. --apply performs the backup.
  --repo-dir DIR  absolute path to the jarvis git working tree to archive
                  (default: --backup-dir if it is a repo, else the repo
                  containing this script). Git is included unless --skip-git.
USAGE
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --backup-dir) backup_dir="$2"; shift 2 ;;
    --apply) apply=1; shift ;;
    --retain-days)
      [ "$#" -ge 2 ] || { echo "ERROR: --retain-days requires a value" >&2; exit 2; }
      case "$2" in
        ''|*[!0-9]*) echo "ERROR: --retain-days must be a positive integer" >&2; exit 2 ;;
      esac
      [ "$2" -gt 0 ] || { echo "ERROR: --retain-days must be greater than zero" >&2; exit 2; }
      retain_days="$2"; shift 2 ;;
    --skip-db) skip_db=1; shift ;;
    --skip-qdrant) skip_qdrant=1; shift ;;
    --skip-git) skip_git=1; shift ;;
    --repo-dir|--git-repo)
      [ "$#" -ge 2 ] || { echo "ERROR: --repo-dir requires a value" >&2; exit 2; }
      case "$2" in
        /*) ;;
        *) echo "ERROR: --repo-dir must be an absolute path" >&2; exit 2 ;;
      esac
      repo_dir="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "ERROR: unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

# Resolve the jarvis git working tree. All queries are read-only git metadata.
# Order: explicit --repo-dir, --backup-dir if it is a repo, else the repo that
# contains this script. This keeps the existing timer invocation (which only
# passes --backup-dir and --apply) valid without changing the unit.
resolve_repo_dir() {
  local base work
  if [ -n "$repo_dir" ]; then
    printf '%s\n' "$repo_dir"
    return 0
  fi
  if [ -d "$backup_dir" ]; then
    work="$(git -C "$backup_dir" rev-parse --show-toplevel 2>/dev/null)" || work=""
    if [ -n "$work" ]; then
      printf '%s\n' "$work"
      return 0
    fi
  fi
  base="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." 2>/dev/null && pwd -P)" || return 1
  work="$(git -C "$base" rev-parse --show-toplevel 2>/dev/null)" || return 1
  [ -n "$work" ] || return 1
  printf '%s\n' "$work"
}

# Fail closed on a non-absolute path, a missing directory, or a non-repo.
validate_repo_dir() { # path -> resolved toplevel on stdout
  local path="$1" top
  case "$path" in
    /*) ;;
    *) echo "ERROR: git repository path must be absolute: $path" >&2; return 1 ;;
  esac
  [ -d "$path" ] || { echo "ERROR: git repository directory not found: $path" >&2; return 1; }
  [ "$(git -C "$path" rev-parse --is-inside-work-tree 2>/dev/null)" = "true" ] \
    || { echo "ERROR: not a git working tree: $path" >&2; return 1; }
  top="$(git -C "$path" rev-parse --show-toplevel 2>/dev/null)" || return 1
  [ -e "$top/.git" ] || { echo "ERROR: no .git metadata in $top" >&2; return 1; }
  printf '%s\n' "$top"
}

stamp="$(date +%Y%m%d-%H%M%S)"
host="$(hostname)"
set_dir="${backup_dir}/${host}-${stamp}"

echo "JARVIS local backup: dest=${set_dir} apply=${apply}"

if [ "$apply" -ne 1 ]; then
  echo "DRY RUN: no files will be written (pass --apply to execute)"
  [ "$skip_db" -eq 0 ] && echo "DRY RUN: would dump PostgreSQL 'jarvis'"
  [ "$skip_qdrant" -eq 0 ] && echo "DRY RUN: would snapshot Qdrant 'jarvis_memories'"
  if [ "$skip_git" -eq 0 ]; then
    dry_repo="$(resolve_repo_dir || true)"
    echo "DRY RUN: would backup git repo (path=${dry_repo:-<auto-detect>})"
  else
    echo "DRY RUN: would skip git repo (--skip-git)"
  fi
  echo "DRY RUN: would copy non-secret config allowlist"
  exit 0
fi

# Resolve and validate the git source before any write, so a bad path cannot
# leave a partial backup set behind.
git_top=""
if [ "$skip_git" -eq 0 ]; then
  git_top="$(resolve_repo_dir || true)"
  if [ -n "$git_top" ]; then
    git_top="$(validate_repo_dir "$git_top")"
  elif [ -n "$repo_dir" ]; then
    echo "ERROR: cannot resolve --repo-dir $repo_dir" >&2
    exit 1
  else
    # The installed unit runs from /opt/jarvis/scripts where no repo is nearby;
    # warn + skip so the existing timer still completes without re-application.
    echo "WARN: jarvis git repo not found; skipping git archive" >&2
    echo "WARN: pass --repo-dir ABS_DIR to include it, or --skip-git to silence" >&2
  fi
else
  echo "Git repo skipped (--skip-git)"
fi

mkdir -p "$set_dir"
chmod 0700 "$set_dir"

manifest="$set_dir/manifest.txt"
# Header line carries the creation time for precise, opt-in retention.
printf '# created_at=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$manifest"

record() { # name path
  local name="$1" path="$2" bytes sha
  bytes="$(wc -c < "$path" | tr -d ' ')"
  sha="$(sha256sum "$path" | cut -d' ' -f1)"
  printf '%s %s %s\n' "$name" "$bytes" "$sha" >> "$manifest"
}

if [ "$skip_db" -eq 0 ]; then
  pg_dump -d jarvis | gzip > "$set_dir/postgres-jarvis.sql.gz"
  record postgres-jarvis.sql.gz "$set_dir/postgres-jarvis.sql.gz"
  echo "PostgreSQL dump OK"
fi

if [ "$skip_qdrant" -eq 0 ]; then
  curl -fsS -X POST "http://127.0.0.1:6333/collections/jarvis_memories/snapshots" >/dev/null
  sleep 2
  snapshot_name="$(curl -fsS "http://127.0.0.1:6333/collections/jarvis_memories/snapshots" \
    | jq -r '.result[0].name // empty')"
  if [ -z "$snapshot_name" ]; then
    echo "ERROR: no Qdrant snapshot returned" >&2
    exit 1
  fi
  curl -fsS "http://127.0.0.1:6333/collections/jarvis_memories/snapshots/${snapshot_name}" \
    > "$set_dir/qdrant-snapshot.bin"
  record qdrant-snapshot.bin "$set_dir/qdrant-snapshot.bin"
  echo "Qdrant snapshot OK"
fi

# Non-secret config allowlist. Never include .env / bws.env / token files.
# Explicit files only: a recursive tar of /etc/jarvis/policy could capture
# unreviewed files. otelcol-config.yaml is intentionally excluded because it
# may carry credentials (e.g. Grafana Cloud auth). -f (not -e) also skips a
# same-named directory instead of archiving it.
for source in /etc/jarvis/mcp_allowlist.toml /etc/jarvis/pii_patterns.toml \
              /etc/jarvis/policy/casbin_model.conf \
              /etc/jarvis/policy/casbin_policy.csv \
              /etc/jarvis/policy/cedar_schema.cedarschema \
              /etc/jarvis/policy/policies.cedar; do
  if [ -f "$source" ]; then
    name="config-$(basename "$source")"
    cp "$source" "$set_dir/$name"
    record "$name" "$set_dir/$name"
  fi
done
echo "Config copy OK"
# NOTE: the secret scan in test-verify-backup.sh cannot inspect strings inside
# compressed payloads (gzip dumps / tar snapshots). The explicit allowlist
# above is the primary control; do not add whole-directory archives here.
# The git archive below is a reviewed exception: it reads only committed,
# tracked content from the object DB, never a live directory tree.

# Git repository (closes the "Git bundles" gap in infra/README.md).
# git archive is read-only against the source repo and never includes .git/
# (so .git/config credentials are excluded) or untracked .env / *.key files.
if [ -n "$git_top" ]; then
  archive="$set_dir/git-jarvis-head.tar.gz"
  archive_tmp="$archive.tmp"
  git -C "$git_top" archive --format=tar HEAD | gzip > "$archive_tmp"
  # Fail closed if a tracked secret-like path ever enters the committed tree.
  bad_paths="$(tar -tzf "$archive_tmp" 2>/dev/null \
    | grep -Ei '(^|/)\.env$|\.key$|(^|/)\.git(/|$)' || true)"
  if [ -n "$bad_paths" ]; then
    echo "ERROR: git archive contains secret-like paths; refusing it" >&2
    rm -f "$archive_tmp"
    exit 1
  fi
  mv -f "$archive_tmp" "$archive"
  record git-jarvis-head.tar.gz "$archive"
  echo "Git repo archive OK: $git_top"
fi

chmod 0700 "$set_dir"
# Completion marker: only written after every step succeeded. The verifier
# uses this to reject partially-written sets from a failed run.
printf 'complete %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$set_dir/COMPLETE"
echo "Backup complete: $set_dir"

if [ -n "$retain_days" ]; then
  echo "Retention: ${retain_days} days (opt-in, destructive)"
  cutoff_ts="$(( $(date +%s) - retain_days * 86400 ))"
  while IFS= read -r old; do
    # Prefer the manifest timestamp; fall back to the <host>-<YYYYMMDD-HHMMSS>
    # directory name used by this script and the earlier Rust implementation.
    created="$(sed -n 's/^# created_at=//p' "$old/manifest.txt" 2>/dev/null | head -1)"
    if [ -n "$created" ]; then
      epoch="$(date -u -d "$created" +%s 2>/dev/null || echo 0)"
    else
      base="$(basename "$old")"
      stamp="${base##*-}"
      epoch="$(date -d "${stamp:0:8} ${stamp:9:2}:${stamp:11:2}:${stamp:13:2}" +%s 2>/dev/null || echo 0)"
    fi
    if [ "$epoch" -gt 0 ] && [ "$epoch" -lt "$cutoff_ts" ]; then
      echo "PRUNE: $old (created epoch=$epoch)"
      rm -rf "$old"
    fi
  done < <(find "$backup_dir" -mindepth 1 -maxdepth 1 -type d | sort)
fi
