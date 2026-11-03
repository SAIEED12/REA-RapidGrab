//! Quote-aware splitting for the "Extra yt-dlp args" settings field.
//!
//! The settings UI is a single text field, but yt-dlp takes an argv array, so
//! the text has to be tokenised. Splitting on whitespace alone silently breaks
//! paths that contain spaces (`--cookies "C:\Users\A B\cookies.txt"`), which is
//! exactly the workflow used for sign-in gated videos.
//!
//! Supported syntax:
//! * spaces and tabs separate arguments,
//! * single or double quotes group a run of characters, including spaces,
//! * quotes may start mid-token (`--cookies="a b.txt"`),
//! * inside double quotes `\"` produces a literal quote; every other backslash
//!   is kept as-is so Windows paths survive unchanged,
//! * an unterminated quote consumes the rest of the line.

/// Split `input` into the argument vector passed to yt-dlp.
pub fn split_args(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut has_content = false;
    let mut quote = '\0';
    let mut chars = input.chars();

    while let Some(ch) = chars.next() {
        if quoted {
            if ch == quote {
                quoted = false;
            } else if quote == '"' && ch == '\\' {
                // Only `\"` is special; `\p` must stay `\p` for Windows paths.
                match chars.next() {
                    Some('"') => current.push('"'),
                    Some(next) => {
                        current.push('\\');
                        current.push(next);
                    }
                    None => current.push('\\'),
                }
            } else {
                current.push(ch);
            }
            continue;
        }

        if ch.is_whitespace() {
            if has_content {
                args.push(std::mem::take(&mut current));
                has_content = false;
            }
        } else if ch == '\'' || ch == '"' {
            quote = ch;
            quoted = true;
            has_content = true;
        } else {
            current.push(ch);
            has_content = true;
        }
    }

    if has_content {
        args.push(current);
    }
    args
}

#[cfg(test)]
mod tests {
    use super::split_args;

    #[test]
    fn splits_plain_arguments() {
        assert_eq!(
            split_args("--cookies-from-browser chrome"),
            vec!["--cookies-from-browser", "chrome"]
        );
    }

    #[test]
    fn collapses_repeated_whitespace() {
        assert_eq!(
            split_args("  --a\t\t--b \n --c  "),
            vec!["--a", "--b", "--c"]
        );
        assert!(split_args("   \t\n").is_empty());
    }

    #[test]
    fn keeps_a_quoted_path_with_spaces() {
        assert_eq!(
            split_args(r#"--cookies "C:\Users\Abu Said\cookies.txt""#),
            vec!["--cookies", r"C:\Users\Abu Said\cookies.txt"]
        );
    }

    #[test]
    fn accepts_single_quotes_and_mid_token_quotes() {
        assert_eq!(
            split_args(r#"--cookies='C:\A B\c.txt' --x="a b""#),
            vec!["--cookies=C:\\A B\\c.txt", "--x=a b"]
        );
    }

    #[test]
    fn keeps_empty_quoted_arguments() {
        assert_eq!(split_args(r#"a "" b"#), vec!["a", "", "b"]);
    }

    #[test]
    fn escapes_only_the_double_quote() {
        assert_eq!(
            split_args(r#""a\"b" "c\d""#),
            vec![r#"a"b"#, r"c\d"]
        );
    }

    #[test]
    fn an_unterminated_quote_keeps_the_rest() {
        assert_eq!(
            split_args(r#"--cookies "C:\Users\Abu Said"#),
            vec!["--cookies", r"C:\Users\Abu Said"]
        );
    }
}
