//! Output scrubbing helpers.
//!
//! The matchers below are hand-written equivalents of the reviewed patterns:
//!
//! - assignment: `(?i)\b([a-z0-9_]*(?:token|secret|password|passwd|api[_-]?key|private[_-]?key)[a-z0-9_]*)=([^\s;&|]+)`
//! - flag:       `(?i)(--?(?:token|secret|password|passwd|api[-_]?key|private[-_]?key)(?:\s+|=))([^\s;&|]+)`
//! - bearer:     `(?i)\b(Bearer\s+)([A-Za-z0-9._~+\-/]+=*)`
//! - formula:    `^[\s\r\n\t]*[=+\-@]`

use crate::pyfmt::{is_space, is_word};

/// Return a path with the home directory shortened unless disabled.
pub fn redact_path_with_home(value: &str, absolute_paths: bool, home: &str) -> String {
    if value.is_empty() || absolute_paths {
        return value.to_string();
    }
    if value == home {
        return "~".to_string();
    }
    let prefix = format!("{home}/");
    if let Some(rest) = value.strip_prefix(&prefix) {
        return format!("~/{rest}");
    }
    value.replace(&prefix, "~/")
}

/// Return a path with the current home directory shortened unless disabled.
pub fn redact_path(value: &str, absolute_paths: bool) -> String {
    redact_path_with_home(value, absolute_paths, &crate::paths::home())
}

/// Redact home paths and obvious inline credential material from text.
pub fn redact_command(value: &str, absolute_paths: bool) -> String {
    redact_command_with_home(value, absolute_paths, &crate::paths::home())
}

/// [`redact_command`] with an explicit home directory.
pub fn redact_command_with_home(value: &str, absolute_paths: bool, home: &str) -> String {
    let redacted = redact_path_with_home(value, absolute_paths, home);
    let redacted = redact_secret_values(&redacted);
    let redacted = redact_secret_flags(&redacted);
    redact_bearer(&redacted)
}

/// Hide the local hostname unless the caller explicitly requests it.
pub fn display_host(hostname: &str, show_hostname: bool) -> String {
    if show_hostname {
        hostname.to_string()
    } else {
        "redacted".to_string()
    }
}

/// Neutralize spreadsheet formula prefixes while preserving the original text.
pub fn sanitize_csv_cell(value: &str) -> String {
    let first = value.chars().find(|c| !is_space(*c));
    if matches!(first, Some('=' | '+' | '-' | '@')) {
        format!("'{value}")
    } else {
        value.to_string()
    }
}

/// One character folded the way Python's `re.IGNORECASE` compares it with an
/// ASCII pattern character: ASCII letters ignore case, and the only
/// non-ASCII characters that match ASCII letters are U+0130 and U+0131
/// (`i`), U+017F (`s`) and U+212A (`k`). Other non-ASCII characters never
/// match an ASCII pattern character.
fn fold(c: char) -> Option<char> {
    match c {
        '\u{0130}' | '\u{0131}' => Some('i'),
        '\u{017f}' => Some('s'),
        '\u{212a}' => Some('k'),
        _ if c.is_ascii() => Some(c.to_ascii_lowercase()),
        _ => None,
    }
}

/// `(?i)[a-z0-9_]`
fn is_name_char(c: char) -> bool {
    fold(c).is_some_and(|f| f.is_ascii_alphanumeric() || f == '_')
}

/// `[^\s;&|]`
fn is_value_char(c: char) -> bool {
    !(is_space(c) || matches!(c, ';' | '&' | '|'))
}

/// `(?i)[A-Za-z0-9._~+\-/]`
fn is_token_char(c: char) -> bool {
    fold(c).is_some_and(|f| {
        f.is_ascii_alphanumeric() || matches!(f, '.' | '_' | '~' | '+' | '-' | '/')
    })
}

/// Case-insensitive match of the lowercase ASCII `literal` at `pos`.
fn literal_at(chars: &[char], pos: usize, literal: &str) -> bool {
    let Some(window) = chars.get(pos..pos + literal.len()) else {
        return false;
    };
    window
        .iter()
        .zip(literal.chars())
        .all(|(c, expected)| fold(*c) == Some(expected))
}

/// Number of consecutive characters from `pos` that satisfy `predicate`.
fn run_len(chars: &[char], pos: usize, predicate: fn(char) -> bool) -> usize {
    chars
        .get(pos..)
        .map_or(0, |rest| rest.iter().take_while(|c| predicate(**c)).count())
}

/// End positions of `token|secret|password|passwd|api[_-]?key|private[_-]?key`
/// starting at `pos`, in the order the regex engine tries them.
fn keyword_ends(chars: &[char], pos: usize) -> Vec<usize> {
    let mut ends = Vec::new();
    for word in ["token", "secret", "password", "passwd"] {
        if literal_at(chars, pos, word) {
            ends.push(pos + word.len());
        }
    }
    for stem in ["api", "private"] {
        if !literal_at(chars, pos, stem) {
            continue;
        }
        let after = pos + stem.len();
        if matches!(chars.get(after), Some('_' | '-')) && literal_at(chars, after + 1, "key") {
            ends.push(after + 4);
        }
        if literal_at(chars, after, "key") {
            ends.push(after + 3);
        }
    }
    ends
}

/// Regex `\b` at `pos` (Python `\w` word characters on both sides).
fn word_boundary(chars: &[char], pos: usize) -> bool {
    let before = pos
        .checked_sub(1)
        .is_some_and(|index| is_word(chars[index]));
    let after = chars.get(pos).is_some_and(|c| is_word(*c));
    before != after
}

/// Apply one pattern left to right like `re.sub`: `matcher` returns the end
/// of the kept prefix and the end of the replaced value for a match at a
/// position, and scanning resumes after each match.
fn substitute(text: &str, matcher: impl Fn(&[char], usize) -> Option<(usize, usize)>) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut position = 0;
    while position < chars.len() {
        match matcher(&chars, position) {
            Some((keep_end, value_end)) => {
                out.extend(&chars[copied..keep_end]);
                out.push_str("<redacted>");
                copied = value_end;
                position = value_end;
            }
            None => position += 1,
        }
    }
    out.extend(&chars[copied..]);
    out
}

/// `\b([a-z0-9_]*(?:KEYWORD)[a-z0-9_]*)=([^\s;&|]+)`, case-insensitive.
fn match_assignment(chars: &[char], start: usize) -> Option<(usize, usize)> {
    if !word_boundary(chars, start) {
        return None;
    }
    let prefix = run_len(chars, start, is_name_char);
    // `[a-z0-9_]*` is greedy, so longer prefixes are tried first.
    for keyword_start in (start..=start + prefix).rev() {
        for keyword_end in keyword_ends(chars, keyword_start) {
            let equals = keyword_end + run_len(chars, keyword_end, is_name_char);
            if chars.get(equals) != Some(&'=') {
                continue;
            }
            let value = run_len(chars, equals + 1, is_value_char);
            if value > 0 {
                return Some((equals + 1, equals + 1 + value));
            }
        }
    }
    None
}

/// `(--?(?:KEYWORD)(?:\s+|=))([^\s;&|]+)`, case-insensitive.
fn match_flag(chars: &[char], start: usize) -> Option<(usize, usize)> {
    if chars.get(start) != Some(&'-') {
        return None;
    }
    for dashes in [2, 1] {
        if dashes == 2 && chars.get(start + 1) != Some(&'-') {
            continue;
        }
        for keyword_end in keyword_ends(chars, start + dashes) {
            let spaces = run_len(chars, keyword_end, is_space);
            let separator_end = if spaces > 0 {
                keyword_end + spaces
            } else if chars.get(keyword_end) == Some(&'=') {
                keyword_end + 1
            } else {
                continue;
            };
            let value = run_len(chars, separator_end, is_value_char);
            if value > 0 {
                return Some((separator_end, separator_end + value));
            }
        }
    }
    None
}

/// `\b(Bearer\s+)([A-Za-z0-9._~+\-/]+=*)`, case-insensitive.
fn match_bearer(chars: &[char], start: usize) -> Option<(usize, usize)> {
    if !word_boundary(chars, start) || !literal_at(chars, start, "bearer") {
        return None;
    }
    let spaces = run_len(chars, start + 6, is_space);
    if spaces == 0 {
        return None;
    }
    let token_start = start + 6 + spaces;
    let token = run_len(chars, token_start, is_token_char);
    if token == 0 {
        return None;
    }
    let padding = run_len(chars, token_start + token, |c| c == '=');
    Some((token_start, token_start + token + padding))
}

fn redact_secret_values(text: &str) -> String {
    substitute(text, match_assignment)
}

fn redact_secret_flags(text: &str) -> String {
    substitute(text, match_flag)
}

fn redact_bearer(text: &str) -> String {
    substitute(text, match_bearer)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/home/tester";

    // tests/test_privacy.py::test_redact_command_hides_common_secret_forms
    #[test]
    fn test_redact_command_hides_common_secret_forms() {
        let command = "TOKEN=abc123 deploy --password hunter2 --api-key=dummy-key-value \
                       -H 'Authorization: Bearer bearer-token'";
        let redacted = redact_command_with_home(command, false, HOME);
        assert!(!redacted.contains("abc123"));
        assert!(!redacted.contains("hunter2"));
        assert!(!redacted.contains("dummy-key-value"));
        assert!(!redacted.contains("bearer-token"));
        assert!(redacted.contains("TOKEN=<redacted>"));
        assert!(redacted.contains("--password <redacted>"));
        assert!(redacted.contains("--api-key=<redacted>"));
        assert!(redacted.contains("Bearer <redacted>"));
    }

    // tests/test_privacy.py::test_redact_path_shortens_home_by_default
    #[test]
    fn test_redact_path_shortens_home_by_default() {
        let home = crate::paths::home();
        let home_path = format!("{home}/project");
        assert_eq!(redact_path(&home_path, false), "~/project");
        assert_eq!(redact_path(&home_path, true), home_path);
    }

    #[test]
    fn exact_redaction_outputs_match_python() {
        // Expected strings were produced by the Python 0.3.0 implementation.
        let cases = [
            (
                "TOKEN=abc123 deploy --password hunter2 --api-key=dummy-key-value -H 'Authorization: Bearer bearer-token'",
                "TOKEN=<redacted> deploy --password <redacted> --api-key=<redacted> -H 'Authorization: Bearer <redacted>'",
            ),
            ("api-key-token=x", "api-key-token=<redacted>"),
            ("x-token=v", "x-token=<redacted>"),
            ("éTOKEN=x", "éTOKEN=x"),
            ("---token=x", "---token=<redacted>"),
            ("--tokens=x", "--tokens=<redacted>"),
            ("-token   x;y", "-token   <redacted>;y"),
            ("--token ;x", "--token ;x"),
            ("TOKEN= x", "TOKEN= x"),
            ("bearer   abc==tail", "bearer   <redacted>tail"),
            ("xBearer abc", "xBearer abc"),
            (
                "MY_SECRET_VAR=1&&PRIVATE-KEY=2|API_KEY=3",
                "MY_SECRET_VAR=<redacted>&&PRIVATE-KEY=<redacted>|API_KEY=<redacted>",
            ),
            ("a=b token=c=d", "a=b token=<redacted>"),
            (
                "/home/tester/bin/run --secret=/home/tester/x",
                "~/bin/run --secret=<redacted>",
            ),
            (
                "/home/testerX/y /home/tester",
                "/home/testerX/y /home/tester",
            ),
            ("--api_key  v", "--api_key  <redacted>"),
            (
                "pass --passwd=x --private-key k",
                "pass --passwd=<redacted> --private-key <redacted>",
            ),
            ("Bearer\tabc/+_~.-==x", "Bearer\t<redacted>x"),
            ("SECRET=a;TOKEN=b", "SECRET=<redacted>;TOKEN=<redacted>"),
            ("tok=--token=abc", "tok=--token=<redacted>"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                redact_command_with_home(input, false, HOME),
                expected,
                "{input}"
            );
        }
    }

    #[test]
    fn non_ascii_text_matches_python_regex_semantics() {
        // Expected strings were produced by the Python 0.3.0 implementation.
        // Multi-byte characters near a match must not panic, `(?i)` lets
        // U+0130/U+0131 match `i`, U+017F match `s` and U+212A match `k`,
        // and `\b` uses Python's `str.isalnum()` word characters.
        let cases = [
            ("\u{1f600}\u{1f600}", "\u{1f600}\u{1f600}"),
            ("\u{fc}Bearer x", "\u{fc}Bearer x"),
            ("pyth\u{f6}n --token=v", "pyth\u{f6}n --token=<redacted>"),
            ("\u{17f}Token=x", "\u{17f}Token=<redacted>"),
            ("\u{212a}api_key=v", "\u{212a}api_key=<redacted>"),
            ("\u{130}=x API\u{131}KEY=v", "\u{130}=x API\u{131}KEY=v"),
            ("x \u{5b0}api_key=v", "x \u{5b0}api_key=<redacted>"),
            ("\u{24b6}TOKEN=v", "\u{24b6}TOKEN=<redacted>"),
            (".BEARER\n\u{130}", ".BEARER\n<redacted>"),
            ("Bearer \u{212a}=", "Bearer <redacted>"),
            ("--pa\u{17f}sword s3cret", "--pa\u{17f}sword <redacted>"),
            ("TO\u{212a}EN=abc", "TO\u{212a}EN=<redacted>"),
            ("\u{2160}TOKEN=x", "\u{2160}TOKEN=x"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                redact_command_with_home(input, false, HOME),
                expected,
                "{input:?}"
            );
        }
    }

    #[test]
    fn csv_sanitizer_rules() {
        assert_eq!(sanitize_csv_cell(" \t=CMD()"), "' \t=CMD()");
        assert_eq!(sanitize_csv_cell("\u{2003}-1"), "'\u{2003}-1");
        assert_eq!(sanitize_csv_cell("a=b"), "a=b");
        assert_eq!(sanitize_csv_cell(""), "");
    }
}
