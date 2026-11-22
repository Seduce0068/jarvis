#!/usr/bin/env bash
# jarvis-mcp-gw のリリースバイナリを /opt/jarvis/bin へ配置しサービス再起動（要root）
# 既定は乾燥実行。--apply で実行。失敗時は旧バイナリへロールバック。
set -euo pipefail
apply=0; [ "${1:-}" = "--apply" ] && apply=1
if [ "$apply" -eq 1 ] && [ "$(id -u)" -ne 0 ]; then echo "rootで実行してください" >&2; exit 1; fi
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$script_dir/../.." && pwd)"
src="$repo/rust/target/release/jarvis-mcp-gw"
dst="/opt/jarvis/bin/jarvis-mcp-gw"
ts="$(date +%Y%m%d-%H%M%S)"

# I103-B: 第3のデプロイ経路も deploy-mcp-gw-safe.sh / deploy.rs と同じ
# 上限付きヘルスゲートへ統一（固定 2 秒待ち + 1回プローブは廃止）。
health_attempts=20
health_interval=0.5
# 固定の degraded マーカー。status フィールドのみを検査し、整形済み JSON も許容する。
health_degraded_pattern='"status"[[:space:]]*:[[:space:]]*"degraded"'
health_code=000
health_body=

# /health を1回取得し、本文を health_body、HTTP コード（3桁/失敗時 000）を
# health_code に格納する。上流の理由本文は決して出力しない（code only）。
fetch_health() {
  local raw
  raw="$(curl -s -w '\n%{http_code}' -m 2 http://127.0.0.1:8082/health || true)"
  if [[ "$raw" == *$'\n'* ]]; then
    health_body="${raw%$'\n'*}"
    health_code="${raw##*$'\n'}"
  else
    health_body=""
    health_code=000
  fi
  case "$health_code" in
    [0-9][0-9][0-9]) ;;
    *) health_code=000; health_body="" ;;
  esac
}

# 503 または固定 degraded 本文は degraded サービスとして扱う。
health_is_degraded() {
  [ "$health_code" = "503" ] \
    || printf '%s' "$health_body" | grep -Eq "$health_degraded_pattern"
}

# ロールバック後の drift 検査は情報表示 (|| true) ではなく強制ゲートとして実行する。
# exit 1 (PENDING) はロールバックに伴う通常差分なので警告に留めるが、
# exit 2 (DANGER) は「安全な状態へ戻せていない」として失敗扱い (exit 3) にする。
rollback() {
  echo "== ロールバック: 旧バイナリへ戻します ==" >&2
  # deploy-mcp-gw-safe.sh:87-91 と同じ枠組み: 退避ファイルがあれば復元し、
  # 無ければ今回配置した新バイナリを明示削除して「旧バイナリが無い状態」へ
  # fail-closed に戻す。何も戻さないと新バイナリが残ったまま再起動／drift
  # 検査へ進み、安全状態を確認できない。
  if [ -f "$dst.prev-$ts" ]; then
    install -o root -g root -m 0755 "$dst.prev-$ts" "$dst"
  else
    rm -f "$dst" || true
  fi
  systemctl restart jarvis-mcp-gw || true
  echo "  ロールバック後: $(systemctl is-active jarvis-mcp-gw) / health=$health_code" >&2
  local drift_rc=0
  if bash "$repo/infra/scripts/check-deploy-drift.sh"; then
    drift_rc=0
  else
    drift_rc=$?
  fi
  case "$drift_rc" in
    0) echo "  ロールバック後 drift: OK" >&2 ;;
    1) echo "  ロールバック後 drift: PENDING（差分あり・手動確認）" >&2 ;;
    2) echo "  ロールバック後 drift: DANGER（安全状態を確認できません。手動対応必須）" >&2; exit 3 ;;
    *) echo "  ロールバック後 drift: 不明な終了コード $drift_rc" >&2; exit 3 ;;
  esac
  exit 1
}

echo "== 1. ビルド確認 =="
if [ ! -x "$src" ]; then
  echo "  リリースバイナリがありません。次を実行:"
  echo "  (cd $repo/rust && cargo build --release -p jarvis-mcp-gw)"
  exit 1
fi
echo "  $src ($(stat -c %s "$src")B)"

echo "== 2. 配置計画 =="
echo "  install -o root -g root -m 0755 $src $dst"
echo "  backup: $dst.prev-$ts"
if [ "$apply" -eq 1 ]; then
  [ -f "$dst" ] && cp -a "$dst" "$dst.prev-$ts"
  install -o root -g root -m 0755 "$src" "$dst"
  echo "== 3. サービス再起動 =="
  if ! systemctl restart jarvis-mcp-gw; then
    echo "  再起動失敗: ロールバックします" >&2
    rollback
  fi
  for _ in $(seq 1 "$health_attempts"); do
    fetch_health
    [ "$health_code" = "200" ] && break
    sleep "$health_interval"
  done
  # ループは 200 が一度でも観測されたことしか証明しない。deploy-mcp-gw-safe.sh
  # と同じく最後にもう一度本文を取得し、503 / degraded 本文ならロールバックする。
  fetch_health
  if [ "$health_code" = "503" ] || health_is_degraded; then
    echo "ヘルスチェックが degraded を検出しました (code=$health_code)" >&2
    rollback
  fi
  if [ "$health_code" != "200" ]; then
    echo "ヘルスチェックが 200 を返しませんでした (code=$health_code)" >&2
    rollback
  fi
  echo "  再起動OK: $(systemctl is-active jarvis-mcp-gw) / health=$health_code (not degraded)"
else
  echo "乾燥実行のみ。実行するには --apply を付けてください。"
fi
