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
# 定義する関数は restore_pg() のみ（引数なし、戻り値 0/1）。
# 実 DB 操作は APPLY=1 のときだけ行う。入力は $SET_DIR/postgres-jarvis.sql.gz。
# 設定: DB_NAME=jarvis / 隔離 DB=jarvis_restore_test。
# 出力・ログには DB 名とテーブル件数（数値）のみを出し、テーブル内容・行データ・
# 接続情報（秘密情報）は一切出力しない。
#
# 後始末ポリシー: 既定では隔離 DB の検証直後に DROP し、Qdrant の隔離
# コレクション削除と対称に本番データのコピーを残さない。保持する場合は
# 呼び出し側が --keep-isolated-db を明示する（KEEP_ISOLATED_DB=1）。
# CREATE 成功後 DROP 完了前に異常終了した分は restore-local.sh の EXIT trap が
# ISOLATED_DB_CREATED を見て片付ける（既存のサービス再起動 trap は変更しない）。

restore_pg() {
  local db_name="jarvis"
  local isolated_db="jarvis_restore_test"
  local backup_db="" ts="" exists="" table_count="" dump_file=""
  local count_sql="SELECT count(*) FROM information_schema.tables WHERE table_schema NOT IN ('pg_catalog', 'information_schema')"

  # 共通インターフェースの必須確認（呼び出し側の set -u でも落ちないよう ${...:-} を使う）
  if [ -z "${SET_DIR:-}" ] || ! declare -F log >/dev/null 2>&1 || ! declare -F fail >/dev/null 2>&1; then
    echo "ERROR: restore_pg requires SET_DIR, log() and fail() from the caller" >&2
    return 1
  fi
  dump_file="$SET_DIR/postgres-jarvis.sql.gz"

  # --- APPLY=0: 計画のみ。DB 接続を含む書き込みを一切行わない ---
  if [ "${APPLY:-0}" -ne 1 ]; then
    log "PG: dry-run (APPLY=0) — 書き込みゼロ"
    log "PG: 入力 = $dump_file"
    log "PG: 隔離DB '$isolated_db' へ gzip -cd | psql -v ON_ERROR_STOP=1 で復元する計画"
    log "PG: information_schema.tables の件数を検証する計画（件数のみ記録）"
    if [ "${KEEP_ISOLATED_DB:-0}" -eq 1 ]; then
      log "PG: [計画] 隔離DB '$isolated_db' は --keep-isolated-db により検証後も保持"
    else
      log "PG: [計画] 隔離DB '$isolated_db' は検証後に DROP（Qdrant と対称）"
    fi
    if [ "${CONFIRM:-0}" -eq 1 ]; then
      log "PG: [計画] CONFIRM あり — 旧DB '$db_name' を退避 → DROP/CREATE → 再復元 → 再検証"
    else
      log "PG: [計画] 本番DB '$db_name' は触らない（CONFIRM なし）"
    fi
    return 0
  fi

  [ -f "$dump_file" ] || { fail "PG: バックアップが見つかりません: $dump_file"; return 1; }

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
      fail "PG: 旧DB '$db_name' の DROP に失敗しました"
      return 1
    }
    psql -v ON_ERROR_STOP=1 -d postgres -tAc "CREATE DATABASE \"$db_name\"" >/dev/null 2>&1 || {
      fail "PG: 本番DB '$db_name' の CREATE に失敗しました（退避DB '$backup_db' あり）"
      return 1
    }

    if ! gzip -cd "$dump_file" | psql -v ON_ERROR_STOP=1 -d "$db_name" >/dev/null; then
      fail "PG: 本番DB '$db_name' への復元に失敗しました（退避DB '$backup_db' あり）"
      return 1
    fi

    table_count="$(psql -v ON_ERROR_STOP=1 -d "$db_name" -tAc "$count_sql" 2>/dev/null)" || {
      fail "PG: 本番DB のテーブル件数取得に失敗しました"
      return 1
    }
    case "$table_count" in
      ''|*[!0-9]*) fail "PG: 本番DB のテーブル件数が不正です"; return 1 ;;
    esac
    if [ "$table_count" -eq 0 ]; then
      fail "PG: 本番DB のテーブル件数が 0 です"
      return 1
    fi
    log "PG: 本番DB '$db_name' のテーブル数 = $table_count（退避: $backup_db）"
  fi

  return 0
}
