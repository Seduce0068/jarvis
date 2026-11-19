#!/usr/bin/env bash
# jarvis-mcp-gw の安全デプロイ:
#   ドリフト確認 → web.fetch承認必須化(後方互換) → unit/allowlist同期 → スモーク
#   → バイナリ配置/再起動 → systemd 実行時E2E必須 → 失敗時は全変更をロールバック
# 既定は乾燥実行。--apply で実行（要root）。--allow-web-fetch は後方互換用。
set -uo pipefail
apply=0; allow=0; repo_allowlist_changed=0
# step2 の /etc allowlist パッチを復元するためのフラグ（step3 の allowlist_changed とは独立）
allowlist_step2_changed=0
for arg in "$@"; do
  case "$arg" in
    --apply) apply=1 ;;
    --allow-web-fetch) allow=1 ;;
    *) echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
bin="$repo/rust/target/release/jarvis-mcp-gw"
allowlist="/etc/jarvis/mcp_allowlist.toml"
repo_allowlist="$repo/infra/config/mcp_allowlist.toml"
# I50-A: unit が読む host override env。repo テンプレートをそのまま配置元にする
# （テンプレート名はリネームしない）。
env_target="/etc/jarvis/jarvis-mcp-gw.env"
repo_env="$repo/infra/config/mcp_gateway.env"
unit="/etc/systemd/system/jarvis-mcp-gw.service"
repo_unit="$repo/infra/systemd/jarvis-mcp-gw.service"
ts="$(date +%Y%m%d-%H%M%S)"
die() { echo "ERROR: $*" >&2; exit 1; }
# 適用後の任意の失敗（再起動失敗・事後E2E失敗）で変更を戻す
rollback() {
  echo "== ロールバック: 変更を戻します ==" >&2
  # install-backup-timer.sh:46-48 と同じ枠組み:
  # 退避ファイルがあれば復元し、無ければ今回配置した先を明示削除する。
  if [ -f "/opt/jarvis/bin/jarvis-mcp-gw.prev-$ts" ]; then
    install -o root -g root -m 0755 "/opt/jarvis/bin/jarvis-mcp-gw.prev-$ts" /opt/jarvis/bin/jarvis-mcp-gw
  else
    rm -f /opt/jarvis/bin/jarvis-mcp-gw || true
  fi
  # I50-A: host override env を復元する。退避が無い場合は今回配置した先を削除し、
  # unit のリテラル既定値へ戻す（例: egress=example.com の fail-open は戻すが、
  # ロールバック後も check-deploy-drift.sh が DANGER を返して可視化する）。
  if [ "${env_changed:-0}" -eq 1 ]; then
    if [ -f "$env_target.bak-$ts" ]; then
      cp -a "$env_target.bak-$ts" "$env_target" || true
    else
      rm -f "$env_target" || true
    fi
  fi
  # /etc allowlist は step2(承認必須化パッチ) -> step3(repo版 install) の順に更新する。
  # よって復元は逆順(step3 -> step2)が必須。step2 のバックアップが元の内容を保持する。
  if [ "${allowlist_changed:-0}" -eq 1 ]; then
    if [ -f "$allowlist.bak-step3-$ts" ]; then
      install -o root -g root -m 0644 "$allowlist.bak-step3-$ts" "$allowlist"
    else
      rm -f "$allowlist" || true
    fi
  fi
  if [ "${allowlist_step2_changed:-0}" -eq 1 ]; then
    if [ -f "$allowlist.bak-step2-$ts" ]; then
      install -o root -g root -m 0644 "$allowlist.bak-step2-$ts" "$allowlist"
    else
      rm -f "$allowlist" || true
    fi
  fi
  # web.fetch 承認必須化で書き換えた repo 側 allowlist も元へ戻す（配備版だけの復元は不整合が残る）
  # repo 側は install 先ではないため、退避欠落時は削除せず手動確認を促す。
  if [ "${repo_allowlist_changed:-0}" -eq 1 ]; then
    if [ -f "$repo_allowlist.bak-step2-$ts" ]; then
      cp -a "$repo_allowlist.bak-step2-$ts" "$repo_allowlist"
    else
      echo "  警告: $repo_allowlist.bak-step2-$ts が無く repo allowlist を復元できません（手動確認が必要）" >&2
    fi
  fi
  if [ "${unit_changed:-0}" -eq 1 ]; then
    if [ -f "$unit.bak-step3-$ts" ]; then
      install -o root -g root -m 0644 "$unit.bak-step3-$ts" "$unit"
    else
      rm -f "$unit" || true
    fi
    systemctl daemon-reload || true
  fi
  systemctl restart jarvis-mcp-gw || true
  for _ in $(seq 1 20); do
    code=$(curl -q -s -o /dev/null -w '%{http_code}' -m 2 http://127.0.0.1:8082/health || true)
    [ "$code" = "200" ] && break
    sleep 0.5
  done
  echo "  ロールバック後: $(systemctl is-active jarvis-mcp-gw) / health=$(curl -q -s -o /dev/null -w '%{http_code}' -m 2 http://127.0.0.1:8082/health || echo 000)" >&2
  # ロールバック後の整合確認（情報表示のみ。ロールバックの成否には影響させない）
  bash "$repo/infra/scripts/check-deploy-drift.sh" || true
  exit 1
}

[ -x "$bin" ] || die "リリースバイナリがありません: cargo build --release -p jarvis-mcp-gw"
if [ "$apply" -eq 1 ] && [ "$(id -u)" -ne 0 ]; then die "rootで実行してください（sudo）"; fi

# I41-B: 配置前 provenance ゲート（--apply 時のみ・必須）。
# build-selfhost.sh の clean-tree gate は自スクリプト内に閉じており、
# rust/target/release/ の成果物をここから直接 install すると迂回できた。
# HEAD 解決とクリーンツリー確認を配置前の必須ゲートとし、未コミット成果物の
# 配置を拒否する。step2 は repo 側 allowlist を書き換えるため、ゲートは
# その前（ここ）に置く（書き換え後だと dirty を誤検出する）。
if [ "$apply" -eq 1 ]; then
  echo "== 0a. 配置前 provenance ゲート（git 整合性・必須） =="
  # rev-parse は fail-closed。git 失敗 / 空 / unknown は差分を出さず exit 1。
  if ! deploy_head="$(git -C "$repo" rev-parse HEAD 2>/dev/null)"; then
    die "git rev-parse HEAD が失敗しました: $repo（配置拒否）"
  fi
  if [ -z "$deploy_head" ] || [ "$deploy_head" = "unknown" ]; then
    die "git rev-parse HEAD が不正な値を返しました（配置拒否）"
  fi
  # --quiet は差分内容を出力しない。unstaged / staged の両方を must pass とする。
  if ! git -C "$repo" diff --quiet; then
    die "作業ツリーに未コミット差分があります: $repo（配置拒否）"
  fi
  if ! git -C "$repo" diff --cached --quiet; then
    die "インデックスにステージ済み差分があります: $repo（配置拒否）"
  fi
  echo "  HEAD=$deploy_head / tracked tree clean: OK"

  # I42-B(b): 配置対象と同じディレクトリの provenance マーカーを必須化。
  # 旧コードはマーカー欠落を best-effort として許容するため、
  # build-selfhost.sh が /opt/jarvis/bin/ にしかマーカーを置かない現行経路では
  # 古いバイナリが clean-tree ゲートを通って配置できた。欠落・空・HEAD 不一致
  # ・SHA-256 不一致はすべて fail-closed で拒否する（dry-run 既定・--apply 時
  # のみのゲート配置・ロールバック・段階別バックアップ名は不変）。
  # build-selfhost.sh が clean-tree gate 後に rust/target/release/ へ書く形式:
  #   1行目: ビルドコミット
  #   2行目以降: "<sha256>  <binary-name>"（sha256sum 形式）
  bin_marker="${bin%/*}/.jarvis-build-commit"
  [ -f "$bin_marker" ] || die "ビルドマーカーがありません: $bin_marker（配置拒否）"
  [ -s "$bin_marker" ] || die "ビルドマーカーが空です: $bin_marker（配置拒否）"
  IFS= read -r marker_head < "$bin_marker" || true
  marker_head="$(printf '%s' "$marker_head" | tr -d '[:space:]')"
  if [ -z "$marker_head" ] || [ "$marker_head" = "unknown" ]; then
    die "ビルドマーカーのコミットが不正です: $bin_marker（配置拒否）"
  fi
  if [ "$marker_head" != "$deploy_head" ]; then
    die "ビルドマーカー $marker_head が HEAD $deploy_head と不一致（配置拒否）"
  fi
  echo "  build marker commit: OK ($marker_head)"

  # バイナリ SHA-256 の照合。マーカー 2行目以降の jarvis-mcp-gw レコードを
  # 必須とし、欠落・不正値・配置対象バイナリとの不一致は配置拒否とする。
  expected_sha="$(awk '$2 == "jarvis-mcp-gw" { print $1; exit }' "$bin_marker")"
  [ -n "$expected_sha" ] || die "ビルドマーカーに jarvis-mcp-gw の SHA-256 がありません: $bin_marker（配置拒否）"
  if ! printf '%s\n' "$expected_sha" | grep -Eq '^[0-9a-fA-F]{64}$'; then
    die "ビルドマーカーの SHA-256 が不正です: $bin_marker（配置拒否）"
  fi
  actual_sha="$(sha256sum "$bin" | cut -d' ' -f1)"
  [ -n "$actual_sha" ] || die "配置対象の SHA-256 を計算できません: $bin（配置拒否）"
  if [ "$expected_sha" != "$actual_sha" ]; then
    die "バイナリ SHA-256 不一致: marker=$expected_sha actual=$actual_sha（配置拒否）"
  fi
  echo "  build marker sha256: OK ($actual_sha)"
fi

echo "== 0. 前提確認 =="
id -u koppe >/dev/null 2>&1 || die "ユーザー koppe が存在しません（unit の User= と不一致）"
[ -r /home/koppe ] || die "/home/koppe が読めません（ProtectHome=read-only と base dir の前提を確認）"
python3 -c 'import sys, tomllib; tomllib.load(open(sys.argv[1], "rb"))' "$repo_allowlist" \
  || die "allowlist の TOML が不正です: $repo_allowlist"
systemd-analyze verify "$repo_unit" >/dev/null 2>&1 || die "unit ファイルの検証に失敗: $repo_unit"
[ -f "$repo_env" ] || die "env テンプレートがありません: $repo_env"
# 必須キーが無い/不正なテンプレートを配置すると事後 check-deploy-drift.sh が
# 必ず DANGER になりロールバックするため、配置前に fail-closed で拒否する。
# I52-B / I54-B: BASE_DIR はパスエスケープ境界。EnvironmentFile は同一キーの
# 「最後の割り当てが勝ち」なので、any-match な grep は先頭に安全値・末尾に相対
# パスを置いたすり抜けを許す。check-deploy-drift.sh と同じロジック（重複拒否 +
# 最後の割り当て行のみ検査 + 絶対パス/非ルート/.. なし）を適用する。値は出力しない。
repo_base_count="$(grep -Ec '^[[:space:]]*JARVIS_MCP_GW_BASE_DIR=' "$repo_env" 2>/dev/null || true)"
repo_base_last="$(grep -E '^[[:space:]]*JARVIS_MCP_GW_BASE_DIR=' "$repo_env" 2>/dev/null | tail -n1 || true)"
[ "${repo_base_count:-0}" -ge 1 ] \
  || die "env テンプレートに JARVIS_MCP_GW_BASE_DIR がありません: $repo_env"
[ "${repo_base_count:-0}" -eq 1 ] \
  || die "env テンプレートの JARVIS_MCP_GW_BASE_DIR が重複しています（最後の割り当てが実効値）: $repo_env"
# 最後の割り当て行から値を取り出し、前後の空白と引用符を除去して形式検査する。
repo_base_val="${repo_base_last#*=}"
repo_base_val="${repo_base_val#"${repo_base_val%%[![:space:]]*}"}"
repo_base_val="${repo_base_val%"${repo_base_val##*[![:space:]]}"}"
case "$repo_base_val" in
  \"*\") repo_base_val="${repo_base_val#\"}"; repo_base_val="${repo_base_val%\"}" ;;
  \'*\') repo_base_val="${repo_base_val#\'}"; repo_base_val="${repo_base_val%\'}" ;;
esac
[[ "$repo_base_val" == /* ]] \
  || die "env テンプレートの JARVIS_MCP_GW_BASE_DIR が絶対パスではありません: $repo_env"
[ "$repo_base_val" != "/" ] \
  || die "env テンプレートの JARVIS_MCP_GW_BASE_DIR が / 単独です: $repo_env"
case "/$repo_base_val/" in
  */../*) die "env テンプレートの JARVIS_MCP_GW_BASE_DIR に .. 成分があります: $repo_env" ;;
esac
# egress allowlist の空値は正当（genuine deny-all）。キーの有無のみ検査する。
grep -Eq '^[[:space:]]*JARVIS_MCP_GW_EGRESS_ALLOWLIST=' "$repo_env" \
  || die "env テンプレートに JARVIS_MCP_GW_EGRESS_ALLOWLIST がありません: $repo_env"
echo "  user/base-dir/allowlist/unit/env-template: OK"

echo "== 1. ドリフト確認（事前・情報表示のみ） =="
bash "$repo/infra/scripts/check-deploy-drift.sh" || true

if grep -A8 'name = "web.fetch"' "$allowlist" 2>/dev/null | grep -q 'requires_approval = false'; then
  if [ "$allow" -eq 1 ]; then
    echo "== 2. web.fetch承認必須化（repo + /etc） =="
    if [ "$apply" -eq 1 ]; then
      cp -a "$allowlist" "$allowlist.bak-step2-$ts" || die "退避コピー失敗: $allowlist.bak-step2-$ts"
      # repo 側 allowlist もここで .bak-step2-$ts へ退避（ロールバック時に復元するため）
      # 退避に失敗したら新規配置へ進まない（|| true を外して中止）
      cp -a "$repo_allowlist" "$repo_allowlist.bak-step2-$ts" || die "退避コピー失敗: $repo_allowlist.bak-step2-$ts"
      python3 - "$allowlist" "$repo_allowlist" <<'PY'
import sys
for path in sys.argv[1:]:
    text = open(path).read()
    old = '''name = "web.fetch"
description = "Fetch web page content"
allowed_principals = ["jarvis", "opencode"]
max_args_size = 5242880
timeout_ms = 30000
requires_approval = false'''
    new = old.replace('requires_approval = false', 'requires_approval = true')
    if old in text:
        open(path, 'w').write(text.replace(old, new, 1))
        print('  patched:', path)
PY
      # 実際に書き換わった場合のみロールバック対象にする（step2 は /etc と repo の両方をパッチ）
      if [ -f "$allowlist.bak-step2-$ts" ] && ! cmp -s "$allowlist" "$allowlist.bak-step2-$ts"; then
        allowlist_step2_changed=1
      fi
      if [ -f "$repo_allowlist.bak-step2-$ts" ] && ! cmp -s "$repo_allowlist" "$repo_allowlist.bak-step2-$ts"; then
        repo_allowlist_changed=1
      fi
    else
      echo "  DRY: would set requires_approval = true in repo and /etc"
    fi
  else
    die "web.fetchが承認なしです。--allow-web-fetch を付けるか手動で承認必須化してください"
  fi
else
  echo "== 2. web.fetch承認: 既に必須（スキップ） =="
fi

echo "== 3. unit / allowlist / env 同期 =="
unit_changed=0; allowlist_changed=0; env_changed=0
if [ -f "$unit" ] && cmp -s "$unit" "$repo_unit"; then
  echo "  unit: 既に一致（スキップ）"
else
  unit_changed=1
  echo "  unit: 差分あり -> $unit を $repo_unit で更新"
  if [ "$apply" -eq 1 ] && [ -f "$unit" ]; then
    cp -a "$unit" "$unit.bak-step3-$ts" || die "退避コピー失敗: $unit.bak-step3-$ts"
  fi
fi
if [ -f "$allowlist" ] && cmp -s "$allowlist" "$repo_allowlist"; then
  echo "  allowlist: 既に一致（スキップ）"
else
  allowlist_changed=1
  echo "  allowlist: 差分あり -> $allowlist を repo 版で更新"
  if [ "$apply" -eq 1 ] && [ -f "$allowlist" ]; then
    cp -a "$allowlist" "$allowlist.bak-step3-$ts" || die "退避コピー失敗: $allowlist.bak-step3-$ts"
  fi
fi
# I50-A: unit が読む /etc/jarvis/jarvis-mcp-gw.env を repo テンプレートから配置。
# 既存の unit/allowlist と同じ段階別バックアップ・ドリフト判定の枠組みを使う
# （env のみ .bak-$ts を退避名とし、ロールバックは env_changed で制御）。
if [ -f "$env_target" ] && cmp -s "$env_target" "$repo_env"; then
  echo "  env: 既に一致（スキップ）"
else
  env_changed=1
  echo "  env: 差分あり -> $env_target を $repo_env で更新"
  if [ "$apply" -eq 1 ] && [ -f "$env_target" ]; then
    cp -a "$env_target" "$env_target.bak-$ts" || die "退避コピー失敗: $env_target.bak-$ts"
  fi
fi
if [ "$apply" -eq 1 ]; then
  if [ "$unit_changed" -eq 1 ]; then
    install -o root -g root -m 0644 "$repo_unit" "$unit"
    systemctl daemon-reload
  fi
  if [ "$allowlist_changed" -eq 1 ]; then
    install -o root -g root -m 0644 "$repo_allowlist" "$allowlist"
  fi
  if [ "$env_changed" -eq 1 ]; then
    # 同一ディレクトリの一時ファイルへ書いて mv で原子的に置換する。
    # secret を含みうるため root:root 0600（systemd が root で読み込む）。
    tmp_env="$(mktemp "$env_target.tmp.XXXXXX")" \
      || { echo "env 一時ファイル作成失敗: $env_target" >&2; rollback; }
    if ! install -o root -g root -m 0600 "$repo_env" "$tmp_env"; then
      rm -f "$tmp_env" || true
      echo "env 一時配置失敗: $repo_env -> $tmp_env" >&2
      rollback
    fi
    mv -f "$tmp_env" "$env_target" \
      || { echo "env の原子的配置失敗: $env_target" >&2; rollback; }
  fi
else
  [ "$unit_changed" -eq 1 ] && echo "  DRY: would install $repo_unit -> $unit and daemon-reload"
  [ "$allowlist_changed" -eq 1 ] && echo "  DRY: would install $repo_allowlist -> $allowlist"
  [ "$env_changed" -eq 1 ] && echo "  DRY: would install $repo_env -> $env_target (root:root 0600, backup $env_target.bak-$ts)"
fi

echo "== 4. スモークテスト（systemd 外・別ポート） =="
bash "$repo/infra/scripts/smoke-mcp-gw.sh" "$bin" approval || die "スモークテスト失敗（デプロイ中止）"

echo "== 5. 配置・再起動 =="
if [ "$apply" -eq 1 ]; then
  if [ -f /opt/jarvis/bin/jarvis-mcp-gw ]; then
    cp -a /opt/jarvis/bin/jarvis-mcp-gw "/opt/jarvis/bin/jarvis-mcp-gw.prev-$ts" \
      || die "退避コピー失敗: /opt/jarvis/bin/jarvis-mcp-gw.prev-$ts"
  fi
  install -o root -g root -m 0755 "$bin" /opt/jarvis/bin/jarvis-mcp-gw
  systemctl restart jarvis-mcp-gw || { echo "再起動に失敗しました" >&2; rollback; }
  for _ in $(seq 1 20); do
    code=$(curl -q -s -o /dev/null -w '%{http_code}' -m 2 http://127.0.0.1:8082/health || true)
    [ "$code" = "200" ] && break
    sleep 0.5
  done
  systemctl is-active --quiet jarvis-mcp-gw || { echo "サービスが起動していません" >&2; rollback; }
  echo "  deployed: $(systemctl is-active jarvis-mcp-gw)"
else
  echo "  DRY: would install $bin -> /opt/jarvis/bin/jarvis-mcp-gw and restart"
fi

echo "== 6. 事後確認（systemd 配下の実行時E2Eを含む・必須） =="
if [ "$apply" -eq 1 ]; then
  if bash "$repo/infra/scripts/check-deploy-drift.sh"; then
    echo "== DONE: APPLIED（事後確認 OK） =="
    exit 0
  fi
  echo "== 事後確認 NG ==" >&2
  rollback
else
  echo "  DRY: 適用後に check-deploy-drift.sh が OK を返すまでを合格条件とする"
  echo "== DONE: DRY-RUN =="
fi
