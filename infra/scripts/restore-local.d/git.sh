#!/usr/bin/env bash
# S7: git コンポーネントの復元。restore-local.sh から source して使う。
# このファイルは restore_git() 1 つのみを定義する。
# 呼び出し側が定義済み: SET_DIR / APPLY / CONFIRM / log() / fail()
# シェルオプション・trap は呼び出し側が所有するため、ここでは変更しない。
#
# 対象リポジトリは設計文書の --repo-dir 相当の固定パス。
#   /home/koppe/ドキュメント/作業用/jarvis/jarvis
# 入力は git archive HEAD の成果物（.git/ 不包含・追跡ファイルのみ）。
# I75-B: 入力パスは共有ヘルパ resolve_restore_input() が manifest の
# role/format 記述子から解決する。git-jarvis-head.tar.gz は legacy 3列行の
# フォールバック名に留まる。git は任意コンポーネントのため「行が無い」場合
# のみ警告してスキップし、複数一致・不正 role・ファイル不在は fail-closed。

restore_git() {
  local rc=0
  local archive="" resolve_rc=0
  local repo='/home/koppe/ドキュメント/作業用/jarvis/jarvis'
  local tmp_dir='' bad='' status_out='' ts='' tag='' top='' real_repo='' suggested=''
  # backup-local.sh の拒否条件 (.git / .env / *.key) を基数に、
  # 秘密ファイル (.pem / .ssh / id_rsa / bws.env / credentials) を追加。
  local secret_re='(^|/)\.git(/|$)|(^|/)\.env($|\.)|(^|/)bws\.env$|\.key$|\.pem$|(^|/)\.ssh(/|$)|(^|/)id_rsa|(^|/)credentials($|/)'
  local traversal_re='(^/)|((^|/)\.\.(/|$))'

  # I75-B: manifest の role/format 記述子から入力を解決する。
  if ! declare -F resolve_restore_input >/dev/null 2>&1; then
    fail "git: shared restore input resolver unavailable" || true
    return 1
  fi
  archive="$(resolve_restore_input "${SET_DIR:-}" git git-jarvis-head.tar.gz)" || resolve_rc=$?
  if [ "$resolve_rc" -eq 2 ]; then
    log "WARN: git payload が manifest に無いため git 復元をスキップ（任意コンポーネント）"
    return 0
  elif [ "$resolve_rc" -ne 0 ]; then
    fail "git: cannot resolve a unique git payload from manifest (unknown/ambiguous/missing)" || true
    return 1
  fi

  # 任意コンポーネント: 無ければ警告して成功扱いでスキップ。
  if [ ! -f "$archive" ]; then
    log "WARN: $(basename -- "$archive") が無いため git 復元をスキップ（任意コンポーネント）"
    return 0
  fi

  # 形式チェックは読み取りのみ。破損アーカイブは fail-closed。
  if ! tar -tzf "$archive" >/dev/null 2>&1; then
    fail "git アーカイブを読み取れません: $(basename "$archive")" || true
    return 1
  fi

  # APPLY=0: 計画のみ。tar -t は読み取り専用なのでバイトも書き込まない。
  # （要件の「展開して検査」は APPLY=1 の経路で実施する。）
  if [ "${APPLY:-0}" -ne 1 ]; then
    # grep -q は pipefail 下で tar に SIGPIPE を送るため -q を使わない。
    if tar -tzf "$archive" 2>/dev/null | grep -Ei -- "$secret_re" >/dev/null; then
      fail "git アーカイブに秘密/禁止パスが含まれるため拒否" || true
      return 1
    fi
    if tar -tzf "$archive" 2>/dev/null | grep -E -- "$traversal_re" >/dev/null; then
      fail "git アーカイブに絶対パス/親ディレクトリ参照が含まれるため拒否" || true
      return 1
    fi
    log "計画: git-jarvis-head.tar.gz を読み取り検査（.git/・秘密ファイル・traversal なし）"
    log "計画: repo=$repo への上書きは --apply --confirm の両方指定時のみ"
    log "計画: 上書き前に現在 HEAD を退避タグ bak/git-head-<ts> で保存（名前のみ log）"
    if [ "${CONFIRM:-0}" -eq 1 ]; then
      log "WARN: --confirm のみでは本番へ書き込みません（--apply も必要）"
    fi
    return 0
  fi

  # APPLY=1: 一時ディレクトリへ展開して内容を検査する（書き込みは一時領域のみ）。
  tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/jarvis-git-restore.XXXXXX" 2>/dev/null)" || {
    fail "展開用の一時ディレクトリを作成できません" || true
    return 1
  }
  if ! mkdir -p -- "$tmp_dir/tree"; then
    rm -rf -- "$tmp_dir"
    fail "展開先ディレクトリを作成できません" || true
    return 1
  fi

  if tar -tzf "$archive" 2>/dev/null | grep -Ei -- "$secret_re" >/dev/null; then
    rm -rf -- "$tmp_dir"
    fail "git アーカイブに秘密/禁止パスが含まれるため拒否" || true
    return 1
  fi
  if tar -tzf "$archive" 2>/dev/null | grep -E -- "$traversal_re" >/dev/null; then
    rm -rf -- "$tmp_dir"
    fail "git アーカイブに絶対パス/親ディレクトリ参照が含まれるため拒否" || true
    return 1
  fi
  if ! tar -xzf "$archive" -C "$tmp_dir/tree" 2>/dev/null; then
    rm -rf -- "$tmp_dir"
    fail "一時ディレクトリへの展開に失敗しました" || true
    return 1
  fi

  # 展開後の実ファイルも走査して、名前を変えた秘密ファイルを拾う。
  bad="$(find "$tmp_dir/tree" -mindepth 1 \
      \( -name '.git' -o -name '.env' -o -name '*.env' -o -name 'bws.env' \
      -o -name '*.key' -o -name '*.pem' -o -name '.ssh' \
      -o -name 'id_rsa*' -o -name 'credentials' \) -print -quit 2>/dev/null || true)"
  if [ -n "$bad" ]; then
    rm -rf -- "$tmp_dir"
    fail "git アーカイブに秘密/禁止ファイルが含まれるため拒否: $(basename "$bad")" || true
    return 1
  fi

  # APPLY=1 かつ CONFIRM!=1: 本番リポジトリは一切触らず、隔離先へ展開しただけ。
  if [ "${CONFIRM:-0}" -ne 1 ]; then
    log "隔離復元: 本番は変更せず、隔離先へ展開しました: $tmp_dir/tree"
    log "WARN: 隔離先は一時ディレクトリです（永続化が必要なら手動でコピー）"
    return 0
  fi

  # 本番反映: 対象がリポジトリのトップレベルであることを確認する。
  top="$(git -C "$repo" rev-parse --show-toplevel 2>/dev/null || true)"
  real_repo="$(cd -- "$repo" 2>/dev/null && pwd -P || true)"
  if [ -z "$top" ] || [ -z "$real_repo" ] || [ "$top" != "$real_repo" ]; then
    rm -rf -- "$tmp_dir"
    fail "対象が git 作業ツリーのトップレベルではありません: $repo" || true
    return 1
  fi

  # clean tree ガード: 未コミット変更（untracked 含む）があれば拒否し、
  # 新規ディレクトリへの展開を提案するだけ（自動実行はしない）。
  if ! status_out="$(git -C "$repo" status --porcelain 2>/dev/null)"; then
    rm -rf -- "$tmp_dir"
    fail "git status --porcelain の確認に失敗しました" || true
    return 1
  fi
  if [ -n "$status_out" ]; then
    ts="$(date +%Y%m%d-%H%M%S)"
    suggested="${repo}-restore-${ts}"
    rm -rf -- "$tmp_dir"
    fail "作業ツリーに未コミット変更があるため上書きを拒否しました" || true
    log "提案: 手動で新規ディレクトリへ展開してください: mkdir -p '$suggested' && tar -xzf '$archive' -C '$suggested'"
    return 1
  fi

  # 上書き前に現在 HEAD を退避。タグ名のみを log し、退避対象の内容は出さない。
  ts="${ts:-$(date +%Y%m%d-%H%M%S)}"
  tag="bak/git-head-${ts}"
  if ! git -C "$repo" tag "$tag" HEAD 2>/dev/null; then
    rm -rf -- "$tmp_dir"
    fail "退避タグの作成に失敗したため上書きしません: $tag" || true
    return 1
  fi
  log "退避タグ: $tag"

  # 検査済みアーカイブを作業ツリーへ展開する。
  # 注: git archive は追跡ファイルのみ。削除済みファイルの後始末は S7 の範囲外。
  if ! tar -xzf "$archive" -C "$repo" 2>/dev/null; then
    rm -rf -- "$tmp_dir"
    fail "作業ツリーへの展開に失敗しました（退避タグ: $tag）" || true
    return 1
  fi

  rm -rf -- "$tmp_dir"
  log "git 復元完了: $repo (退避タグ: $tag)"
  return "$rc"
}
