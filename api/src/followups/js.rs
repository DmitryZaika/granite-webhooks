//! JavaScript semantics the follow-up engine was written against.
//!
//! The engine is a port of the CRM's TypeScript heuristics, which are full
//! of regular expression literals. [`js_regex`] turns a JS pattern into the
//! same pattern for Rust's `regex` crate: JS `\b`, `\w` and `\d` are ASCII,
//! so they become ASCII classes; the `i`, `m` and `s` flags carry over. The
//! few patterns with lookaround use [`js_fancy`] (`fancy-regex`). The [`re!`]
//! and [`fre!`] macros compile a literal once, like a JS literal.

use regex::Regex;

/// Rewrites JS-only escapes to their Rust equivalents.
pub fn translate(pattern: &str) -> String {
    translate_with(pattern, r"(?-u:\b)", r"(?-u:\B)")
}

/// ASCII word boundaries spelled with lookaround, for `fancy-regex` (which
/// does not take `(?-u:...)` groups).
const FANCY_WORD_BOUNDARY: &str =
    r"(?:(?<=[0-9A-Za-z_])(?![0-9A-Za-z_])|(?<![0-9A-Za-z_])(?=[0-9A-Za-z_]))";
const FANCY_NOT_WORD_BOUNDARY: &str =
    r"(?:(?<=[0-9A-Za-z_])(?=[0-9A-Za-z_])|(?<![0-9A-Za-z_])(?![0-9A-Za-z_]))";

fn translate_with(pattern: &str, word_boundary: &str, not_word_boundary: &str) -> String {
    let mut out = String::with_capacity(pattern.len() + 16);
    let mut chars = pattern.chars().peekable();
    let mut in_class = false;
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                let Some(next) = chars.next() else {
                    out.push('\\');
                    break;
                };
                match (next, in_class) {
                    ('b', false) => out.push_str(word_boundary),
                    ('B', false) => out.push_str(not_word_boundary),
                    // In a class JS `\b` is a backspace.
                    ('b', true) => out.push_str(r"\x08"),
                    ('w', false) => out.push_str("[0-9A-Za-z_]"),
                    ('W', false) => out.push_str("[^0-9A-Za-z_]"),
                    ('d', false) => out.push_str("[0-9]"),
                    ('D', false) => out.push_str("[^0-9]"),
                    ('w', true) => out.push_str("0-9A-Za-z_"),
                    ('d', true) => out.push_str("0-9"),
                    ('/', _) => out.push('/'),
                    // JS lets any punctuation be escaped; Rust too, except
                    // a few it rejects as unnecessary.
                    ('-', true) => out.push_str(r"\-"),
                    (
                        '\'' | '"' | '`' | '@' | '%' | ',' | '!' | ';' | ':' | '=' | '<' | '>'
                        | '_',
                        _,
                    ) => {
                        out.push(next);
                    }
                    _ => {
                        out.push('\\');
                        out.push(next);
                    }
                }
            }
            '[' if !in_class => {
                in_class = true;
                out.push('[');
                // `[^` and a leading `]` are literal starts in both.
                if chars.peek() == Some(&'^') {
                    out.push('^');
                    chars.next();
                }
            }
            '[' => out.push_str(r"\["),
            ']' if in_class => {
                in_class = false;
                out.push(']');
            }
            // Rust class syntax treats `&&`, `--` and `~~` as operators.
            '&' | '~' if in_class => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

fn with_flags(translated: String, flags: &str) -> String {
    let wanted: String = flags
        .chars()
        .filter(|f| matches!(f, 'i' | 'm' | 's'))
        .collect();
    if wanted.is_empty() {
        translated
    } else {
        format!("(?{wanted}){translated}")
    }
}

/// A JS regular expression literal (`pattern`, `flags`) for the `regex` crate.
pub fn js_regex(pattern: &str, flags: &str) -> Regex {
    Regex::new(&with_flags(translate(pattern), flags))
        .unwrap_or_else(|err| panic!("JS regex /{pattern}/{flags}: {err}"))
}

/// A JS regular expression built at run time (from data); `None` where JS
/// would throw on an invalid pattern.
pub fn js_regex_try(pattern: &str, flags: &str) -> Option<Regex> {
    Regex::new(&with_flags(translate(pattern), flags)).ok()
}

/// JS `String.prototype.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')`.
pub fn escape_reg_exp(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if ".*+?^${}()|[]\\".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A JS regular expression with lookaround, for `fancy-regex`.
pub fn js_fancy(pattern: &str, flags: &str) -> fancy_regex::Regex {
    let translated = translate_with(pattern, FANCY_WORD_BOUNDARY, FANCY_NOT_WORD_BOUNDARY);
    fancy_regex::Regex::new(&with_flags(translated, flags))
        .unwrap_or_else(|err| panic!("JS regex /{pattern}/{flags}: {err}"))
}

/// A compiled-once JS regex literal: `re!(r"\bcall\b", "i")`.
#[macro_export]
macro_rules! re {
    ($pattern:expr) => {
        $crate::re!($pattern, "")
    };
    ($pattern:expr, $flags:expr) => {{
        static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        RE.get_or_init(|| $crate::followups::js::js_regex($pattern, $flags))
    }};
}

/// Like [`re!`], for patterns with lookaround (`fancy-regex`).
#[macro_export]
macro_rules! fre {
    ($pattern:expr) => {
        $crate::fre!($pattern, "")
    };
    ($pattern:expr, $flags:expr) => {{
        static RE: std::sync::OnceLock<fancy_regex::Regex> = std::sync::OnceLock::new();
        RE.get_or_init(|| $crate::followups::js::js_fancy($pattern, $flags))
    }};
}

/// `fancy_regex` `is_match`, false on a backtracking limit like a non-match.
pub fn fancy_test(re: &fancy_regex::Regex, text: &str) -> bool {
    re.is_match(text).unwrap_or(false)
}

/// JS `String.prototype.trim` (Unicode whitespace and line terminators).
pub fn trim(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

/// JS `toLowerCase` for the ASCII-heavy text the engine reads.
pub fn lower(text: &str) -> String {
    text.to_lowercase()
}

/// Length in UTF-16 code units, like JS `.length`.
pub fn js_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// JS `text.slice(start, end)` with UTF-16 indices (clamped).
pub fn js_slice(text: &str, start: usize, end: usize) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    let end = end.min(units.len());
    let start = start.min(end);
    String::from_utf16_lossy(&units[start..end])
}

/// A string enum like a TS union of string literals: `as_str` gives the
/// literal, and it serializes as it.
#[macro_export]
macro_rules! str_enum {
    ($(#[$meta:meta])* $vis:vis enum $name:ident { $($(#[$vmeta:meta])* $variant:ident = $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        $vis enum $name { $($(#[$vmeta])* $variant),+ }
        impl $name {
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }
            pub fn parse(text: &str) -> Option<Self> {
                match text { $($text => Some(Self::$variant),)+ _ => None }
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }
        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let text = <String as serde::Deserialize>::deserialize(d)?;
                Self::parse(&text).ok_or_else(|| serde::de::Error::custom(format!("unknown {}: {text}", stringify!($name))))
            }
        }
    };
}

/// JS `Math.round`: halves go up (towards +∞).
pub fn js_round(x: f64) -> f64 {
    let floor = x.floor();
    if x - floor >= 0.5 { floor + 1.0 } else { floor }
}

/// JS `String(number)` for the values the engine prints.
pub fn js_number(x: f64) -> String {
    if x == 0.0 {
        return "0".to_string();
    }
    if x.is_nan() {
        return "NaN".to_string();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    if x.abs() >= 1e21 || x.abs() < 1e-6 {
        // JS switches to exponent notation here: `1e+21`, `1.5e-7`.
        let formatted = format!("{x:e}");
        return match formatted.split_once('e') {
            Some((mantissa, exponent)) if !exponent.starts_with('-') => {
                format!("{mantissa}e+{exponent}")
            }
            _ => formatted,
        };
    }
    format!("{x}")
}

/// JS `x.toFixed(digits)`: rounds the exact binary value half away from
/// zero (Rust's `{:.N}` rounds half to even).
pub fn js_to_fixed(x: f64, digits: usize) -> String {
    if !x.is_finite() || x.abs() >= 1e21 {
        return js_number(x);
    }
    // The exact expansion is enough to see a tie at this precision.
    let exact = format!("{:.*}", digits + 30, x.abs());
    let (int_part, frac_part) = exact.split_once('.').unwrap_or((exact.as_str(), ""));
    let kept = &frac_part[..digits];
    let round_up = frac_part.as_bytes().get(digits).is_some_and(|d| *d >= b'5');
    let mut number: Vec<u8> = format!("{int_part}{kept}").into_bytes();
    if round_up {
        let mut index = number.len();
        loop {
            if index == 0 {
                number.insert(0, b'1');
                break;
            }
            index -= 1;
            if number[index] == b'9' {
                number[index] = b'0';
            } else {
                number[index] += 1;
                break;
            }
        }
    }
    let text = String::from_utf8(number).unwrap_or_default();
    let (whole, fraction) = text.split_at(text.len() - digits);
    let whole = if whole.is_empty() { "0" } else { whole };
    let body = if digits == 0 {
        whole.to_string()
    } else {
        format!("{whole}.{fraction}")
    };
    if x < 0.0 { format!("-{body}") } else { body }
}

/// JS `Number(text)` for digit strings captured by the engine's patterns;
/// `None` for what JS would make `NaN` (or an empty capture, which JS
/// makes 0 and the engine then rejects).
pub fn js_parse_number(text: &str) -> Option<f64> {
    let text = trim(text);
    if text.is_empty() {
        return None;
    }
    text.parse::<f64>().ok().filter(|x| x.is_finite())
}

/// JS `parseFloat(text)`: the longest decimal prefix after leading
/// whitespace (`"12.5 slabs"` → 12.5); `NaN` when there is none.
pub fn js_parse_float(text: &str) -> f64 {
    let t = text.trim_start_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    let bytes = t.as_bytes();
    let mut end = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    if t[end..].starts_with("Infinity") {
        return if t.starts_with('-') {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    let digits_from = end;
    while bytes.get(end).is_some_and(u8::is_ascii_digit) {
        end += 1;
    }
    let mut digits = end - digits_from;
    if bytes.get(end) == Some(&b'.') {
        let mut frac = end + 1;
        while bytes.get(frac).is_some_and(u8::is_ascii_digit) {
            frac += 1;
        }
        digits += frac - end - 1;
        end = frac;
    }
    if digits == 0 {
        return f64::NAN;
    }
    if matches!(bytes.get(end), Some(b'e' | b'E')) {
        let mut exp = end + 1;
        if matches!(bytes.get(exp), Some(b'+' | b'-')) {
            exp += 1;
        }
        let exp_digits = exp;
        while bytes.get(exp).is_some_and(u8::is_ascii_digit) {
            exp += 1;
        }
        if exp > exp_digits {
            end = exp;
        }
    }
    t[..end]
        .trim_end_matches('.')
        .parse::<f64>()
        .or_else(|_| t[..end].parse::<f64>())
        .unwrap_or(f64::NAN)
}

/// JS `parseInt(text, 10)`: leading digits after whitespace and a sign;
/// `NaN` when there are none.
pub fn js_parse_int(text: &str) -> f64 {
    let t = text.trim_start_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    let (sign, rest) = match t.as_bytes().first() {
        Some(b'-') => (-1.0, &t[1..]),
        Some(b'+') => (1.0, &t[1..]),
        _ => (1.0, t),
    };
    let digits: &str = &rest[..rest.bytes().take_while(u8::is_ascii_digit).count()];
    if digits.is_empty() {
        return f64::NAN;
    }
    sign * digits.parse::<f64>().unwrap_or(f64::NAN)
}

/// `/\s+/g` → single spaces, then trimmed.
pub fn squash(text: &str) -> String {
    trim(&crate::re!(r"\s+", "g").replace_all(text, " ")).to_string()
}

/// JS `text.replace(re, replacement)` (first match; `$1`, `$&` work).
pub fn replace_first(re: &Regex, text: &str, replacement: &str) -> String {
    re.replace(text, js_replacement(replacement).as_str())
        .into_owned()
}

/// JS `text.replace(/.../g, replacement)`.
pub fn replace_all(re: &Regex, text: &str, replacement: &str) -> String {
    re.replace_all(text, js_replacement(replacement).as_str())
        .into_owned()
}

/// A JS replacement string in Rust syntax: `$&` is `${0}`, `$1` is `${1}`, `$$` is `$`.
pub fn js_replacement(replacement: &str) -> String {
    let mut out = String::with_capacity(replacement.len());
    let mut chars = replacement.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.peek().copied() {
            Some('&') => {
                chars.next();
                out.push_str("${0}");
            }
            Some('$') => {
                chars.next();
                out.push_str("$$");
            }
            Some(d) if d.is_ascii_digit() => {
                chars.next();
                out.push_str("${");
                out.push(d);
                out.push('}');
            }
            _ => out.push_str("$$"),
        }
    }
    out
}

/// The UTF-16 index (JS `match.index`) of a byte offset.
pub fn utf16_index(text: &str, byte_index: usize) -> usize {
    text[..byte_index.min(text.len())].encode_utf16().count()
}

/// JS `text.slice(start, end)` given a match's byte offset and UTF-16 deltas:
/// `text.slice(Math.max(0, index - before), index + after)`.
pub fn js_window(text: &str, match_byte_index: usize, before: usize, after: usize) -> String {
    let index = utf16_index(text, match_byte_index);
    js_slice(text, index.saturating_sub(before), index + after)
}

/// JS `Array.prototype.sort` with a comparator, as V8 runs it.
///
/// On short arrays V8 finds one run, then inserts by binary search: stable,
/// and a comparator that returns NaN counts as 0. For consistent comparators any stable sort gives
/// the same order; this one also matches V8 when timestamps fail to parse.
pub fn js_sort_by<T>(items: &mut [T], compare: impl Fn(&T, &T) -> f64) {
    let order = |a: &T, b: &T| {
        let v = compare(a, b);
        if v.is_nan() { 0.0 } else { v }
    };
    let n = items.len();
    if n < 2 {
        return;
    }
    // CountAndMakeRun
    let mut run = 2;
    let descending = order(&items[1], &items[0]) < 0.0;
    while run < n {
        let o = order(&items[run], &items[run - 1]);
        if (descending && o >= 0.0) || (!descending && o < 0.0) {
            break;
        }
        run += 1;
    }
    if descending {
        items[..run].reverse();
    }
    // BinaryInsertionSort
    for start in run..n {
        let (mut left, mut right) = (0, start);
        while left < right {
            let mid = left + ((right - left) >> 1);
            if order(&items[start], &items[mid]) < 0.0 {
                right = mid;
            } else {
                left = mid + 1;
            }
        }
        items[left..=start].rotate_right(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_escapes_become_ascii_classes() {
        assert_eq!(translate(r"\bcall\b"), r"(?-u:\b)call(?-u:\b)");
        assert_eq!(translate(r"[\w-]+\d"), r"[0-9A-Za-z_-]+[0-9]");
        assert_eq!(translate(r"a\/b"), "a/b");
        assert_eq!(translate(r"[^\d,]"), "[^0-9,]");
        assert_eq!(translate("[&~]"), r"[\&\~]");
    }

    #[test]
    fn literals_match_like_js() {
        assert!(re!(r"\bcall\b", "i").is_match("Please CALL me"));
        assert!(!re!(r"\bcall\b", "i").is_match("recall"));
        // JS \w is ASCII: é is a word boundary.
        assert!(re!(r"caf\b", "i").is_match("café"));
        assert!(fancy_test(fre!(r"price(?! list)", "i"), "the price was"));
        assert!(!fancy_test(fre!(r"price(?! list)", "i"), "price list"));
    }

    #[test]
    fn js_lengths_count_utf16_units() {
        assert_eq!(js_len("a😀"), 3);
        assert_eq!(js_slice("hello", 1, 3), "el");
        assert_eq!(trim("\u{feff} hi \n"), "hi");
    }
}
