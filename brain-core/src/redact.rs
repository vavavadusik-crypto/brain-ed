//! Secret masking for stored text, byte-for-byte the same as the archive's
//! `tools/session_archive.py::_mask_secrets` (mask:3: SECRET_PATTERNS +
//! KEYED_SECRET/SPOKEN_SECRET/FLAG_SECRET with their decision helpers).
//!
//! The shared test vectors live in `tests/secret_cases.json`; both sides read
//! that file, so the two maskers are pinned byte-for-byte.
//!
//! The `regex` crate has no lookaround or backreferences, so the Python
//! lookarounds (`(?!\[REDACTED\])`, `(?!\d+\b)`, `(?<!no)passwd`,
//! `(?<![\w-])--?`, `(?<!\S)`) and the `(?P=quote)` backreference are
//! implemented as explicit code: patterns match a slightly wider span, and a
//! manual match loop validates the context — either rewriting the span or
//! advancing the search position exactly as the Python engine's backtracking
//! would. The quoted-value branch of KEYED_SECRET is one of those cases: the
//! same-quote closing is checked after the match, and a differently-quoted
//! line means no match at all, just as in Python.

use regex::Regex;

/// How a bare-pattern match is rewritten.
#[derive(Clone, Copy)]
enum Shape {
    /// The whole match becomes `[REDACTED]` (bare tokens, PEM blocks).
    Whole,
    /// The named `keep` prefix stays unless the rest is already masked
    /// (`Bearer …`, `Basic …`).
    Prefixed,
    /// `keep` + `[REDACTED]` + `tail`: `scheme://user:` … `@host` in a URL.
    UrlPassword,
}

const MASK_TABLE: [(&str, Shape); 10] = [
    (
        r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?(?:-----END [A-Z ]*PRIVATE KEY-----|$)",
        Shape::Whole,
    ),
    (
        r"\b(?:sk|rk|pk)-(?:ant-|proj-|live-|test-)?[A-Za-z0-9_\-]{16,}",
        Shape::Whole,
    ),
    (
        r"\b(?:ghp|gho|ghu|ghs|ghr|github_pat)_[A-Za-z0-9_]{20,}",
        Shape::Whole,
    ),
    (r"\bxox[abposr]-[A-Za-z0-9\-]{10,}", Shape::Whole),
    (r"\bAKIA[0-9A-Z]{16}\b", Shape::Whole),
    (
        r"\b[A-Za-z]{1,16}_(?:live|test|prod)_[A-Za-z0-9]{24,}",
        Shape::Whole,
    ),
    (r"\bAIza[0-9A-Za-z_\-]{30,}", Shape::Whole),
    (
        r"\beyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}",
        Shape::Whole,
    ),
    (
        r"(?i)(?P<keep>\b(?:bearer|basic)\s+)(?P<value>[A-Za-z0-9_\-\.\+/=]{12,})",
        Shape::Prefixed,
    ),
    // The scheme and user are bounded (an unbounded scheme made a long
    // "a-a-a-…" line quadratic); the secret is not.
    (
        r"(?P<keep>\b[a-z][a-z0-9+.\-]{0,30}://[^\s:/@]{1,128}:)(?P<value>[^\s/]+)(?P<tail>@)",
        Shape::UrlPassword,
    ),
];

fn bare_rules() -> &'static [(Regex, Shape)] {
    static BUILT: std::sync::OnceLock<Vec<(Regex, Shape)>> = std::sync::OnceLock::new();
    BUILT.get_or_init(|| {
        MASK_TABLE
            .iter()
            .map(|(pattern, shape)| (Regex::new(pattern).expect("static mask pattern"), *shape))
            .collect()
    })
}

/// `\$\{?[A-Za-z_]\w*\}?` — `$VAR` / `${VAR}`: a reference, not the value.
fn variable_reference(inner: &str) -> bool {
    let Some(rest) = inner.strip_prefix('$') else {
        return false;
    };
    let (braced, name) = match rest.strip_prefix('{') {
        Some(without) => (true, without),
        None => (false, rest),
    };
    let name = if braced {
        name.strip_suffix('}').unwrap_or("")
    } else {
        name
    };
    !name.is_empty()
        && name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `[A-Za-z_][A-Za-z_]*(?:\.[A-Za-z_][A-Za-z_]*)*` — a dotted code reference.
fn code_reference(inner: &str) -> bool {
    !inner.is_empty()
        && inner.split('.').all(|segment| {
            !segment.is_empty()
                && segment.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                && segment.chars().all(|c| c.is_ascii_alphabetic() || c == '_')
        })
}

/// `[A-Z][A-Z0-9_]*` — an environment variable name after a flag.
fn env_name(core: &str) -> bool {
    core.starts_with(|c: char| c.is_ascii_uppercase())
        && core
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// `[A-Za-z0-9_\-+/=.!@#$%^&*~]+`
fn generated_value_shape(core: &str) -> bool {
    core.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(
                c,
                '_' | '-'
                    | '+'
                    | '/'
                    | '='
                    | '.'
                    | '!'
                    | '@'
                    | '#'
                    | '$'
                    | '%'
                    | '^'
                    | '&'
                    | '*'
                    | '~'
            )
    })
}

/// `[A-Za-z0-9_\-+/=]+`
fn base64_shape(core: &str) -> bool {
    core.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '/' | '='))
}

/// Values that name a type or state rather than hold a secret.
fn is_not_secret(lower: &str) -> bool {
    matches!(
        lower,
        "true"
            | "false"
            | "null"
            | "none"
            | "nil"
            | "undefined"
            | "string"
            | "str"
            | "int"
            | "integer"
            | "number"
            | "bool"
            | "boolean"
            | "required"
            | "optional"
            | "example"
            | "redacted"
            | "[redacted]"
            | "***"
            | "..."
    )
}

/// `[REDACTED]` in the value's own quotes; an unclosed quote stays unclosed.
fn quoted_redaction(value: &str) -> String {
    let Some(first) = value.chars().next() else {
        return "[REDACTED]".to_owned();
    };
    match first {
        '"' | '\'' => {
            let closing = if value.len() > 1 && value.ends_with(first) {
                first.to_string()
            } else {
                String::new()
            };
            format!("{first}[REDACTED]{closing}")
        }
        _ => "[REDACTED]".to_owned(),
    }
}

/// The `(?<!no)passwd` exclusion: when the keyword the regex matched is
/// `passwd` and the two characters of the original text before it are `no`,
/// the Python engine finds no match at this position at all.
fn keyword_blocked_by_no(text: &str, keep: &str, match_start: usize) -> bool {
    let lower = keep.to_lowercase();
    let Some(offset) = KEYWORD_NEEDLES
        .iter()
        .filter_map(|needle| lower.find(needle))
        .min()
    else {
        return false;
    };
    let Some(keyword) = KEYWORD_NEEDLES
        .iter()
        .find(|needle| lower[offset..].starts_with(*needle))
    else {
        return false;
    };
    *keyword == "passwd"
        && match_start + offset >= 2
        && text[match_start + offset - 2..match_start + offset].eq_ignore_ascii_case("no")
}

const KEYWORD_NEEDLES: [&str; 9] = [
    "password",
    "passwd",
    "passphrase",
    "secret",
    "token",
    "api_key",
    "api-key",
    "access_key",
    "access-key",
];

/// `\b[A-Za-z0-9_]{0,64}?(?:password|passwd|passphrase|secret|token|api[_-]?key|access[_-]?key|private[_-]?key)(?:[_-](?:key|value))?['\"]?[ \t]*[:=][ \t]*(?:[|>][-+]?[ \t]*\n[ \t]*)?`
const KEYED_KEEP: &str = concat!(
    r#"(?i)(?P<keep>\b[A-Za-z0-9_]{0,64}?(?:password|passwd|passphrase|secret|token|api[_-]?key"#,
    r#"|access[_-]?key|private[_-]?key)(?:[_-](?:key|value))?['"]?[ \t]*[:=][ \t]*"#,
    r#"(?:[|>][-+]?[ \t]*\n[ \t]*)?)"#,
);

/// The quoted branch of the KEYED value: one quote, 1+ characters without
/// quotes or newlines. The Python pattern then requires the same quote or the
/// end of the line; the span is corrected in code after the match.
fn keyed_quoted_pattern() -> String {
    format!("{KEYED_KEEP}(?P<quote>['\"])(?P<body>[^'\"\n]+)")
}

/// The unquoted branch: printable ASCII minus quotes, commas, semicolons,
/// ampersands and all bracket characters.
fn keyed_unquoted_pattern() -> String {
    // The class is a raw string so the regex escapes (`\-`, `\^`, `\\`)
    // survive the Rust literal.
    format!(
        r#"{}(?P<value>[!#$%*+\-./0-9:<>?@A-Z\\^_a-z{{|}}~]+)"#,
        KEYED_KEEP
    )
}

/// `(?:(?<![\w-])--?)?\bSECRET_NAME[ \t]+` plus the spoken value; the dash
/// lookbehind is validated in the match loop.
const SPOKEN_PATTERN: &str = concat!(
    r#"(?i)(?P<dash>--)?(?P<keep>\b(?:password|passwd|passphrase|secret|token"#,
    r#"|api[_-]?key|access[_-]?key|private[_-]?key)[ \t]+)(?P<value>[!-~]{4,})"#,
);

/// mysql-style `-p'secret'` / `-p"secret"`; the `(?<!\S)` lookbehind is
/// validated in the match loop.
const FLAG_PATTERN: &str = r#"(?P<keep>-p)(?P<value>'[^'
]+'|"[^"
]+")"#;

fn keyed_quoted() -> &'static Regex {
    static BUILT: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    BUILT.get_or_init(|| Regex::new(&keyed_quoted_pattern()).expect("keyed quoted pattern"))
}

fn keyed_unquoted() -> &'static Regex {
    static BUILT: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    BUILT.get_or_init(|| Regex::new(&keyed_unquoted_pattern()).expect("keyed unquoted pattern"))
}

fn spoken() -> &'static Regex {
    static BUILT: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    BUILT.get_or_init(|| Regex::new(SPOKEN_PATTERN).expect("spoken pattern"))
}

fn flag() -> &'static Regex {
    static BUILT: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    BUILT.get_or_init(|| Regex::new(FLAG_PATTERN).expect("flag pattern"))
}

/// Masks every recognised credential shape in `text`.
///
/// Returns the masked text and whether anything changed. Masking is
/// idempotent: `mask_secrets(mask_secrets(t).0).0 == mask_secrets(t).0`.
pub fn mask_secrets(text: &str) -> (String, bool) {
    let mut changed = false;
    let mut current = text.to_owned();
    for (pattern, shape) in bare_rules() {
        let next = replace_bare(&current, pattern, *shape);
        if next != current {
            changed = true;
        }
        current = next;
    }
    for step in [mask_keyed, mask_spoken, mask_flag] {
        let next = step(&current);
        if next != current {
            changed = true;
        }
        current = next;
    }
    (current, changed)
}

/// Sequential `replace_all` for the bare patterns with their code checks.
fn replace_bare(text: &str, pattern: &Regex, shape: Shape) -> String {
    let mut result = String::with_capacity(text.len());
    let mut last = 0;
    for captures in pattern.captures_iter(text) {
        let whole = captures.get(0).expect("group 0");
        result.push_str(&text[last..whole.start()]);
        let keep = captures.name("keep").map(|m| m.as_str()).unwrap_or("");
        let value = captures.name("value").map(|m| m.as_str()).unwrap_or("");
        let tail = captures.name("tail").map(|m| m.as_str()).unwrap_or("");
        match shape {
            Shape::Whole => result.push_str("[REDACTED]"),
            Shape::Prefixed => {
                if value.starts_with("[REDACTED]") {
                    result.push_str(whole.as_str());
                } else {
                    result.push_str(&format!("{keep}[REDACTED]"));
                }
            }
            Shape::UrlPassword => {
                if value.starts_with("[REDACTED]") {
                    result.push_str(whole.as_str());
                } else {
                    result.push_str(&format!("{keep}[REDACTED]{tail}"));
                }
            }
        }
        last = whole.end();
    }
    result.push_str(&text[last..]);
    result
}

/// A quoted KEYED value span: the opening quote, the body, and the closing
/// quote when the same quote character sits right after the body.
fn quoted_value_span(
    text: &str,
    body_start: usize,
    body_end: usize,
    quote: char,
) -> (usize, usize, bool) {
    let closing = text.as_bytes().get(body_end) == Some(&(quote as u8));
    let start = body_start - quote.len_utf8();
    let end = if closing {
        body_end + quote.len_utf8()
    } else {
        body_end
    };
    (start, end, closing)
}

/// Applies `KEYED_SECRET` with `_keyed_secret`'s decisions. Two regexes cover
/// the value alternation (quoted / unquoted); the manual loop takes the
/// leftmost match of either, exactly like the Python engine.
fn mask_keyed(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut pos = 0;
    // The cursor moves inside the body (skipped spans, closing quotes), so a
    // plain `while let` over captures_at(pos) does not express the progress.
    #[allow(clippy::while_let_loop)]
    loop {
        let quoted = keyed_quoted().captures_at(text, pos);
        let unquoted = keyed_unquoted().captures_at(text, pos);
        let pick = match (quoted, unquoted) {
            (Some(q), Some(u)) => {
                if q.get(0).unwrap().start() <= u.get(0).unwrap().start() {
                    (q, true)
                } else {
                    (u, false)
                }
            }
            (Some(q), None) => (q, true),
            (None, Some(u)) => (u, false),
            (None, None) => break,
        };
        let captures = pick.0;
        let whole = captures.get(0).unwrap();
        let keep = captures.name("keep").unwrap().as_str();
        if pick.1 {
            // Quoted value: decide with `_keyed_secret`.
            let quote = captures.name("quote").unwrap().as_str();
            let body = captures.name("body").unwrap();
            let (value_start, value_end, closing) = quoted_value_span(
                text,
                body.start(),
                body.end(),
                quote.chars().next().unwrap(),
            );
            let raw = &text[value_start..value_end];
            let inner = &raw[1..raw.len() - if closing { 1 } else { 0 }];
            let after = text[value_end..].chars().next();
            let skip = inner.chars().count() < 4
                || is_not_secret(&inner.to_lowercase())
                || inner.chars().all(|c| c.is_ascii_digit())
                || variable_reference(inner)
                || (!closing
                    && code_reference(inner)
                    && (inner.contains('.') || after.is_some_and(|c| "([.".contains(c))));
            if skip {
                result.push_str(&text[pos..whole.end().max(pos)]);
                pos = whole.end().max(pos);
            } else {
                result.push_str(&text[pos..whole.start()]);
                result.push_str(keep);
                result.push_str(&quoted_redaction(raw));
                // The rewritten span includes the closing quote the regex did
                // not consume, so the cursor moves past it too.
                pos = value_end;
            }
        } else {
            // Unquoted value.
            let value = captures.name("value").unwrap();
            let after = text[value.end()..].chars().next();
            let skip = value.as_str().chars().count() < 4
                || is_not_secret(&value.as_str().to_lowercase())
                || value.as_str().chars().all(|c| c.is_ascii_digit())
                || variable_reference(value.as_str())
                || (code_reference(value.as_str())
                    && (value.as_str().contains('.') || after.is_some_and(|c| "([.".contains(c))))
                || keyword_blocked_by_no(text, keep, whole.start());
            if skip {
                result.push_str(&text[pos..whole.end().max(pos)]);
                pos = whole.end().max(pos);
            } else {
                result.push_str(&text[pos..whole.start()]);
                result.push_str(keep);
                result.push_str("[REDACTED]");
                pos = whole.end();
            }
        }
    }
    result.push_str(&text[pos..]);
    result
}

/// Applies `SPOKEN_SECRET` with `_spoken_secret`'s decisions.
fn mask_spoken(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut pos = 0;
    #[allow(clippy::while_let_loop)]
    loop {
        let captures = match spoken().captures_at(text, pos) {
            Some(captures) => captures,
            None => break,
        };
        let whole = captures.get(0).unwrap();
        let keep = captures.name("keep").unwrap().as_str();
        let value = captures.name("value").unwrap().as_str();
        let dashed = captures.name("dash").is_some();
        let blocked = dashed
            && whole
                .start()
                .checked_sub(1)
                .and_then(|index| text.as_bytes().get(index))
                .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_');
        if blocked {
            // The Python engine drops the optional dashes and retries from the
            // next character: continue the search one byte in.
            pos = whole.start() + 1;
            continue;
        }
        let dashes = captures.name("dash").map(|m| m.as_str()).unwrap_or("");
        let core = value.trim_end_matches(['.', ',', ':', ';', ')', ']', '}', '"', '\'', '`', '*']);
        let tail = &value[core.len()..];
        // Python's keep includes the leading dashes, so the flag check and
        // the replacement do too.
        let flag = dashed || keep.starts_with('-');
        let generated = (core.len() >= 8
            && core.chars().any(|c| c.is_ascii_digit())
            && core.chars().any(|c| c.is_ascii_alphabetic())
            && generated_value_shape(core))
            || (core.len() >= 16 && base64_shape(core));
        let skip = core.is_empty()
            || value.starts_with('-')
            || value.starts_with("[REDACTED]")
            || value.starts_with('$')
            || core.contains('\\')
            || is_not_secret(core.to_lowercase().trim_matches(['\'', '"']))
            || !(flag || generated)
            || env_name(core)
            || core.starts_with('<')
            || core.starts_with('{');
        if skip {
            result.push_str(&text[pos..whole.end().max(pos)]);
            pos = whole.end().max(pos);
        } else {
            result.push_str(&text[pos..whole.start()]);
            result.push_str(dashes);
            result.push_str(keep);
            result.push_str("[REDACTED]");
            result.push_str(tail);
            pos = whole.end();
        }
    }
    result.push_str(&text[pos..]);
    result
}

/// Applies `FLAG_SECRET`: mysql-style `-p'secret'` / `-p"secret"`.
fn mask_flag(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut pos = 0;
    #[allow(clippy::while_let_loop)]
    loop {
        let captures = match flag().captures_at(text, pos) {
            Some(captures) => captures,
            None => break,
        };
        let whole = captures.get(0).unwrap();
        let preceded_by_nonspace = whole
            .start()
            .checked_sub(1)
            .and_then(|index| text.as_bytes().get(index))
            .is_some_and(|byte| !byte.is_ascii_whitespace());
        if preceded_by_nonspace {
            pos = whole.start() + 1;
            continue;
        }
        let value = captures.name("value").unwrap().as_str();
        result.push_str(&text[pos..whole.start()]);
        result.push_str("-p");
        result.push_str(&quoted_redaction(value));
        pos = whole.end();
    }
    result.push_str(&text[pos..]);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glue(parts: &[serde_json::Value]) -> String {
        parts
            .iter()
            .map(|part| part.as_str().expect("string part"))
            .collect()
    }

    #[test]
    fn shared_secret_vectors_match_byte_for_byte() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/secret_cases.json");
        let cases: Vec<serde_json::Value> =
            serde_json::from_str(&std::fs::read_to_string(path).expect("vectors file"))
                .expect("vectors parse");
        assert!(!cases.is_empty(), "the shared vector file is not empty");
        for case in &cases {
            let name = case["name"].as_str().expect("case name");
            let input = glue(case["parts"].as_array().expect("parts"));
            let expected = glue(case["expected_parts"].as_array().expect("expected_parts"));
            let (masked, changed) = mask_secrets(&input);
            assert_eq!(masked, expected, "{name}");
            assert_eq!(changed, input != expected, "{name}: changed flag");
            // Idempotency: the masked text never changes again.
            let (again, changed_again) = mask_secrets(&masked);
            assert_eq!(again, expected, "{name}: idempotent");
            assert!(!changed_again, "{name}: second pass is a no-op");
        }
    }

    #[test]
    fn megabyte_inputs_stay_linear() {
        // Linear, not merely fast on an idle machine: 8x the input may cost at most 16x the
        // time (a quadratic pattern costs ~64x). The absolute bound only catches a hang, so a
        // loaded CI or a full parallel `cargo test` cannot make this flaky.
        let time = |text: &str| {
            let started = std::time::Instant::now();
            let (masked, _) = mask_secrets(text);
            (started.elapsed(), masked)
        };
        for (name, chunk) in [
            ("bare-ish", "a-"),
            ("keyed", "password "),
            ("spoken", "token "),
            ("url", "a://b:"),
        ] {
            let small = chunk.repeat((1 << 17) / chunk.len() + 1);
            let text = chunk.repeat((1 << 20) / chunk.len() + 1);
            let (small_elapsed, _) = time(&small);
            let (elapsed, masked) = time(&text);
            let ratio = elapsed.as_secs_f64() / small_elapsed.as_secs_f64().max(1e-4);
            assert!(
                ratio < 16.0 && elapsed.as_secs() < 30,
                "{name}: 128 KiB took {small_elapsed:?}, 1 MiB took {elapsed:?} (x{ratio:.1}), must stay linear"
            );
            assert!(!masked.is_empty(), "{name}");
        }
    }
}
