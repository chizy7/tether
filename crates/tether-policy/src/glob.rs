//! Glob matching for rule patterns.
//!
//! `*` matches any run of characters within one path segment, `?` one such
//! character, and `**` any run including `/`. A `**/` also matches nothing, so
//! `**/*.rs` covers `a.rs` as well as `src/a.rs`. Everything else is literal.
//!
//! That is for paths. A tool or server name is not a path, and a server may
//! name a tool anything, `/` included, so [`name_match`] lets `*` and `?`
//! match `/` too: `fs.*` covers every tool of `fs`.
//!
//! The text matched is chosen by the agent and the pattern can come out of a
//! trace, so matching takes time proportional to pattern times text and no
//! stack: a table, not backtracking.

pub fn glob_match(pattern: &str, text: &str) -> bool {
    matches(pattern, text, true)
}

/// As [`glob_match`], but `/` is an ordinary character: `*` and `?` match it.
pub fn name_match(pattern: &str, text: &str) -> bool {
    matches(pattern, text, false)
}

enum Token {
    Literal(char),
    /// `?`
    One,
    /// `*`
    Star,
    /// `**`
    DoubleStar,
}

fn tokens(pattern: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        out.push(match c {
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                Token::DoubleStar
            }
            '*' => Token::Star,
            '?' => Token::One,
            literal => Token::Literal(literal),
        });
    }
    out
}

/// One row per token, filled from the last token backwards: `row[t]` says
/// whether the pattern from that token on matches the text from `t` on. A
/// row needs only the one or two after it.
fn matches(pattern: &str, text: &str, segments: bool) -> bool {
    let pattern = tokens(pattern);
    let text: Vec<char> = text.chars().collect();
    let n = text.len();
    // What stops a single `*` or `?`.
    let stops = |c: char| segments && c == '/';

    // The row past the last token: only the empty text matches.
    let mut next: Vec<bool> = vec![false; n + 1];
    next[n] = true;
    // The row after `next`. Only read where a `**` is followed by a `/`,
    // and then it has been filled.
    let mut after_next: Vec<bool> = vec![false; n + 1];

    for (at, token) in pattern.iter().enumerate().rev() {
        let mut row = vec![false; n + 1];
        match token {
            Token::DoubleStar => {
                // `**/` may also match no directories at all: skip both.
                let zero_dirs = matches!(pattern.get(at + 1), Some(Token::Literal('/')));
                for t in (0..=n).rev() {
                    row[t] = next[t] || (t < n && row[t + 1]) || (zero_dirs && after_next[t]);
                }
            }
            Token::Star => {
                for t in (0..=n).rev() {
                    row[t] = next[t] || (t < n && !stops(text[t]) && row[t + 1]);
                }
            }
            Token::One => {
                for t in 0..n {
                    row[t] = !stops(text[t]) && next[t + 1];
                }
            }
            Token::Literal(literal) => {
                for t in 0..n {
                    row[t] = text[t] == *literal && next[t + 1];
                }
            }
        }
        after_next = std::mem::replace(&mut next, row);
    }
    next[0]
}

#[cfg(test)]
mod tests {
    use super::{glob_match, name_match};

    #[test]
    fn literals_and_single_stars() {
        assert!(glob_match("sandbox.exec", "sandbox.exec"));
        assert!(!glob_match("sandbox.exec", "sandbox.exec2"));
        assert!(glob_match("net.*", "net.fetch"));
        assert!(glob_match("net.*", "net."));
        assert!(!glob_match("net.*", "network.fetch"));
        assert!(glob_match("*.rs", "main.rs"));
        assert!(!glob_match("*.rs", "src/main.rs"));
        assert!(glob_match("src/*/mod.rs", "src/a/mod.rs"));
        assert!(!glob_match("src/*/mod.rs", "src/a/b/mod.rs"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "a/c"));
    }

    #[test]
    fn double_star_crosses_directories() {
        assert!(glob_match("src/**", "src/a.rs"));
        assert!(glob_match("src/**", "src/a/b/c.rs"));
        assert!(!glob_match("src/**", "src"));
        assert!(!glob_match("src/**", "srcs/a.rs"));
        assert!(glob_match("**/*.rs", "a.rs"));
        assert!(glob_match("**/*.rs", "src/deep/a.rs"));
        assert!(!glob_match("**/*.rs", "src/a.txt"));
        assert!(glob_match("**", ""));
        assert!(glob_match("**", "anything/at/all"));
    }

    #[test]
    fn a_name_is_not_a_path() {
        // A server may name a tool anything; a rule for the whole server
        // must still cover it.
        assert!(name_match("evil.*", "evil.admin/delete_all"));
        assert!(!glob_match("evil.*", "evil.admin/delete_all"));
        assert!(name_match("a?c", "a/c"));
        assert!(!name_match("evil.*", "other.x"));
    }

    #[test]
    fn runs_of_stars_and_awkward_patterns() {
        assert!(glob_match("***", "a/b"));
        assert!(glob_match("a/**/b", "a/b"));
        assert!(glob_match("a/**/b", "a/x/y/b"));
        assert!(!glob_match("a/**/b", "a/x/y/c"));
        assert!(glob_match("", ""));
        assert!(!glob_match("", "a"));
        assert!(glob_match("*", ""));
        assert!(!glob_match("?", ""));
    }

    #[test]
    fn a_hostile_pattern_and_text_finish_at_once() {
        // Backtracking takes minutes on this; a long literal used to
        // overflow the stack.
        let stars = "*a".repeat(25) + "*b";
        assert!(!glob_match(&stars, &"a".repeat(40)));
        let long = "x".repeat(1_000);
        assert!(glob_match(&long, &long));
        assert!(!glob_match(&long, &"x".repeat(20_000)));
        assert!(glob_match("**/*.rs", &("a/".repeat(10_000) + "b.rs")));
    }
}
