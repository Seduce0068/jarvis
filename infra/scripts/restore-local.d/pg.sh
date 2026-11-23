#!/usr/bin/env bash
# S4 (restore-local.d/pg.sh): PostgreSQL コンポーネントの復元。
#
# このファイルは restore-local.sh から source される前提。呼び出し側が次を定義済み:
#   SET_DIR   : 選択済みバックアップセットのディレクトリ
#   APPLY     : 0=計画のみ / 1=実行
#   CONFIRM   : 1=本番反映まで行う（APPLY=1 と併用して初めて有効）
#   log()     : 情報ログ（秘密情報を渡さない）
#   fail()    : エラー報告（呼び出し側の規約に従う）
#   KEEP_ISOLATED_DB : 1=隔離DBを保持（既定=検証後にDROP）。--keep-isolated-db 由来。
#   ISOLATED_DB_NAME / ISOLATED_DB_CREATED : 後始末状態を EXIT trap と共有するグローバル。
#
# 定義する関数は restore_pg()（引数なし、戻り値 0/1）と、その補助関数
# pg_fixed_error() / expected_sqlx_version() / verify_sqlx_migrations() /
# rollback_from_backup()。実 DB 操作は APPLY=1 のときだけ行う。入力は共有ヘルパ
# resolve_restore_input()（restore-local.d/configs.sh で定義）が manifest の
# role/format 記述子から解決するパス。固定名 postgres-jarvis.sql.gz は
# role 列を持たない legacy 3列行のフォールバックとしてのみ使う（I75-B）。
# 設定: DB_NAME=jarvis / 隔離 DB=jarvis_restore_test。
# 出力・ログには DB 名とテーブル件数（数値）のみを出し、テーブル内容・行データ・
# 接続情報（秘密情報）は一切出力しない。
#
# 後始末ポリシー: 既定では隔離 DB の検証直後に DROP し、Qdrant の隔離
# コレクション削除と対称に本番データのコピーを残さない。保持する場合は
# 呼び出し側が --keep-isolated-db を明示する（KEEP_ISOLATED_DB=1）。
# CREATE 成功後 DROP 完了前に異常終了した分は restore-local.sh の EXIT trap が
# ISOLATED_DB_CREATED を見て片付ける（既存のサービス再起動 trap は変更しない）。
#
# 本番反映（CONFIRM=1）のロールバック方針:
# 旧DB を TEMPLATE で退避した後に本番DB を DROP した以降の失敗
# （CREATE 失敗 / ダンプ適用失敗 / 再検証失敗）では、退避DB の実在を確認してから
# 本番DB を退避コピーで再作成する。DROP 自体が失敗した分岐は本番DB が無傷のため
# ロールバックしない（二重に壊すリスクを避ける）。退避DB を確認できない場合は
# 何も触らず fail-closed にする。
# 呼び出し側の fail() は exit 1 するため、ロールバックが必要な分岐では
# 先に rollback_from_backup() を呼び、その後に従来の fail() 語彙で終了する。

# exit せず固定語彙だけを stderr に出す（値・SQL・DB 名は埋め込まない）。
# 呼び出し側が元の fail() を呼べるようにするための補助。
pg_fixed_error() {
  printf '[restore-local] ERROR: PG: %s\n' "$1" >&2
  return 1
}

# 期待する sqlx マイグレーション version を repo から動的に導出する。
# rust/migrations/*.sql のファイル名プレフィクス（3 桁）の最大値を採用。
# 単体実行で migrations ディレクトリが見つからない場合の既定 007 の根拠:
# 本リポジトリは 001_initial.sql から
# 007_pending_embeddings_active_index.sql まで連番で、最大番号が 007 のため。
expected_sqlx_version() {
  local fallback="007"
  local mod_dir repo_root f name candidate max=""
  mod_dir="$(dirname -- "${BASH_SOURCE[0]}")" || {
    printf '%s\n' "$fallback"
    return 0
  }
  repo_root="$(cd -- "$mod_dir/../.." 2>/dev/null && pwd -P)" || {
    printf '%s\n' "$fallback"
    return 0
  }
  if [ ! -d "$repo_root/rust/migrations" ]; then
    printf '%s\n' "$fallback"
    return 0
  fi
  for f in "$repo_root"/rust/migrations/*.sql; do
    [ -f "$f" ] || continue
    name="${f##*/}"
    case "$name" in
      [0-9][0-9][0-9]_*) candidate="${name%%_*}" ;;
      *) continue ;;
    esac
    if [ -z "$max" ] || [ "$((10#$candidate))" -gt "$((10#$max))" ]; then
      max="$candidate"
    fi
  done
  printf '%s\n' "${max:-$fallback}"
}

# 復元先 DB の _sqlx_migrations を検証する（存在 + 最新 version の照合）。
# 引数: target_db, mode(strict|lenient)
#   strict  : version が取得できない場合も失敗（本番反映後の再検証）。
#   lenient : version が取得できない場合は legacy ペイロードとして警告のみ。
#             隔離検証では sqlx 情報を持たない合成/レガシー dump を許容するため。
# 値が取得できて期待値と異なる場合は常に失敗（fail-closed）。
verify_sqlx_migrations() { # target_db mode
  local target_db="$1" mode="${2:-strict}"
  local expected="" mgmt_count="" actual=""
  expected="$(expected_sqlx_version)" || return 1

  mgmt_count="$(psql -v ON_ERROR_STOP=1 -d "$target_db" -tAc \
    "SELECT count(*) FROM information_schema.tables WHERE table_schema NOT IN ('pg_catalog', 'information_schema') AND table_name = '_sqlx_migrations'" 2>/dev/null)" || {
    pg_fixed_error "マイグレーション管理表の確認に失敗しました"
    return 1
  }
  case "$mgmt_count" in
    ''|*[!0-9]*) pg_fixed_error "マイグレーション管理表の確認結果が不正です"; return 1 ;;
  esac
  if [ "$mgmt_count" -eq 0 ]; then
    if [ "$mode" = "lenient" ]; then
      # sqlx 未管理 DB（手動作成／他経路で作られた DB）では _sqlx_migrations
      # 自体が存在しない。隔離検証ではレガシー/合成 dump と同列に扱い、
      # 警告のみで通過させる。本番再検証（strict）は従来どおり fail-closed。
      log "PG: マイグレーション管理表が無いため version 照合をスキップしました（未管理 DB）"
      return 0
    fi
    pg_fixed_error "マイグレーション管理表が見つかりません"
    return 1
  fi

  actual="$(psql -v ON_ERROR_STOP=1 -d "$target_db" -tAc \
    "SELECT max(version) FROM _sqlx_migrations" 2>/dev/null)" || {
    pg_fixed_error "マイグレーション version の取得に失敗しました"
    return 1
  }
  if [ -z "$actual" ]; then
    if [ "$mode" = "strict" ]; then
      pg_fixed_error "マイグレーション version が取得できませんでした"
      return 1
    fi
    log "PG: マイグレーション version が未記録のため照合をスキップしました"
    return 0
  fi
  case "$actual" in
    ''|*[!0-9]*) pg_fixed_error "マイグレーション version の形式が不正です"; return 1 ;;
  esac
  if [ "$((10#$actual))" -ne "$((10#$expected))" ]; then
    pg_fixed_error "マイグレーション version が期待値と一致しません"
    return 1
  fi
  log "PG: マイグレーション version の照合に成功しました"
  return 0
}

# 退避DB から本番DB を再作成する。exit せず 0/1 を返す。
# 退避DB の実在を確認できなければ何も壊さず失敗（fail-closed）。
rollback_from_backup() { # backup_db prod_db
  local backup_db="$1" prod_db="$2"
  local rb_exists=""
  rb_exists="$(psql -v ON_ERROR_STOP=1 -d postgres -tAc \
    "SELECT 1 FROM pg_database WHERE datname = '$backup_db'" 2>/dev/null)" || {
    pg_fixed_error "ロールバックできません（退避DB の確認に失敗しました）"
    return 1
  }
  if [ -z "$rb_exists" ]; then
    pg_fixed_error "ロールバックできません（退避DB が見つかりません）"
    return 1
  fi
  psql -v ON_ERROR_STOP=1 -d postgres -tAc \
    "DROP DATABASE IF EXISTS \"$prod_db\"" >/dev/null 2>&1 || {
    pg_fixed_error "ロールバックに失敗しました（本番DB の削除に失敗）"
    return 1
  }
  psql -v ON_ERROR_STOP=1 -d postgres -tAc \
    "CREATE DATABASE \"$prod_db\" TEMPLATE \"$backup_db\"" >/dev/null 2>&1 || {
    pg_fixed_error "ロールバックに失敗しました（本番DB の再作成に失敗）"
    return 1
  }
  log "PG: 退避DB から本番DB を再作成しました（ロールバック完了）"
  return 0
}

restore_pg() {
  # DB_NAME is defined by restore-local.sh; honour it here so the dry-run plan
  # and the real psql calls target the same database (Round 95).
  local db_name="${DB_NAME:-jarvis}"
  local isolated_db="jarvis_restore_test"
  local backup_db="" ts="" exists="" table_count="" dump_file=""
  local count_sql="SELECT count(*) FROM information_schema.tables WHERE table_schema NOT IN ('pg_catalog', 'information_schema')"

  # 共通インターフェースの必須確認（呼び出し側の set -u でも落ちないよう ${...:-} を使う）
  if [ -z "${SET_DIR:-}" ] || ! declare -F log >/dev/null 2>&1 || ! declare -F fail >/dev/null 2>&1; then
    echo "ERROR: restore_pg requires SET_DIR, log() and fail() from the caller" >&2
    return 1
  fi
  # I75-B: resolve the input from the manifest descriptors instead of a fixed
  # basename. Fail closed on unknown/ambiguous roles and missing files, using
  # the same rules as verify_set(). `none` is the only accepted encryption /
  # signature (P1-6 connection point: lib/payload-descriptors.sh).
  if ! declare -F resolve_restore_input >/dev/null 2>&1; then
    fail "PG: shared restore input resolver unavailable"
    return 1
  fi
  if ! dump_file="$(resolve_restore_input "$SET_DIR" postgres postgres-jarvis.sql.gz)"; then
    fail "PG: cannot resolve a unique postgres payload from manifest (unknown/ambiguous/missing)"
    return 1
  fi

  # --- APPLY=0: 計画のみ。DB 接続を含む書き込みを一切行わない ---
  if [ "${APPLY:-0}" -ne 1 ]; then
    log "PG: dry-run (APPLY=0) — 書き込みゼロ"
    log "PG: 入力 = $dump_file"
    log "PG: 隔離DB '$isolated_db' へ gzip -cd | psql -v ON_ERROR_STOP=1 で復元する計画"
    log "PG: information_schema.tables の件数を検証する計画（件数のみ記録）"
    log "PG: _sqlx_migrations の存在と最新 version を repo の期待値と照合する計画"
    if [ "${KEEP_ISOLATED_DB:-0}" -eq 1 ]; then
      log "PG: [計画] 隔離DB '$isolated_db' は --keep-isolated-db により検証後も保持"
    else
      log "PG: [計画] 隔離DB '$isolated_db' は検証後に DROP（Qdrant と対称）"
    fi
    if [ "${CONFIRM:-0}" -eq 1 ]; then
      log "PG: [計画] CONFIRM あり — 旧DB '$db_name' を退避 → DROP/CREATE → 再復元 → 再検証"
      log "PG: [計画] 本番反映後の失敗時は退避DB から本番DB を再作成する（退避未確認時は fail-closed）"
    else
      log "PG: [計画] 本番DB '$db_name' は触らない（CONFIRM なし）"
    fi
    return 0
  fi

  [ -f "$dump_file" ] || { fail "PG: バックアップが見つかりません: $dump_file"; return 1; }
  # 解決済み入力パスを記録（ファイル名のみ。内容・接続情報は出さない）。
  log "PG: resolved input = $dump_file"

  # --- 既存の隔離 DB は上書きせず fail ---
  exists="$(psql -v ON_ERROR_STOP=1 -d postgres -tAc \
    "SELECT 1 FROM pg_database WHERE datname = '$isolated_db'" 2>/dev/null)" || {
    fail "PG: 隔離DB の存在確認に失敗しました"
    return 1
  }
  if [ -n "$exists" ]; then
    fail "PG: 隔離DB '$isolated_db' が既に存在します（上書きしません）"
    return 1
  fi

  # --- 隔離 DB を作成して復元（設計 S4-1 のコマンドを厳守） ---
  psql -v ON_ERROR_STOP=1 -d postgres -tAc "CREATE DATABASE \"$isolated_db\"" >/dev/null 2>&1 || {
    fail "PG: 隔離DB '$isolated_db' の作成に失敗しました"
    return 1
  }
  # CREATE 成功を EXIT trap に通知。DROP 前に失敗した場合の保険として残す。
  # export は呼び出し側シェル (restore-local.sh の on_exit) との共有を示す。
  export ISOLATED_DB_NAME="$isolated_db"
  export ISOLATED_DB_CREATED=1
  if ! gzip -cd "$dump_file" | psql -v ON_ERROR_STOP=1 -d "$isolated_db" >/dev/null; then
    fail "PG: 隔離DB '$isolated_db' への復元に失敗しました"
    return 1
  fi

  # --- 件数検証（件数のみ log。テーブル内容/行データは出さない） ---
  table_count="$(psql -v ON_ERROR_STOP=1 -d "$isolated_db" -tAc "$count_sql" 2>/dev/null)" || {
    fail "PG: 隔離DB のテーブル件数取得に失敗しました"
    return 1
  }
  case "$table_count" in
    ''|*[!0-9]*) fail "PG: 隔離DB のテーブル件数が不正です"; return 1 ;;
  esac
  if [ "$table_count" -eq 0 ]; then
    fail "PG: 隔離DB のテーブル件数が 0 です（復元結果を疑います）"
    return 1
  fi
  log "PG: 隔離DB '$isolated_db' のテーブル数 = $table_count"

  # 復元後検証の強化: テーブル件数だけでなく sqlx マイグレーション管理表の
  # 存在と最新 version を照合する。隔離段は legacy ペイロード互換のため
  # version 未記録のみ許容し、不一致は fail-closed にする。
  if ! verify_sqlx_migrations "$isolated_db" lenient; then
    fail "PG: 隔離DB のマイグレーション検証に失敗しました"
  fi

  # --- 隔離 DB の後始末（Qdrant の隔離コレクション削除と対称）---------------
  # 既定は検証直後に DROP し、本番データのコピーを残さない。
  # 残す場合は --keep-isolated-db（KEEP_ISOLATED_DB=1）の明示指定のみ。
  if [ "${KEEP_ISOLATED_DB:-0}" -eq 1 ]; then
    log "PG: 隔離DB '$isolated_db' を保持します（--keep-isolated-db）"
  else
    psql -v ON_ERROR_STOP=1 -d postgres -tAc \
      "DROP DATABASE \"$isolated_db\"" >/dev/null 2>&1 || {
      fail "PG: 隔離DB '$isolated_db' の DROP に失敗しました"
      return 1
    }
    export ISOLATED_DB_CREATED=0
    log "PG: 隔離DB '$isolated_db' を削除しました（Qdrant と対称）"
  fi

  # --- CONFIRM 時のみ本番反映 ---
  if [ "${CONFIRM:-0}" -eq 1 ]; then
    ts="$(date -u +%Y%m%dT%H%M%SZ)"
    backup_db="${db_name}_restore_backup_${ts}"

    # 旧 DB を退避（コピー）。退避が成功しなければ DROP しない。
    psql -v ON_ERROR_STOP=1 -d postgres -tAc \
      "CREATE DATABASE \"$backup_db\" TEMPLATE \"$db_name\"" >/dev/null 2>&1 || {
      fail "PG: 旧DB の退避に失敗しました。本番反映を中止し DROP は行いません"
      return 1
    }
    log "PG: 旧DB を退避しました: $backup_db"   # 名前のみを log

    # 退避DB の実在を再確認してから DROP / CREATE（退避なしでは DROP しない）
    exists="$(psql -v ON_ERROR_STOP=1 -d postgres -tAc \
      "SELECT 1 FROM pg_database WHERE datname = '$backup_db'" 2>/dev/null)" || {
      fail "PG: 退避DB の確認に失敗しました。DROP は行いません"
      return 1
    }
    if [ -z "$exists" ]; then
      fail "PG: 退避DB が見つかりません。DROP は行いません"
      return 1
    fi

    psql -v ON_ERROR_STOP=1 -d postgres -tAc "DROP DATABASE \"$db_name\"" >/dev/null 2>&1 || {
      # DROP 自体の失敗では本番DB は無傷。二重に壊さないようロールバックしない。
      fail "PG: 旧DB '$db_name' の DROP に失敗しました"
      return 1
    }
    psql -v ON_ERROR_STOP=1 -d postgres -tAc "CREATE DATABASE \"$db_name\"" >/dev/null 2>&1 || {
      # 以降は本番DB が消えた状態。退避DB から再作成してから従来の fail 語彙で終了。
      rollback_from_backup "$backup_db" "$db_name" || true
      fail "PG: 本番DB '$db_name' の CREATE に失敗しました（退避DB '$backup_db' あり）"
      return 1
    }

    if ! gzip -cd "$dump_file" | psql -v ON_ERROR_STOP=1 -d "$db_name" >/dev/null; then
      rollback_from_backup "$backup_db" "$db_name" || true
      fail "PG: 本番DB '$db_name' への復元に失敗しました（退避DB '$backup_db' あり）"
      return 1
    fi

    table_count="$(psql -v ON_ERROR_STOP=1 -d "$db_name" -tAc "$count_sql" 2>/dev/null)" || {
      rollback_from_backup "$backup_db" "$db_name" || true
      fail "PG: 本番DB のテーブル件数取得に失敗しました"
      return 1
    }
    case "$table_count" in
      ''|*[!0-9]*)
        rollback_from_backup "$backup_db" "$db_name" || true
        fail "PG: 本番DB のテーブル件数が不正です"
        return 1 ;;
    esac
    if [ "$table_count" -eq 0 ]; then
      rollback_from_backup "$backup_db" "$db_name" || true
      fail "PG: 本番DB のテーブル件数が 0 です"
      return 1
    fi
    # sqlx マイグレーションの存在と最新 version を照合（本番は strict）。
    if ! verify_sqlx_migrations "$db_name" strict; then
      rollback_from_backup "$backup_db" "$db_name" || true
      fail "PG: 本番DB のマイグレーション検証に失敗しました"
      return 1
    fi
    # 復元後も退避DB が残っていることを確認（失われていたら成功にしない）。
    exists="$(psql -v ON_ERROR_STOP=1 -d postgres -tAc \
      "SELECT 1 FROM pg_database WHERE datname = '$backup_db'" 2>/dev/null)" || {
      fail "PG: 復元後の退避DB 確認に失敗しました"
      return 1
    }
    if [ -z "$exists" ]; then
      fail "PG: 復元後に退避DB が見つかりません"
      return 1
    fi
    log "PG: 本番DB '$db_name' のテーブル数 = $table_count（退避: $backup_db）"
  fi

  return 0
}
