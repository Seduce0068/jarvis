#!/usr/bin/env bash
# シェル共通のロケール設定（UTF-8 強制）
# ~/.bashrc または ~/.zshrc に source して使用

# ロケールを UTF-8 に固定
export LANG=ja_JP.UTF-8
export LC_ALL=ja_JP.UTF-8
export LC_CTYPE=ja_JP.UTF-8
export LC_MESSAGES=ja_JP.UTF-8

# 文字化け防止: man ページ等のページャ設定
export PAGER=less
export LESS='-R -i -M --shift 5'

# 端末の文字コード確認用エイリアス
alias check-locale='locale | sort'
alias check-encoding='echo -e "ひらがな カタカナ 漢字 🎌" && printf "UTF-8: \xE3\x81\x82\n"'