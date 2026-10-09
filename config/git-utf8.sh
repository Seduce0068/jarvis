#!/usr/bin/env bash
# Git の文字化け防止設定
# 実行: source config/git-utf8.sh

# コミットメッセージ・ログ・diff を UTF-8 で扱う
git config --global i18n.commitencoding utf-8
git config --global i18n.logoutputencoding utf-8

# ファイル名の文字化け防止（日本語ファイル名対応）
git config --global core.quotepath false

# ページャで文字化けさせない
git config --global core.pager "less -R"

# diff/highlight の色付け
git config --global color.ui auto
git config --global color.diff auto
git config --global color.status auto
git config --global color.branch auto
git config --global color.grep auto

# コミット時のエディタで UTF-8 を使う
git config --global core.editor "vim -c 'set fenc=utf-8'"

# 確認用
echo "=== Git i18n 設定 ==="
git config --global --get-regexp '^i18n\.'
echo ""
echo "=== core.quotepath ==="
git config --global --get core.quotepath