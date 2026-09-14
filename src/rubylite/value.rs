//! Ruby literal arguments: enough of the grammar for formula and cask DSL calls.
//!
//! Handles strings (with `#{...}` interpolation against a caller-supplied
//! variable table), symbols, integers, booleans, arrays, hashes in both the
//! `key: value` and `key => value` spellings, and `bin/"foo"` path joins.

use std::collections::BTreeMap;

use serde_json::{Map, Value as Json};

/// Values the DSL can carry.
#[derive(Debug, Clone, PartialEq)]
pub enum RValue {
    Str(String),
    Sym(String),
    Int(i64),
    Bool(bool),
    Nil,
    Array(Vec<RValue>),
    Hash(Vec<(String, RValue)>),
    /// An expression rubylite could not resolve, kept verbatim.
    Expr(String),
}

impl RValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            RValue::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_sym(&self) -> Option<&str> {
        match self {
            RValue::Sym(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            RValue::Int(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            RValue::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// A string, whatever the literal kind (symbols lose their colon).
    pub fn to_text(&self) -> Option<String> {
        match self {
            RValue::Str(s) => Some(s.clone()),
            RValue::Sym(s) => Some(s.clone()),
            RValue::Int(i) => Some(i.to_string()),
            RValue::Expr(s) => Some(s.clone()),
            _ => None,
        }
    }

    /// The internal-API JSON encoding: symbols keep a leading colon.
    pub fn to_json(&self) -> Json {
        match self {
            RValue::Str(s) => Json::String(s.clone()),
            RValue::Sym(s) => Json::String(format!(":{s}")),
            RValue::Int(i) => Json::Number((*i).into()),
            RValue::Bool(b) => Json::Bool(*b),
            RValue::Nil => Json::Null,
            RValue::Array(items) => Json::Array(items.iter().map(RValue::to_json).collect()),
            RValue::Hash(pairs) => {
                let mut map = Map::new();
                for (k, v) in pairs {
                    map.insert(format!(":{k}"), v.to_json());
                }
                Json::Object(map)
            }
            RValue::Expr(s) => Json::String(s.clone()),
        }
    }
}

/// Positional arguments plus trailing keyword arguments.
#[derive(Debug, Clone, Default)]
pub struct Args {
    pub positional: Vec<RValue>,
    pub kwargs: Vec<(String, RValue)>,
}

impl Args {
    pub fn first(&self) -> Option<&RValue> {
        self.positional.first()
    }

    pub fn first_str(&self) -> Option<String> {
        self.positional.first().and_then(RValue::to_text)
    }

    pub fn kwarg(&self, key: &str) -> Option<&RValue> {
        self.kwargs.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
}

/// Split `text` on top-level commas and parse each part.
pub fn parse_args(text: &str, vars: &BTreeMap<String, String>) -> Args {
    let mut out = Args::default();
    for part in split_top_level(text, ',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((key, value)) = split_kwarg(part) {
            out.kwargs.push((key, parse_value(value.trim(), vars)));
            continue;
        }
        if let Some((key, value)) = split_rocket(part) {
            let key = parse_value(key.trim(), vars)
                .to_text()
                .unwrap_or_else(|| key.trim().to_string());
            out.kwargs.push((key, parse_value(value.trim(), vars)));
            continue;
        }
        out.positional.push(parse_value(part, vars));
    }
    out
}

/// `key: value` at the top level (not `::`, not a `:symbol`, not inside a string).
fn split_kwarg(part: &str) -> Option<(String, &str)> {
    if part.starts_with(':') {
        return None;
    }
    let chars: Vec<char> = part.chars().collect();
    let mut depth = 0i32;
    let mut i = 0usize;
    let mut byte = 0usize;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '"' | '\'' => {
                let skipped = skip_literal(&chars[i..]);
                byte += chars[i..i + skipped]
                    .iter()
                    .map(|c| c.len_utf8())
                    .sum::<usize>();
                i += skipped;
                continue;
            }
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ':' if depth == 0
                && chars.get(i + 1) != Some(&':')
                && (i == 0 || chars[i - 1] != ':') =>
            {
                let key = part[..byte].trim();
                if !key.is_empty()
                    && key
                        .chars()
                        .all(|c| c.is_alphanumeric() || c == '_' || c == '"')
                {
                    return Some((key.trim_matches('"').to_string(), &part[byte + 1..]));
                }
            }
            _ => {}
        }
        byte += c.len_utf8();
        i += 1;
    }
    None
}

fn skip_literal(chars: &[char]) -> usize {
    let quote = chars[0];
    let mut i = 1usize;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 2,
            c if c == quote => return i + 1,
            _ => i += 1,
        }
    }
    chars.len()
}

/// `key => value` at the top level.
fn split_rocket(part: &str) -> Option<(&str, &str)> {
    let masked = super::scanner::mask_strings(part);
    let mut depth = 0i32;
    let bytes = masked.as_bytes();
    for i in 0..bytes.len() {
        match bytes[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b'=' if depth == 0 && bytes.get(i + 1) == Some(&b'>') => {
                return Some((&part[..i], &part[i + 2..]));
            }
            _ => {}
        }
    }
    None
}

/// Split on `sep` at bracket depth zero, ignoring separators inside literals.
pub fn split_top_level(text: &str, sep: char) -> Vec<String> {
    let masked = super::scanner::mask_strings(text);
    let mut out: Vec<String> = vec![];
    let mut depth = 0i32;
    let mut start = 0usize;
    let text_bytes: Vec<char> = text.chars().collect();
    let mask_chars: Vec<char> = masked.chars().collect();
    // `mask_strings` preserves length only for ASCII; fall back when it does not.
    if mask_chars.len() != text_bytes.len() {
        return vec![text.to_string()];
    }
    for (i, c) in mask_chars.iter().enumerate() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            c if *c == sep && depth == 0 => {
                out.push(text_bytes[start..i].iter().collect());
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(text_bytes[start..].iter().collect());
    out
}

/// Parse one Ruby literal.
pub fn parse_value(text: &str, vars: &BTreeMap<String, String>) -> RValue {
    let t = text.trim();
    if t.is_empty() {
        return RValue::Nil;
    }
    match t {
        "true" => return RValue::Bool(true),
        "false" => return RValue::Bool(false),
        "nil" => return RValue::Nil,
        _ => {}
    }
    if let Some(rest) = t.strip_prefix(':')
        && !rest.starts_with(':')
        && !rest.starts_with('"')
        && !rest.is_empty()
        && rest
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '?' || c == '!')
    {
        return RValue::Sym(rest.to_string());
    }
    if let Some(rest) = t.strip_prefix(":\"").and_then(|r| r.strip_suffix('"')) {
        return RValue::Sym(rest.to_string());
    }
    if (t.starts_with('"') && t.ends_with('"') && t.len() >= 2)
        || (t.starts_with('\'') && t.ends_with('\'') && t.len() >= 2)
    {
        let inner = &t[1..t.len() - 1];
        // Only a single literal, not `"a" + b`.
        if skip_literal(&t.chars().collect::<Vec<_>>()) == t.chars().count() {
            return RValue::Str(unescape(inner, t.starts_with('"'), vars));
        }
    }
    if let Ok(i) = t.replace('_', "").parse::<i64>() {
        return RValue::Int(i);
    }
    if let Some(inner) = t.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        let items = split_top_level(inner, ',')
            .into_iter()
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .map(|p| parse_value(&p, vars))
            .collect();
        return RValue::Array(items);
    }
    if let Some(inner) = t.strip_prefix('{').and_then(|r| r.strip_suffix('}')) {
        let args = parse_args(inner, vars);
        return RValue::Hash(args.kwargs);
    }
    if let Some(inner) = t.strip_prefix("%w[").and_then(|r| r.strip_suffix(']')) {
        return RValue::Array(
            inner
                .split_whitespace()
                .map(|w| RValue::Str(w.to_string()))
                .collect(),
        );
    }
    if let Some(inner) = t.strip_prefix("%i[").and_then(|r| r.strip_suffix(']')) {
        return RValue::Array(
            inner
                .split_whitespace()
                .map(|w| RValue::Sym(w.to_string()))
                .collect(),
        );
    }
    if let Some(joined) = path_join(t, vars) {
        return RValue::Str(joined);
    }
    if let Some(v) = vars.get(t) {
        return RValue::Str(v.clone());
    }
    RValue::Expr(t.to_string())
}

/// `opt_bin/"foo"`, `var/"log/x.log"`, `etc`/`prefix`.
fn path_join(text: &str, vars: &BTreeMap<String, String>) -> Option<String> {
    let parts = split_top_level(text, '/');
    if parts.len() < 2 {
        return None;
    }
    let mut out: Vec<String> = vec![];
    for (i, part) in parts.iter().enumerate() {
        let p = part.trim();
        if p.is_empty() {
            return None;
        }
        if (p.starts_with('"') && p.ends_with('"')) || (p.starts_with('\'') && p.ends_with('\'')) {
            out.push(unescape(&p[1..p.len() - 1], p.starts_with('"'), vars));
        } else if let Some(v) = vars.get(p) {
            out.push(v.clone());
        } else if i > 0
            && p.chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '.')
        {
            // A bare word after the first segment is a directory name.
            out.push(p.to_string());
        } else {
            return None;
        }
    }
    Some(out.join("/"))
}

/// Resolve escapes and `#{...}` interpolation inside a double-quoted string.
pub fn unescape(inner: &str, interpolate: bool, vars: &BTreeMap<String, String>) -> String {
    let chars: Vec<char> = inner.chars().collect();
    let mut out = String::new();
    let mut i = 0usize;
    while i < chars.len() {
        match chars[i] {
            '\\' if i + 1 < chars.len() => {
                let c = chars[i + 1];
                out.push(match c {
                    'n' if interpolate => '\n',
                    't' if interpolate => '\t',
                    other => other,
                });
                i += 2;
            }
            '#' if interpolate && chars.get(i + 1) == Some(&'{') => {
                let mut depth = 0i32;
                let mut j = i + 1;
                while j < chars.len() {
                    match chars[j] {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    j += 1;
                }
                let expr: String = chars[i + 2..j.min(chars.len())].iter().collect();
                match resolve_expr(expr.trim(), vars) {
                    Some(v) => out.push_str(&v),
                    None => out.push_str(&format!("#{{{expr}}}")),
                }
                i = j + 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Resolve an interpolated expression, including `version.major` style calls.
pub fn resolve_expr(expr: &str, vars: &BTreeMap<String, String>) -> Option<String> {
    if let Some(v) = vars.get(expr) {
        return Some(v.clone());
    }
    if let Some((base, method)) = expr.rsplit_once('.') {
        let value = resolve_expr(base.trim(), vars)?;
        return match method.trim() {
            "to_s" | "to_str" | "chomp" | "strip" | "to_i" => Some(value),
            "major" => Some(version_parts(&value, 1)),
            "major_minor" => Some(version_parts(&value, 2)),
            "major_minor_patch" => Some(version_parts(&value, 3)),
            "dots_to_underscores" => Some(value.replace('.', "_")),
            "no_dots" => Some(value.replace('.', "")),
            _ => None,
        };
    }
    if let Some(inner) = expr.strip_prefix("ENV[").and_then(|r| r.strip_suffix(']')) {
        let key = inner.trim().trim_matches(['"', '\'']);
        if key == "HOME" {
            return vars.get("HOME").cloned();
        }
    }
    // `bin/"x"` and friends inside an interpolation.
    path_join(expr, vars)
}

/// First `n` dot-separated components of a version.
pub fn version_parts(version: &str, n: usize) -> String {
    version.split('.').take(n).collect::<Vec<_>>().join(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("version".to_string(), "1.4.2".to_string()),
            ("name".to_string(), "bun".to_string()),
            ("var".to_string(), "$HOMEBREW_PREFIX/var".to_string()),
            (
                "opt_bin".to_string(),
                "$HOMEBREW_PREFIX/opt/bun/bin".to_string(),
            ),
        ])
    }

    #[test]
    fn parses_positional_and_keyword_args() {
        let a = parse_args(r#""x", tag: "v1", revision: "abc""#, &vars());
        assert_eq!(a.positional[0], RValue::Str("x".into()));
        assert_eq!(a.kwarg("tag").unwrap(), &RValue::Str("v1".into()));
        assert_eq!(a.kwarg("revision").unwrap(), &RValue::Str("abc".into()));
    }

    #[test]
    fn parses_rocket_hash() {
        let a = parse_args(r#""x" => :build"#, &vars());
        assert_eq!(a.kwarg("x").unwrap(), &RValue::Sym("build".into()));
    }

    #[test]
    fn interpolates_version() {
        let a = parse_args(
            r#""https://x/bun-v#{version}/bun-#{version.major_minor}.zip""#,
            &vars(),
        );
        assert_eq!(a.first_str().unwrap(), "https://x/bun-v1.4.2/bun-1.4.zip");
    }

    #[test]
    fn joins_paths() {
        let a = parse_args(r#"[opt_bin/"redis-server", var/"redis.conf"]"#, &vars());
        assert_eq!(
            a.positional[0],
            RValue::Array(vec![
                RValue::Str("$HOMEBREW_PREFIX/opt/bun/bin/redis-server".into()),
                RValue::Str("$HOMEBREW_PREFIX/var/redis.conf".into()),
            ])
        );
    }

    #[test]
    fn parses_nested_hash_and_array() {
        let a = parse_args(r#"trash: ["~/a", "~/b"], quit: "com.x""#, &vars());
        assert_eq!(
            a.kwarg("trash").unwrap(),
            &RValue::Array(vec![RValue::Str("~/a".into()), RValue::Str("~/b".into())])
        );
        assert_eq!(a.kwarg("quit").unwrap(), &RValue::Str("com.x".into()));
    }

    #[test]
    fn json_shape_uses_ruby_symbols() {
        let v = RValue::Hash(vec![(
            "trash".into(),
            RValue::Array(vec![RValue::Str("~/a".into())]),
        )]);
        assert_eq!(v.to_json(), serde_json::json!({":trash": ["~/a"]}));
        assert_eq!(
            RValue::Sym("build".into()).to_json(),
            serde_json::json!(":build")
        );
    }
}
