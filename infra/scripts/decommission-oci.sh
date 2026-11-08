#!/usr/bin/env bash
# OCI jarvis-host の削除手順（既定は乾燥実行。--apply で実行）
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
command -v tofu >/dev/null || die "tofu が見つかりません（OpenTofuを導入してください）"
[ -d "$env_dir" ] || die "環境ディレクトリがありません: $env_dir"

cd "$env_dir"
echo "== 1. init =="
tofu init -input=false >/dev/null

echo "== 2. state確認 =="
state_list="$(tofu state list 2>/dev/null || true)"
[ -n "$state_list" ] || die "state が空です（対象が無いか、state参照先が違います）"
echo "$state_list" | head -n 20 | sed 's/^/  /'
count="$(echo "$state_list" | wc -l)"
echo "  リソース数: $count"

echo "== 3. 最終stateバックアップ =="
backup="$HOME/jarvis-oci-state-$ts.json"
tofu state pull > "$backup"
chmod 600 "$backup"
echo "  $backup"

echo "== 4. destroy plan =="
plan="destroy-$ts.tfplan"
tofu plan -destroy -out "$plan"
echo "  plan: $env_dir/$plan"

if [ "$apply" -eq 1 ]; then
  echo "== 5. 適用（OCIリソースを削除） =="
  tofu apply "$plan"
  echo "完了。Tailscale管理コンソールから jarvis-host を削除してください。"
else
  echo "乾燥実行のみ。実行するには --apply を付けてください。"
  echo "  bash $SCRIPT_DIR/decommission-oci.sh --apply"
fi
