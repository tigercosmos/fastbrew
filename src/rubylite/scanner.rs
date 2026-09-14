//! Tolerant lexical pass over a Ruby formula or cask file.
//!
//! Strips comments, folds heredocs into the line that opens them, joins lines
//! that are inside an unclosed bracket, and counts block openers and `end`s so
//! the parsers can skip blocks they do not understand. It is deliberately not
//! a Ruby parser: everything it cannot recognise is left as text.

/// One logical line: code with comments removed and continuations joined.
#[derive(Debug, Clone, Default)]
pub struct Line {
    pub text: String,
    /// Heredoc bodies opened on this line, in order (`<<~EOS` is dedented).
    pub heredocs: Vec<String>,
}

impl Line {
    /// First whitespace-separated word (the DSL method name).
    pub fn head(&self) -> &str {
        let t = self.text.trim_start();
        let end = t
            .find(|c: char| c.is_whitespace() || c == '(' || c == ',')
            .unwrap_or(t.len());
        &t[..end]
    }

    /// Everything after the head word, with a wrapping `(...)` removed.
    pub fn args(&self) -> &str {
        let t = self.text.trim();
        let rest = t[self.head().len()..].trim();
        let rest = rest.strip_prefix('(').unwrap_or(rest);
        let rest = rest.strip_suffix(')').unwrap_or(rest);
        rest.trim()
    }

    /// Opened blocks minus closed blocks on this line.
    pub fn block_delta(&self) -> i32 {
        let masked = mask_strings(&self.text);
        block_opens(&masked) - count_ends(&masked)
    }
}

/// Split a source file into logical lines.
pub fn logical_lines(src: &str) -> Vec<Line> {
    let raw: Vec<&str> = src.lines().collect();
    let mut out: Vec<Line> = vec![];
    let mut i = 0usize;

    while i < raw.len() {
        let mut text = String::new();
        let mut heredocs: Vec<String> = vec![];
        let mut depth = 0i32;

        loop {
            let scan = scan_code(raw[i], depth);
            depth = scan.depth;
            if !text.is_empty() && !scan.code.trim().is_empty() {
                text.push(' ');
            }
            text.push_str(scan.code.trim());
            i += 1;

            // Consume every heredoc opened on this physical line.
            for tag in scan.heredocs {
                let mut body: Vec<&str> = vec![];
                while i < raw.len() {
                    let line = raw[i];
                    let candidate = if tag.indented {
                        line.trim()
                    } else {
                        line.trim_end()
                    };
                    i += 1;
                    if candidate == tag.name {
                        break;
                    }
                    body.push(line);
                }
                heredocs.push(if tag.squiggly {
                    dedent(&body)
                } else {
                    let mut s = body.join("\n");
                    if !s.is_empty() {
                        s.push('\n');
                    }
                    s
                });
            }

            let trimmed = text.trim_end();
            let continued = depth > 0
                || trimmed.ends_with(',')
                || trimmed.ends_with('\\')
                || trimmed.ends_with("&&")
                || trimmed.ends_with("||")
                || trimmed.ends_with("=>");
            if !continued || i >= raw.len() {
                break;
            }
            if trimmed.ends_with('\\') {
                text.truncate(text.trim_end().len() - 1);
            }
        }

        let text = text.trim().to_string();
        if text.is_empty() && heredocs.is_empty() {
            continue;
        }
        out.push(Line { text, heredocs });
    }
    out
}

#[derive(Debug, Clone)]
struct HeredocTag {
    name: String,
    squiggly: bool,
    indented: bool,
}

struct CodeScan {
    code: String,
    depth: i32,
    heredocs: Vec<HeredocTag>,
}

/// Copy the code part of one physical line, tracking bracket depth and heredocs.
fn scan_code(line: &str, start_depth: i32) -> CodeScan {
    let chars: Vec<char> = line.chars().collect();
    let mut code = String::new();
    let mut depth = start_depth;
    let mut heredocs: Vec<HeredocTag> = vec![];
    let mut i = 0usize;

    while i < chars.len() {
        let c = chars[i];
        match c {
            '#' => break, // comment: the rest of the line is gone
            '"' | '\'' => {
                let end = scan_quoted(&chars, i);
                code.extend(&chars[i..end]);
                i = end;
                continue;
            }
            '(' | '[' | '{' => {
                depth += 1;
                code.push(c);
            }
            ')' | ']' | '}' => {
                depth -= 1;
                code.push(c);
            }
            '%' if percent_literal_here(&chars, i) => {
                let end = scan_percent(&chars, i);
                code.extend(&chars[i..end]);
                i = end;
                continue;
            }
            '/' if regex_here(&chars, i) => {
                let end = scan_regex(&chars, i);
                code.extend(&chars[i..end]);
                i = end;
                continue;
            }
            '<' if chars.get(i + 1) == Some(&'<') => {
                if let Some((tag, end)) = scan_heredoc_tag(&chars, i) {
                    heredocs.push(tag);
                    code.extend(&chars[i..end]);
                    i = end;
                    continue;
                }
                code.push(c);
            }
            _ => code.push(c),
        }
        i += 1;
    }

    CodeScan {
        code,
        depth,
        heredocs,
    }
}

/// Index just past a `"..."` or `'...'` literal starting at `start`.
fn scan_quoted(chars: &[char], start: usize) -> usize {
    let quote = chars[start];
    let mut i = start + 1;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 2,
            '#' if quote == '"' && chars.get(i + 1) == Some(&'{') => {
                i = skip_interpolation(chars, i + 1);
            }
            c if c == quote => return i + 1,
            _ => i += 1,
        }
    }
    chars.len()
}

/// Index just past the closing `}` of a `#{ ... }` interpolation.
fn skip_interpolation(chars: &[char], brace: usize) -> usize {
    let mut depth = 0i32;
    let mut i = brace;
    while i < chars.len() {
        match chars[i] {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            '"' | '\'' => {
                i = scan_quoted(chars, i);
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    chars.len()
}

fn closing_delim(open: char) -> char {
    match open {
        '(' => ')',
        '[' => ']',
        '{' => '}',
        '<' => '>',
        other => other,
    }
}

/// True when `%` at `i` starts a `%w[]`/`%r{}`/`%q()` literal rather than modulo.
fn percent_literal_here(chars: &[char], i: usize) -> bool {
    let prev = chars[..i].iter().rev().find(|c| !c.is_whitespace());
    let at_literal_position = matches!(
        prev,
        None | Some('(') | Some('[') | Some(',') | Some('=') | Some('{')
    );
    if !at_literal_position {
        return false;
    }
    let mut j = i + 1;
    if chars.get(j).is_some_and(|c| c.is_ascii_alphabetic()) {
        j += 1;
    }
    chars.get(j).is_some_and(|c| "([{<|!/~^".contains(*c))
}

fn scan_percent(chars: &[char], start: usize) -> usize {
    let mut j = start + 1;
    if chars.get(j).is_some_and(|c| c.is_ascii_alphabetic()) {
        j += 1;
    }
    let Some(&open) = chars.get(j) else {
        return chars.len();
    };
    let close = closing_delim(open);
    let nests = open != close;
    let mut depth = 1i32;
    let mut i = j + 1;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' {
            i += 2;
            continue;
        }
        if nests && c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                return i + 1;
            }
        }
        i += 1;
    }
    chars.len()
}

/// True when `/` at `i` starts a regex literal rather than a division.
fn regex_here(chars: &[char], i: usize) -> bool {
    let prev = chars[..i].iter().rev().find(|c| !c.is_whitespace());
    matches!(
        prev,
        None | Some('(') | Some(',') | Some('=') | Some('~') | Some('!') | Some('[')
    )
}

fn scan_regex(chars: &[char], start: usize) -> usize {
    let mut i = start + 1;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 2,
            '/' => return i + 1,
            _ => i += 1,
        }
    }
    chars.len()
}

/// `<<~EOS`, `<<-EOS`, `<<EOS`, `<<~'EOS'`. Returns the tag and the end index.
fn scan_heredoc_tag(chars: &[char], start: usize) -> Option<(HeredocTag, usize)> {
    let mut i = start + 2;
    let mut squiggly = false;
    let mut indented = false;
    match chars.get(i) {
        Some('~') => {
            squiggly = true;
            indented = true;
            i += 1;
        }
        Some('-') => {
            indented = true;
            i += 1;
        }
        _ => {}
    }
    let quote = match chars.get(i) {
        Some('\'') | Some('"') => {
            let q = chars[i];
            i += 1;
            Some(q)
        }
        _ => None,
    };
    let name_start = i;
    while chars
        .get(i)
        .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
    {
        i += 1;
    }
    if i == name_start {
        return None;
    }
    let name: String = chars[name_start..i].iter().collect();
    // A bare `<<X` must look like a heredoc tag, not the shift operator.
    if quote.is_none() && !squiggly && !indented {
        let first = name.chars().next()?;
        if !(first.is_ascii_uppercase() || first == '_') {
            return None;
        }
    }
    if let Some(q) = quote {
        if chars.get(i) != Some(&q) {
            return None;
        }
        i += 1;
    }
    Some((
        HeredocTag {
            name,
            squiggly,
            indented,
        },
        i,
    ))
}

/// `<<~` semantics: strip the smallest indentation of the non-blank lines.
fn dedent(body: &[&str]) -> String {
    let indent = body
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    let mut out = String::new();
    for line in body {
        if line.trim().is_empty() {
            out.push('\n');
        } else {
            out.push_str(&line[indent.min(line.len())..]);
            out.push('\n');
        }
    }
    out
}

/// Replace every string, regex and `%`-literal body with `_` so keyword
/// detection cannot trip over their contents.
pub fn mask_strings(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        let end = match c {
            '"' | '\'' => scan_quoted(&chars, i),
            '%' if percent_literal_here(&chars, i) => scan_percent(&chars, i),
            '/' if regex_here(&chars, i) => scan_regex(&chars, i),
            _ => {
                out.push(c);
                i += 1;
                continue;
            }
        };
        out.push('"');
        out.extend(std::iter::repeat_n('_', end.saturating_sub(i + 2)));
        out.push('"');
        i = end;
    }
    out
}

const BLOCK_KEYWORDS: [&str; 8] = [
    "def", "class", "module", "begin", "case", "if", "unless", "while",
];

fn first_word(s: &str) -> &str {
    let t = s.trim_start();
    let end = t
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(t.len());
    &t[..end]
}

/// True when the line ends with `do` or `do |args|`.
pub fn ends_with_do(masked: &str) -> bool {
    let t = masked.trim_end();
    let t = match t.rfind('|') {
        Some(i) if t.ends_with('|') => {
            let head = &t[..i];
            match head.rfind('|') {
                Some(j) => t[..j].trim_end(),
                None => t,
            }
        }
        _ => t,
    };
    t == "do" || t.ends_with(" do") || t.ends_with(")do")
}

/// Number of blocks the line opens.
pub fn block_opens(masked: &str) -> i32 {
    let mut n = 0;
    let word = first_word(masked);
    if BLOCK_KEYWORDS.contains(&word) {
        // `next if x` style modifiers never start with the keyword.
        n += 1;
    }
    if ends_with_do(masked) {
        n += 1;
    }
    n
}

/// Number of `end` keywords on the line.
pub fn count_ends(masked: &str) -> i32 {
    let mut n = 0;
    let bytes = masked.as_bytes();
    let mut i = 0usize;
    while let Some(pos) = masked[i..].find("end") {
        let at = i + pos;
        let before_ok = at == 0 || !is_word_byte(bytes[at - 1]);
        let after = bytes.get(at + 3).copied();
        let after_ok = after.is_none_or(|b| !is_word_byte(b));
        if before_ok && after_ok {
            n += 1;
        }
        i = at + 3;
    }
    n
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'?' || b == b'!' || b == b':'
}

/// Index of the line that closes the block opened at `start`, or `lines.len()`.
pub fn find_block_end(lines: &[Line], start: usize) -> usize {
    let mut depth = lines[start].block_delta();
    if depth <= 0 {
        return start;
    }
    let mut i = start + 1;
    while i < lines.len() {
        depth += lines[i].block_delta();
        if depth <= 0 {
            return i;
        }
        i += 1;
    }
    lines.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_comments_and_keeps_strings() {
        let lines = logical_lines("desc \"a # b\" # trailing\nurl \"x\"\n");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "desc \"a # b\"");
        assert_eq!(lines[1].text, "url \"x\"");
    }

    #[test]
    fn joins_bracketed_continuations() {
        let src = "zap trash: [\n  \"a\",\n  \"b\",\n]\n";
        let lines = logical_lines(src);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "zap trash: [ \"a\", \"b\", ]");
    }

    #[test]
    fn folds_squiggly_heredocs() {
        let src = "def caveats\n  <<~EOS\n    hello\n      world\n  EOS\nend\n";
        let lines = logical_lines(src);
        assert_eq!(lines[0].text, "def caveats");
        assert_eq!(lines[1].heredocs[0], "hello\n  world\n");
        assert_eq!(lines[2].text, "end");
    }

    #[test]
    fn percent_and_regex_literals_do_not_confuse_comments() {
        let src = "regex(%r{href=.*?/tag/bun-v?(\\d+)[\"' >]}i)\nsha256 \"abc\"\n";
        let lines = logical_lines(src);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].text, "sha256 \"abc\"");
    }

    #[test]
    fn counts_blocks() {
        let l = |s: &str| logical_lines(s).remove(0);
        assert_eq!(l("service do").block_delta(), 1);
        assert_eq!(l("resource \"x\" do |r|").block_delta(), 1);
        assert_eq!(l("if OS.mac?").block_delta(), 1);
        assert_eq!(l("elsif OS.linux?").block_delta(), 0);
        assert_eq!(l("else").block_delta(), 0);
        assert_eq!(l("end").block_delta(), -1);
        assert_eq!(l("def install").block_delta(), 1);
        assert_eq!(l("next if x.end?").block_delta(), 0);
        assert_eq!(l("sha256 \"end do\"").block_delta(), 0);
        assert_eq!(l("bin.install \"a\", \"b\"").block_delta(), 0);
    }

    #[test]
    fn finds_matching_end() {
        let src = "class Foo < Formula\n  def install\n    if x\n    end\n  end\nend\n";
        let lines = logical_lines(src);
        assert_eq!(find_block_end(&lines, 0), 5);
        assert_eq!(find_block_end(&lines, 1), 4);
        assert_eq!(find_block_end(&lines, 2), 3);
    }
}
