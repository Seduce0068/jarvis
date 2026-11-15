#!/usr/bin/env bash
# OCI jarvis-host の削除手順（既定は乾燥実行。--apply で実行）
# NOTE: OCI は使用しない方針（ADR-0004）。本スクリプトは過去に作成した OCI
#       リソースの後始末専用レガシーであり、新規プロビジョニングには使わない。
#
# 前提:
#   - OpenTofu (tofu) がインストール済み
#   - OCI CLI が設定済み（oci setup config）
#   - infra/opentofu/environments/prod で tofu init 済み
#   - OCI側のObject Storage/Block Volumeに残存データがないことを確認済み
set -euo pipefail

apply=0
[ "${1:-}" = "--apply" ] && apply=1
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../.." && pwd)"
env_dir="$repo_root/infra/opentofu/environments/prod"
ts="$(date +%Y%m%d-%H%M%S)"

die() { echo "ERROR: $*" >&2; exit 1; }
TOFU=$(command -v tofu 2>/dev/null || true)
[ -z "$TOFU" ] && [ -x /snap/bin/tofu ] && TOFU=/snap/bin/tofu
[ -n "$TOFU" ] || die "tofu が見つかりません（OpenTofuを導入してください）"
echo "  tofu: $TOFU"

[ -d "$env_dir" ] || die "環境ディレクトリがありません: $env_dir"

cd "$env_dir"
echo "== 1. init =="
"$TOFU" init -input=false >/dev/null

echo "== 2. state確認 =="
state_list="$("$TOFU" state list 2>/dev/null || true)"
if [ -z "$state_list" ]; then
  echo "ERROR: state が空です。" >&2
  echo "  考えられる原因:" >&2
  echo "   - terraform.tfvars が無い（prod/terraform.tfvars.example から作成）" >&2
  echo "   - terraform.tfstate が別の場所/失われている（OCI側に未追跡リソースが残っている可能性）" >&2
  echo "  → OCIコンソール/CLIで実リソースを確認し、必要なら import してから再実行してください。" >&2
  echo "  → 未追跡なら手動削除（OCIコンソール推奨）。A1 Free枠の再取得不可に注意。" >&2
  exit 1
fi
echo "$state_list" | head -n 20 | sed 's/^/  /'
count="$(echo "$state_list" | wc -l)"
echo "  リソース数: $count"

echo "== 3. 最終stateバックアップ =="
backup="$HOME/jarvis-oci-state-$ts.json"
"$TOFU" state pull > "$backup"
chmod 600 "$backup"
echo "  $backup"

echo "== 4. destroy plan =="
plan="destroy-$ts.tfplan"
"$TOFU" plan -destroy -out "$plan"
echo "  plan: $env_dir/$plan"

if [ "$apply" -eq 1 ]; then
  echo "== 5. 適用（OCIリソースを削除） =="
  "$TOFU" apply "$plan"
  echo "完了。Tailscale管理コンソールから jarvis-host を削除してください。"
else
  echo "乾燥実行のみ。実行するには --apply を付けてください。"
  echo "  bash $script_dir/decommission-oci.sh --apply"
fi
