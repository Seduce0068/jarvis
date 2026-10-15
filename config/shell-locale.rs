//! Emit shell code for UTF-8 locale setup in the current interactive shell.
fn main() {
    print!(
        r#"export LANG=ja_JP.UTF-8
export LC_ALL=ja_JP.UTF-8
export LC_CTYPE=ja_JP.UTF-8
export LC_MESSAGES=ja_JP.UTF-8
export PAGER=less
export LESS='-R -i -M --shift 5'
alias check-locale='locale | sort'
alias check-encoding='printf "%s\nUTF-8: %s\n" "ひらがな カタカナ 漢字 🎌" "あ"'
"#
    );
}
