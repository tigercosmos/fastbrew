//! Homebrew version semantics (`docs/COMPAT.md` 8). Port of
//! `Library/Homebrew/version.rb` and `pkg_version.rb`.
//!
//! Required API:
//! - `Version::new(&str)`, `Version::head()`, `is_head()`, `Display`.
//! - `impl Ord for Version` with Homebrew's token rules.
//! - `Version::detect_from_url(url)` (used by rubylite when a formula has no
//!   explicit version; port `Version.parse`/`detect`).
//! - `PkgVersion { version, revision }`, `PkgVersion::parse("1.2_3")`,
//!   `Display` (`1.2_3`), `Ord`.
//!
//! Tests port the comparison cases from `Library/Homebrew/test/version_spec.rb`.

use std::cmp::Ordering;
use std::fmt;
use std::sync::OnceLock;

use regex::Regex;

// ---------------------------------------------------------------------------
// Tokens (`Version::Token` and subclasses)
// ---------------------------------------------------------------------------

/// One token of a version string. Mirrors the `Version::Token` hierarchy:
/// `NullToken`, `StringToken`, `NumericToken` and the `CompositeToken`
/// subclasses `AlphaToken`, `BetaToken`, `PreToken`, `RCToken`, `PatchToken`
/// and `PostToken`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Token {
    Null,
    /// `[a-z]+`
    Str(String),
    /// `[0-9]+`
    Num(u128),
    /// `alpha[0-9]*|a[0-9]+`
    Alpha(String),
    /// `beta[0-9]*|b[0-9]+`
    Beta(String),
    /// `pre[0-9]*`
    Pre(String),
    /// `rc[0-9]*`
    Rc(String),
    /// `p[0-9]*`
    Patch(String),
    /// `.post[0-9]+`
    Post(String),
}

/// Rank inside the pre-release ordering used by the composite tokens:
/// alpha < beta < pre < rc < patch/post.
fn composite_rank(t: &Token) -> Option<u8> {
    match t {
        Token::Alpha(_) => Some(0),
        Token::Beta(_) => Some(1),
        Token::Pre(_) => Some(2),
        Token::Rc(_) => Some(3),
        Token::Patch(_) => Some(4),
        Token::Post(_) => Some(4),
        _ => None,
    }
}

impl Token {
    pub fn is_numeric(&self) -> bool {
        matches!(self, Token::Num(_))
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Token::Null)
    }

    /// The raw text of a string-ish token (`StringToken#value`).
    fn text(&self) -> Option<&str> {
        match self {
            Token::Str(s)
            | Token::Alpha(s)
            | Token::Beta(s)
            | Token::Pre(s)
            | Token::Rc(s)
            | Token::Patch(s)
            | Token::Post(s) => Some(s),
            _ => None,
        }
    }

    /// `CompositeToken#rev`: the first run of digits in the token, else 0.
    fn rev(&self) -> u128 {
        let Some(s) = self.text() else { return 0 };
        let bytes = s.as_bytes();
        let Some(start) = bytes.iter().position(u8::is_ascii_digit) else {
            return 0;
        };
        let end = bytes[start..]
            .iter()
            .position(|b| !b.is_ascii_digit())
            .map(|i| start + i)
            .unwrap_or(bytes.len());
        parse_digits(&s[start..end])
    }

    /// `Version::Token#to_s`.
    pub fn to_token_string(&self) -> String {
        match self {
            Token::Null => String::new(),
            Token::Num(n) => n.to_string(),
            other => other.text().unwrap_or_default().to_string(),
        }
    }

    /// Port of `StringToken#<=>` used as the `super` fallback of the composite
    /// tokens (every composite token is a `StringToken`).
    fn cmp_as_string(&self, other: &Token) -> Ordering {
        let me = self.text().unwrap_or_default();
        match other {
            Token::Num(_) => Ordering::Less,
            // `StringToken#<=>` inverts `NullToken#<=>` here, which is what
            // keeps `1.2.3alpha < 1.2.3` while `1.2.3-p34 > 1.2.3`.
            Token::Null => Token::Null.cmp(self).reverse(),
            o => me.cmp(o.text().unwrap_or_default()),
        }
    }
}

impl PartialOrd for Token {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Token {
    fn cmp(&self, other: &Self) -> Ordering {
        match self {
            // NullToken#<=>
            Token::Null => match other {
                Token::Null => Ordering::Equal,
                Token::Num(0) => Ordering::Equal,
                Token::Num(_) => Ordering::Less,
                Token::Alpha(_) | Token::Beta(_) | Token::Pre(_) | Token::Rc(_) => {
                    Ordering::Greater
                }
                _ => Ordering::Less,
            },
            // NumericToken#<=>
            Token::Num(v) => match other {
                Token::Num(w) => v.cmp(w),
                Token::Null => {
                    if *v == 0 {
                        Ordering::Equal
                    } else {
                        Ordering::Greater
                    }
                }
                _ => Ordering::Greater,
            },
            // StringToken#<=>
            Token::Str(_) => self.cmp_as_string(other),
            // CompositeToken subclasses.
            _ => {
                let mine = composite_rank(self).expect("composite token");
                match composite_rank(other) {
                    Some(theirs)
                        if std::mem::discriminant(self) == std::mem::discriminant(other) =>
                    {
                        let _ = theirs;
                        self.rev().cmp(&other.rev())
                    }
                    Some(theirs) if mine != theirs => mine.cmp(&theirs),
                    // Patch vs Post (equal rank, different class): both fall
                    // through to the `StringToken` comparison in the Ruby.
                    _ => self.cmp_as_string(other),
                }
            }
        }
    }
}

fn parse_digits(s: &str) -> u128 {
    // Ruby integers are arbitrary precision; saturate instead of overflowing.
    s.parse::<u128>().unwrap_or(u128::MAX)
}

/// Port of `Version::SCAN_PATTERN`: scan the string for tokens, trying the
/// alternatives in the union's order at each position and skipping a character
/// when none matches.
fn scan_tokens(s: &str) -> Vec<Token> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        if let Some((tok, len)) = token_at(b, i) {
            out.push(tok);
            i += len;
        } else {
            // Advance one UTF-8 character.
            i += 1;
            while i < b.len() && (b[i] & 0xC0) == 0x80 {
                i += 1;
            }
        }
    }
    out
}

fn eq_ignore_case(b: &[u8], at: usize, word: &str) -> bool {
    let w = word.as_bytes();
    at + w.len() <= b.len() && b[at..at + w.len()].eq_ignore_ascii_case(w)
}

fn digits_len(b: &[u8], at: usize) -> usize {
    let mut n = 0;
    while at + n < b.len() && b[at + n].is_ascii_digit() {
        n += 1;
    }
    n
}

fn token_at(b: &[u8], i: usize) -> Option<(Token, usize)> {
    let text = |len: usize| String::from_utf8_lossy(&b[i..i + len]).into_owned();

    // AlphaToken: /alpha[0-9]*|a[0-9]+/i
    if eq_ignore_case(b, i, "alpha") {
        let len = 5 + digits_len(b, i + 5);
        return Some((Token::Alpha(text(len)), len));
    }
    if b[i].eq_ignore_ascii_case(&b'a') {
        let d = digits_len(b, i + 1);
        if d > 0 {
            return Some((Token::Alpha(text(1 + d)), 1 + d));
        }
    }
    // BetaToken: /beta[0-9]*|b[0-9]+/i
    if eq_ignore_case(b, i, "beta") {
        let len = 4 + digits_len(b, i + 4);
        return Some((Token::Beta(text(len)), len));
    }
    if b[i].eq_ignore_ascii_case(&b'b') {
        let d = digits_len(b, i + 1);
        if d > 0 {
            return Some((Token::Beta(text(1 + d)), 1 + d));
        }
    }
    // PreToken: /pre[0-9]*/i
    if eq_ignore_case(b, i, "pre") {
        let len = 3 + digits_len(b, i + 3);
        return Some((Token::Pre(text(len)), len));
    }
    // RCToken: /rc[0-9]*/i
    if eq_ignore_case(b, i, "rc") {
        let len = 2 + digits_len(b, i + 2);
        return Some((Token::Rc(text(len)), len));
    }
    // PatchToken: /p[0-9]*/i
    if b[i].eq_ignore_ascii_case(&b'p') {
        let len = 1 + digits_len(b, i + 1);
        return Some((Token::Patch(text(len)), len));
    }
    // PostToken: /.post[0-9]+/i  (the leading `.` is Ruby's any-character)
    if i + 1 < b.len() && eq_ignore_case(b, i + 1, "post") {
        let d = digits_len(b, i + 5);
        if d > 0 {
            let len = 5 + d;
            return Some((Token::Post(text(len)), len));
        }
    }
    // NumericToken: /[0-9]+/
    let d = digits_len(b, i);
    if d > 0 {
        return Some((
            Token::Num(parse_digits(&String::from_utf8_lossy(&b[i..i + d]))),
            d,
        ));
    }
    // StringToken: /[a-z]+/i
    let mut n = 0;
    while i + n < b.len() && b[i + n].is_ascii_alphabetic() {
        n += 1;
    }
    if n > 0 {
        return Some((Token::Str(text(n)), n));
    }
    None
}

// ---------------------------------------------------------------------------
// Version
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Version {
    raw: String,
}

impl Version {
    pub fn new(s: &str) -> Self {
        Version { raw: s.to_string() }
    }

    pub fn head() -> Self {
        Version {
            raw: "HEAD".to_string(),
        }
    }

    pub fn is_head(&self) -> bool {
        self.raw == "HEAD" || self.raw.starts_with("HEAD-")
    }

    /// The commit of a `HEAD-<commit>` version.
    pub fn commit(&self) -> Option<&str> {
        self.raw.strip_prefix("HEAD-").filter(|c| !c.is_empty())
    }

    /// `Version::NULL`: the absence of a version, represented by an empty string.
    pub fn null() -> Self {
        Version { raw: String::new() }
    }

    pub fn is_null(&self) -> bool {
        self.raw.is_empty()
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// The tokens this version compares by (`Version#tokens`).
    pub fn tokens(&self) -> Vec<Token> {
        scan_tokens(&self.raw)
    }

    /// `Version#major`.
    pub fn major(&self) -> Option<Token> {
        self.tokens().into_iter().next()
    }

    /// `Version#minor`.
    pub fn minor(&self) -> Option<Token> {
        self.tokens().into_iter().nth(1)
    }

    /// `Version#patch`.
    pub fn patch(&self) -> Option<Token> {
        self.tokens().into_iter().nth(2)
    }

    /// `Version#major_minor`.
    pub fn major_minor(&self) -> Version {
        self.first_tokens(2)
    }

    /// `Version#major_minor_patch`.
    pub fn major_minor_patch(&self) -> Version {
        self.first_tokens(3)
    }

    fn first_tokens(&self, n: usize) -> Version {
        if self.is_null() {
            return self.clone();
        }
        let parts: Vec<String> = self
            .tokens()
            .into_iter()
            .take(n)
            .map(|t| t.to_token_string())
            .collect();
        if parts.is_empty() {
            Version::null()
        } else {
            Version::new(&parts.join("."))
        }
    }

    /// Port of `Version.parse(spec)`: run every URL/stem parser in order and
    /// return the first version found. `None` is Homebrew's `Version::NULL`.
    pub fn detect_from_url(url: &str) -> Option<Version> {
        Self::parse_spec(url, true)
    }

    /// Port of `Version.detect(url, tag:)`: a `tag:` spec wins over the URL.
    pub fn detect(url: &str, tag: Option<&str>) -> Option<Version> {
        Self::parse_spec(tag.unwrap_or(url), true)
    }

    /// Port of `Version.parse(spec, detected_from_url:)`.
    pub fn parse_spec(spec: &str, detected_from_url: bool) -> Option<Version> {
        let spec = if detected_from_url {
            decode_www_form_component(spec)
        } else {
            spec.to_string()
        };
        for parser in version_parsers() {
            if let Some(v) = parser.parse(&spec) {
                return Some(Version::new(&v));
            }
        }
        None
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        // `Version::NULL` sorts below everything (Ruby returns nil for
        // NULL <=> NULL; we report Equal so that `Ord` stays total).
        match (self.is_null(), other.is_null()) {
            (true, true) => return Ordering::Equal,
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            (false, false) => {}
        }
        if self.raw == other.raw {
            return Ordering::Equal;
        }
        match (self.is_head(), other.is_head()) {
            (true, false) => return Ordering::Greater,
            (false, true) => return Ordering::Less,
            (true, true) => return Ordering::Equal,
            (false, false) => {}
        }

        let ltokens = self.tokens();
        let rtokens = other.tokens();
        let max = ltokens.len().max(rtokens.len());
        let (mut l, mut r) = (0usize, 0usize);
        while l < max {
            let a = ltokens.get(l).unwrap_or(&Token::Null);
            let b = rtokens.get(r).unwrap_or(&Token::Null);
            let ord = a.cmp(b);
            if ord == Ordering::Equal {
                l += 1;
                r += 1;
                continue;
            }
            if a.is_numeric() && !b.is_numeric() {
                if a.cmp(&Token::Null) == Ordering::Greater {
                    return Ordering::Greater;
                }
                l += 1;
            } else if !a.is_numeric() && b.is_numeric() {
                if b.cmp(&Token::Null) == Ordering::Greater {
                    return Ordering::Less;
                }
                r += 1;
            } else {
                return ord;
            }
        }
        Ordering::Equal
    }
}

// ---------------------------------------------------------------------------
// PkgVersion
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PkgVersion {
    pub version: Version,
    pub revision: u32,
}

impl PkgVersion {
    pub fn new(version: Version, revision: u32) -> Self {
        PkgVersion { version, revision }
    }

    /// Parse `1.2.3` or `1.2.3_4` (`PkgVersion::REGEX`: the last `_<digits>`
    /// is the revision, and the version part must not be empty).
    pub fn parse(s: &str) -> Self {
        if let Some((v, r)) = s.rsplit_once('_')
            && !v.is_empty()
            && !r.is_empty()
            && r.bytes().all(|b| b.is_ascii_digit())
            && let Ok(rev) = r.parse::<u32>()
        {
            return PkgVersion {
                version: Version::new(v),
                revision: rev,
            };
        }
        PkgVersion {
            version: Version::new(s),
            revision: 0,
        }
    }

    pub fn is_head(&self) -> bool {
        self.version.is_head()
    }
}

impl fmt::Display for PkgVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.revision > 0 {
            write!(f, "{}_{}", self.version, self.revision)
        } else {
            write!(f, "{}", self.version)
        }
    }
}

impl PartialOrd for PkgVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PkgVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        self.version
            .cmp(&other.version)
            .then(self.revision.cmp(&other.revision))
    }
}

// ---------------------------------------------------------------------------
// URL version detection (`Version::VERSION_PARSERS`)
// ---------------------------------------------------------------------------

/// `URI.decode_www_form_component`: `+` becomes a space and `%XX` is decoded.
/// Invalid escapes are left alone (Ruby raises; we keep the input usable).
fn decode_www_form_component(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    None => {
                        out.push(b[i]);
                        i += 1;
                    }
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpecSource {
    /// `Version::UrlParser`: match against the whole spec.
    Url,
    /// `Version::StemParser`: match against the file stem (see `process_stem`).
    Stem,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transform {
    None,
    /// `{ |s| s.tr("_", ".") }`
    UnderscoreToDot,
}

struct VersionParser {
    source: SpecSource,
    regex: Regex,
    transform: Transform,
}

impl VersionParser {
    fn parse(&self, spec: &str) -> Option<String> {
        let subject = match self.source {
            SpecSource::Url => spec.to_string(),
            SpecSource::Stem => process_stem(spec),
        };
        let caps = self.regex.captures(&subject)?;
        let v = caps.get(1)?.as_str();
        if v.trim().is_empty() {
            return None;
        }
        Some(match self.transform {
            Transform::None => v.to_string(),
            Transform::UnderscoreToDot => v.replace('_', "."),
        })
    }
}

fn basename(spec: &str) -> &str {
    match spec.rsplit_once('/') {
        Some((_, last)) => last,
        None => spec,
    }
}

fn dirname(spec: &str) -> &str {
    match spec.rsplit_once('/') {
        Some((head, _)) if !head.is_empty() => head,
        Some(_) => "/",
        None => ".",
    }
}

/// Port of Homebrew's `Pathname#extname` (`extend/pathname.rb`).
fn extname(basename: &str) -> String {
    static BOTTLE: OnceLock<Regex> = OnceLock::new();
    static ARCHIVE: OnceLock<Regex> = OnceLock::new();
    static VERSIONY: OnceLock<Regex> = OnceLock::new();
    let bottle =
        BOTTLE.get_or_init(|| Regex::new(r"\.[a-z0-9_]+\.bottle\.(?:\d+\.)?tar\.gz$").unwrap());
    if let Some(m) = bottle.find(basename) {
        return m.as_str().to_string();
    }
    let archive = ARCHIVE
        .get_or_init(|| Regex::new(r"(\.(?:tar|cpio|pax)\.(?:gz|bz2|lz|xz|zst|Z))$").unwrap());
    if let Some(c) = archive.captures(basename) {
        return c[1].to_string();
    }
    let versiony = VERSIONY.get_or_init(|| Regex::new(r"\b\d+\.\d+[^.]*$").unwrap());
    if versiony.is_match(basename) && !basename.ends_with(".7z") {
        return String::new();
    }
    // `File.extname`: the last dot-separated suffix, ignoring leading dots.
    let trimmed = basename.trim_start_matches('.');
    match trimmed.rfind('.') {
        Some(idx) if idx + 1 < trimmed.len() => trimmed[idx..].to_string(),
        _ => String::new(),
    }
}

fn stem(path: &str) -> String {
    let base = basename(path);
    let ext = extname(base);
    if !ext.is_empty() && base.ends_with(&ext) {
        base[..base.len() - ext.len()].to_string()
    } else {
        base.to_string()
    }
}

/// Port of `Version::StemParser.process_spec`.
fn process_stem(spec: &str) -> String {
    static SOURCEFORGE: OnceLock<Regex> = OnceLock::new();
    static NO_EXT: OnceLock<Regex> = OnceLock::new();
    let sourceforge = SOURCEFORGE
        .get_or_init(|| Regex::new(r"(?:sourceforge\.net|sf\.net)/.*/download$").unwrap());
    if sourceforge.is_match(spec) {
        return stem(dirname(spec));
    }
    let no_ext = NO_EXT.get_or_init(|| Regex::new(r"\.[^a-zA-Z]+$").unwrap());
    if no_ext.is_match(spec) {
        return basename(spec).to_string();
    }
    stem(spec)
}

const NUMERIC_WITH_OPTIONAL_DOTS: &str = r"(?:\d+(?:\.\d+)*)";
const NUMERIC_WITH_DOTS: &str = r"(?:\d+(?:\.\d+)+)";
const MINOR_OR_PATCH: &str = r"(?:\d+(?:\.\d+){1,2})";
const CONTENT_SUFFIX: &str = r"(?:[._-](?i:bin|dist|stable|src|sources?|final|full))";
const PRERELEASE_SUFFIX: &str = r"(?:[._-]?(?i:alpha|beta|pre|rc)\.?\d{0,2})";

fn version_parsers() -> &'static [VersionParser] {
    static PARSERS: OnceLock<Vec<VersionParser>> = OnceLock::new();
    PARSERS.get_or_init(build_parsers)
}

fn build_parsers() -> Vec<VersionParser> {
    let n = NUMERIC_WITH_OPTIONAL_DOTS;
    let nd = NUMERIC_WITH_DOTS;
    let mp = MINOR_OR_PATCH;
    let cs = CONTENT_SUFFIX;
    let ps = PRERELEASE_SUFFIX;

    fn stem(re: String) -> (SpecSource, String, Transform) {
        (SpecSource::Stem, re, Transform::None)
    }
    fn url(re: String) -> (SpecSource, String, Transform) {
        (SpecSource::Url, re, Transform::None)
    }

    let specs: Vec<(SpecSource, String, Transform)> = vec![
        // date-based versioning, e.g. `2023-09-28.tar.gz`
        stem(r"(?:^|[._\-]?)v?(\d{4}-\d{2}-\d{2})".to_string()),
        // GitHub tarballs
        url(r"github\.com/.+/(?:zip|tar)ball/(?:v|\w+-)?((?:\d+[._\-])+\d*)$".to_string()),
        // GitHub releases
        url(format!(
            r"github\.com/.+/releases/download/(?:[rvV]_?)?({nd})/"
        )),
        // erlang style, e.g. `OTP_R15B01`
        url(r"[_\-]([Rr]\d+[AaBb]\d*(?:-\d+)?)".to_string()),
        // e.g. `boost_1_39_0`
        (
            SpecSource::Stem,
            r"((?:\d+_)+\d+)$".to_string(),
            Transform::UnderscoreToDot,
        ),
        // e.g. `foobar-4.5.1-1`, `ruby-1.9.1-p243`
        stem(format!(r"[_\-]({nd}-(?:p|P|rc|RC)?\d+){cs}?$")),
        // hyphenated versions without a software-name prefix
        stem(format!(r"^v?({nd}(?:-{n})+)")),
        // URL with no extension
        url(format!(r"[\-v]({n})$")),
        // e.g. `lame-398-1`
        stem(r"-(\d+-\d+)".to_string()),
        // e.g. `foobar-4.5.1`
        stem(format!(r"-({n})$")),
        // e.g. `foobar-4.5.1.post1`
        stem(format!(r"-({n}(?:.post\d+)?)$")),
        // e.g. `foobar-4.5.1b`
        stem(format!(r"-({n}(?:[abc]|rc|RC)\d*)$")),
        // e.g. `foobar-4.5.0-alpha5`
        stem(format!(r"-({n}-(?:alpha|beta|rc)\d*)$")),
        // e.g. `libidn-1.29-win64.zip`
        stem(format!(r"-({mp})-w(?:in)?(?:32|64)$")),
        // opam packages
        stem(format!(r"\.({mp})\+opam$")),
        // e.g. `mtools-4.0.18-1.i686.rpm`
        stem(format!(
            r"[_\-]({mp}(?:-\d+)?)[._\-](?:i[36]86|x86|x64(?:[_\-](?:32|64))?)$"
        )),
        // e.g. `cli-1.3.0-beta.1.tgz`
        stem(format!(r"[\-.vV]?({nd}{ps})")),
        // e.g. `foobar4.5.1`
        stem(format!(r"({n})$")),
        // e.g. `foobar-4.5.0-bin`
        stem(format!(r"[\-vV]({nd}[abc]?){cs}$")),
        // dash version style, e.g. `antlr-3.4-complete.jar`
        stem(format!(r"-({nd})-")),
        // Debian style, e.g. `dash_0.5.5.1.orig.tar.gz`
        stem(format!(r"_({n}[abc]?)\.orig$")),
        // e.g. `openssl-0.9.8s.tar.gz`
        stem(r"-v?(\d[^-]+)".to_string()),
        // e.g. `astyle_1.23_macosx.tar.gz`
        stem(r"_v?(\d[^_]+)".to_string()),
        // e.g. `https://mirrors.jenkins-ci.org/war/1.486/jenkins.war`
        url(r"/(?:[rvV]_?)?(\d+\.\d+(?:\.\d+){0,2})".to_string()),
        // e.g. `jpegsrc.v8d.tar.gz`
        stem(r"\.v(\d+[a-z]?)".to_string()),
        // e.g. `https://secure.php.net/get/php-7.1.10.tar.bz2/from/this/mirror`
        url(format!(r"[\-.vV]?({nd}{ps}?)")),
    ];

    specs
        .into_iter()
        .map(|(source, re, transform)| VersionParser {
            source,
            regex: Regex::new(&re).unwrap_or_else(|e| panic!("bad version regex {re}: {e}")),
            transform,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::new(s)
    }

    #[test]
    fn comparison() {
        assert_eq!(v("0.1"), v("0.1"));
        assert_eq!(v("0.1").cmp(&v("0.1.0")), Ordering::Equal);
        assert!(v("0.1") < v("0.2"));
        assert!(v("1.2.3") > v("1.2.2"));
        assert!(v("1.2.4") < v("1.2.4.1"));

        assert!(v("1.2.3") > v("1.2.3alpha4"));
        assert!(v("1.2.3") > v("1.2.3beta2"));
        assert!(v("1.2.3") > v("1.2.3rc3"));
        assert!(v("1.2.3") < v("1.2.3-p34"));
    }

    #[test]
    fn head_versions() {
        assert!(v("HEAD") > v("1.2.3"));
        assert!(v("HEAD-abcdef") > v("1.2.3"));
        assert!(v("1.2.3") < v("HEAD"));
        assert!(v("1.2.3") < v("HEAD-fedcba"));
        assert_eq!(v("HEAD-abcdef").cmp(&v("HEAD-fedcba")), Ordering::Equal);
        assert_eq!(v("HEAD").cmp(&v("HEAD-fedcba")), Ordering::Equal);
        assert_eq!(v("HEAD-abcdef").commit(), Some("abcdef"));
        assert_eq!(v("HEAD").commit(), None);
    }

    #[test]
    fn alpha_versions() {
        assert!(v("1.2.3alpha") < v("1.2.3"));
        assert!(v("1.2.3") < v("1.2.3a"));
        assert_eq!(v("1.2.3alpha4").cmp(&v("1.2.3a4")), Ordering::Equal);
        assert_eq!(v("1.2.3alpha4").cmp(&v("1.2.3A4")), Ordering::Equal);
        assert!(v("1.2.3alpha4") > v("1.2.3alpha3"));
        assert!(v("1.2.3alpha4") < v("1.2.3alpha5"));
        assert!(v("1.2.3alpha4") < v("1.2.3alpha10"));
        assert!(v("1.2.3alpha4") < v("1.2.3beta2"));
        assert!(v("1.2.3alpha4") < v("1.2.3rc3"));
        assert!(v("1.2.3alpha4") < v("1.2.3"));
        assert!(v("1.2.3alpha4") < v("1.2.3-p34"));
    }

    #[test]
    fn beta_versions() {
        assert_eq!(v("1.2.3beta2").cmp(&v("1.2.3b2")), Ordering::Equal);
        assert_eq!(v("1.2.3beta2").cmp(&v("1.2.3B2")), Ordering::Equal);
        assert!(v("1.2.3beta2") > v("1.2.3beta1"));
        assert!(v("1.2.3beta2") < v("1.2.3beta3"));
        assert!(v("1.2.3beta2") < v("1.2.3beta10"));
        assert!(v("1.2.3beta2") > v("1.2.3alpha4"));
        assert!(v("1.2.3beta2") < v("1.2.3rc3"));
        assert!(v("1.2.3beta2") < v("1.2.3"));
        assert!(v("1.2.3beta2") < v("1.2.3-p34"));
    }

    #[test]
    fn pre_versions() {
        assert_eq!(v("1.2.3pre9").cmp(&v("1.2.3PRE9")), Ordering::Equal);
        assert!(v("1.2.3pre9") > v("1.2.3pre8"));
        assert!(v("1.2.3pre8") < v("1.2.3pre9"));
        assert!(v("1.2.3pre9") < v("1.2.3pre10"));
        assert!(v("1.2.3pre3") > v("1.2.3alpha2"));
        assert!(v("1.2.3pre3") > v("1.2.3alpha4"));
        assert!(v("1.2.3pre3") > v("1.2.3beta3"));
        assert!(v("1.2.3pre3") > v("1.2.3beta5"));
        assert!(v("1.2.3pre3") < v("1.2.3rc2"));
        assert!(v("1.2.3pre3") < v("1.2.3"));
        assert!(v("1.2.3pre3") < v("1.2.3-p2"));
    }

    #[test]
    fn rc_versions() {
        assert_eq!(v("1.2.3rc3").cmp(&v("1.2.3RC3")), Ordering::Equal);
        assert!(v("1.2.3rc3") > v("1.2.3rc2"));
        assert!(v("1.2.3rc3") < v("1.2.3rc4"));
        assert!(v("1.2.3rc3") < v("1.2.3rc10"));
        assert!(v("1.2.3rc3") > v("1.2.3alpha4"));
        assert!(v("1.2.3rc3") > v("1.2.3beta2"));
        assert!(v("1.2.3rc3") < v("1.2.3"));
        assert!(v("1.2.3rc3") < v("1.2.3-p34"));
    }

    #[test]
    fn patch_level_versions() {
        assert_eq!(v("1.2.3-p34").cmp(&v("1.2.3-P34")), Ordering::Equal);
        assert!(v("1.2.3-p34") > v("1.2.3-p33"));
        assert!(v("1.2.3-p34") < v("1.2.3-p35"));
        assert!(v("1.2.3-p34") > v("1.2.3-p9"));
        assert!(v("1.2.3-p34") > v("1.2.3alpha4"));
        assert!(v("1.2.3-p34") > v("1.2.3beta2"));
        assert!(v("1.2.3-p34") > v("1.2.3rc3"));
        assert!(v("1.2.3-p34") > v("1.2.3"));
    }

    #[test]
    fn post_level_versions() {
        assert!(v("1.2.3.post34") > v("1.2.3.post33"));
        assert!(v("1.2.3.post34") < v("1.2.3.post35"));
        assert!(v("1.2.3.post34") > v("1.2.3rc35"));
        assert!(v("1.2.3.post34") > v("1.2.3alpha35"));
        assert!(v("1.2.3.post34") > v("1.2.3beta35"));
        assert!(v("1.2.3.post34") > v("1.2.3"));
    }

    #[test]
    fn unevenly_padded_versions() {
        assert!(v("2.1.0-p194") < v("2.1-p195"));
        assert!(v("2.1-p195") > v("2.1.0-p194"));
        assert!(v("2.1-p194") < v("2.1.0-p195"));
        assert!(v("2.1.0-p195") > v("2.1-p194"));
        assert!(v("2-p194") < v("2.1-p195"));
    }

    #[test]
    fn null_versions() {
        assert!(Version::null() < v("1"));
        assert!(!(Version::null() > v("0")));
        assert!(v("2.1.0-p194") > Version::null());
    }

    #[test]
    fn erlang_versions_sort() {
        let mut versions: Vec<&str> = vec![
            "R13B02-1", "R13B03", "R13B04", "R14B", "R14B01", "R14B02", "R14B03", "R14B04",
            "R15B01", "R15B02", "R15B03", "R15B03-1", "R16B",
        ];
        let expected = versions.clone();
        versions.sort_by_key(|s| Version::new(s));
        assert_eq!(versions, expected);
    }

    #[test]
    fn version_parts() {
        assert_eq!(v("1.2.3alpha4").major(), Some(Token::Num(1)));
        assert_eq!(v("1.2.3alpha4").minor(), Some(Token::Num(2)));
        assert_eq!(v("1.2.3alpha4").patch(), Some(Token::Num(3)));
        assert_eq!(v("1").minor(), None);
        assert_eq!(v("1.2").patch(), None);
        assert_eq!(v("1.2.3-p4").major_minor(), v("1.2"));
        assert_eq!(v("1.2.3-p4").major_minor_patch(), v("1.2.3"));
        assert_eq!(v("1").major_minor(), v("1"));
        assert_eq!(v("1.2").major_minor_patch(), v("1.2"));
    }

    #[test]
    fn pkg_versions() {
        assert_eq!(PkgVersion::parse("1.0_1"), PkgVersion::new(v("1.0"), 1));
        assert_eq!(PkgVersion::parse("1.0"), PkgVersion::new(v("1.0"), 0));
        assert_eq!(PkgVersion::parse("1.0_0"), PkgVersion::new(v("1.0"), 0));
        assert_eq!(PkgVersion::parse("2.1.4_0"), PkgVersion::new(v("2.1.4"), 0));
        assert_eq!(
            PkgVersion::parse("1.0.1e_1"),
            PkgVersion::new(v("1.0.1e"), 1)
        );
        assert_eq!(PkgVersion::parse("1.2.3_4").version, v("1.2.3"));
        assert_eq!(PkgVersion::parse("1.2.3_4").revision, 4);
        assert_eq!(PkgVersion::parse("1.0_0"), PkgVersion::parse("1.0"));
        assert!(PkgVersion::parse("1.1") > PkgVersion::parse("1.0_1"));
        assert!(PkgVersion::parse("HEAD") > PkgVersion::parse("1.0"));
        assert!(PkgVersion::parse("1.0_1") < PkgVersion::parse("2.0_1"));
        assert!(PkgVersion::parse("1.0") < PkgVersion::parse("HEAD"));
        assert_eq!(PkgVersion::new(v("1.0"), 0).to_string(), "1.0");
        assert_eq!(PkgVersion::new(v("1.0"), 1).to_string(), "1.0_1");
        assert_eq!(PkgVersion::new(v("HEAD"), 1).to_string(), "HEAD_1");
        assert_eq!(
            PkgVersion::new(v("HEAD-ffffff"), 1).to_string(),
            "HEAD-ffffff_1"
        );
    }

    #[track_caller]
    fn detected(url: &str, expected: &str) {
        let got = Version::detect_from_url(url)
            .unwrap_or_else(|| panic!("no version detected from {url}"));
        assert_eq!(got.as_str(), expected, "url: {url}");
    }

    #[test]
    fn parse_returns_null_for_unparseable() {
        assert!(Version::detect_from_url("https://brew.sh/blah.tar").is_none());
        assert!(Version::parse_spec("foo", false).is_none());
    }

    #[test]
    fn detect_from_urls() {
        detected("https://brew.sh/foo.bar.la.1.14.zip", "1.14");
        detected("https://brew.sh/grc_1.1.tar.gz", "1.1");
        detected("https://brew.sh/boost_1_39_0.tar.bz2", "1.39.0");
        detected("https://erlang.org/download/otp_src_R13B.tar.gz", "R13B");
        detected("https://github.com/erlang/otp/tarball/OTP_R15B01", "R15B01");
        detected(
            "https://github.com/erlang/otp/tarball/OTP_R15B03-1",
            "R15B03-1",
        );
        detected(
            "https://kent.dl.sourceforge.net/sourceforge/p7zip/p7zip_9.04_src_all.tar.bz2",
            "9.04",
        );
        detected(
            "https://github.com/sam-github/libnet/tarball/libnet-1.1.4",
            "1.1.4",
        );
        detected(
            "https://codeload.github.com/gsamokovarov/jump/tar.gz/v0.7.1",
            "0.7.1",
        );
        detected(
            "https://camaya.net/download/gloox-1.0-beta7.tar.bz2",
            "1.0-beta7",
        );
        detected(
            "http://sphinxsearch.com/downloads/sphinx-1.10-beta.tar.gz",
            "1.10-beta",
        );
        detected(
            "https://kent.dl.sourceforge.net/sourceforge/astyle/astyle_1.23_macosx.tar.gz",
            "1.23",
        );
        detected(
            "http://www.sfr-fresh.com/linux/misc/dos2unix-3.1.tar.gz",
            "3.1",
        );
        detected("https://brew.sh/foo-arse-1.1-2.tar.gz", "1.1-2");
        detected("https://brew.sh/3.3.04-1.tar.gz", "3.3.04-1");
        detected("https://brew.sh/v1.2-20200102.tar.gz", "1.2-20200102");
        detected("https://brew.sh/v3.6.6-0.2.tar.gz", "3.6.6-0.2");
        detected("https://brew.sh/foo_bar.45.tar.gz", "45");
        detected("https://brew.sh/foo_bar45.tar.gz", "45");
        detected("https://brew.sh/foo-bar-la.1.2.3.tar.gz", "1.2.3");
        detected("https://brew.sh/foo_bar-1.21.tar.gz", "1.21");
        detected(
            "https://sourceforge.net/foo_bar-1.21.tar.gz/download",
            "1.21",
        );
        detected("https://sf.net/foo_bar-1.21.tar.gz/download", "1.21");
        detected("https://github.com/lloyd/yajl/tarball/1.0.5", "1.0.5");
        detected("https://github.com/lloyd/yajl/tarball/v1.2.34", "1.2.34");
        detected("https://brew.sh/mad-0.15.1b.tar.gz", "0.15.1b");
        detected(
            "https://kent.dl.sourceforge.net/sourceforge/lame/lame-398-2.tar.gz",
            "398-2",
        );
        detected(
            "ftp://ftp.ruby-lang.org/pub/ruby/1.9/ruby-1.9.1-p243.tar.gz",
            "1.9.1-p243",
        );
        detected(
            "http://www.alcyone.com/binaries/omega/omega-0.80.2-src.tar.gz",
            "0.80.2",
        );
        detected(
            "https://downloads.xiph.org/releases/vorbis/libvorbis-1.2.2rc1.tar.bz2",
            "1.2.2rc1",
        );
        detected(
            "https://ftp.mozilla.org/pub/mozilla.org/js/js-1.8.0-rc1.tar.gz",
            "1.8.0-rc1",
        );
        detected(
            "http://rephial.org/downloads/3.0/angband-3.0.9b-src.tar.gz",
            "3.0.9b",
        );
        detected(
            "https://www.monkey.org/~provos/libevent-1.4.14b-stable.tar.gz",
            "1.4.14b",
        );
        detected(
            "https://ftp.de.debian.org/debian/pool/main/s/sl/sl_3.03.orig.tar.gz",
            "3.03",
        );
        detected(
            "https://ftp.de.debian.org/debian/pool/main/m/mmv/mmv_1.01b.orig.tar.gz",
            "1.01b",
        );
        detected(
            "https://deb.debian.org/debian/pool/main/e/example/example_1.orig.tar.gz",
            "1",
        );
        detected(
            "https://deb.debian.org/debian/pool/main/e/example/example_20040914.orig.tar.gz",
            "20040914",
        );
        detected(
            "https://homebrew.bintray.com/bottles/qt-4.8.0.big_sur.bottle.tar.gz",
            "4.8.0",
        );
        detected(
            "https://homebrew.bintray.com/bottles/qt-4.8.1.big_sur.bottle.1.tar.gz",
            "4.8.1",
        );
        detected(
            "https://homebrew.bintray.com/bottles/erlang-R15B.big_sur.bottle.tar.gz",
            "R15B",
        );
        detected(
            "https://homebrew.bintray.com/bottles/erlang-R15B01.monterey.bottle.tar.gz",
            "R15B01",
        );
        detected(
            "https://homebrew.bintray.com/bottles/erlang-R15B03-1.monterey.bottle.tar.gz",
            "R15B03-1",
        );
        detected(
            "https://downloads.sf.net/project/machomebrew/mirror/ImageMagick-6.7.5-7.tar.bz2",
            "6.7.5-7",
        );
        detected(
            "https://homebrew.bintray.com/bottles/imagemagick-6.7.5-7.big_sur.bottle.tar.gz",
            "6.7.5-7",
        );
        detected(
            "https://homebrew.bintray.com/bottles/imagemagick-6.7.5-7.lion.bottle.1.tar.gz",
            "6.7.5-7",
        );
        detected("https://brew.sh/dada-v2017-04-17.tar.gz", "2017-04-17");
        detected(
            "https://registry.npmjs.org/@angular/cli/-/cli-1.3.0-beta.1.tgz",
            "1.3.0-beta.1",
        );
        detected(
            "https://github.com/dlang/dmd/archive/v2.074.0-beta1.tar.gz",
            "2.074.0-beta1",
        );
        detected(
            "https://github.com/dlang/dmd/archive/v2.074.0-rc1.tar.gz",
            "2.074.0-rc1",
        );
        detected(
            "https://github.com/premake/premake-core/releases/download/v5.0.0-alpha10/premake-5.0.0-alpha10-src.zip",
            "5.0.0-alpha10",
        );
        detected(
            "https://mirrors.jenkins-ci.org/war/1.486/jenkins.war",
            "1.486",
        );
        detected(
            "https://github.com/hechoendrupal/DrupalConsole/releases/download/0.10.11/drupal.phar",
            "0.10.11",
        );
        detected(
            "https://github.com/clojure/clojurescript/releases/download/r1.9.293/cljs.jar",
            "1.9.293",
        );
        detected(
            "https://github.com/fibjs/fibjs/releases/download/v0.6.1/fullsrc.zip",
            "0.6.1",
        );
        detected(
            "https://wwwlehre.dhbw-stuttgart.de/~sschulz/WORK/E_DOWNLOAD/V_1.9/E.tgz",
            "1.9",
        );
        detected(
            "https://github.com/dvorka-oss/hstr/releases/download/v3.2/hstr-3.2.0-tarball.tgz",
            "3.2",
        );
        detected(
            "https://github.com/JustArchi/ArchiSteamFarm/releases/download/2.3.2.0/ASF.zip",
            "2.3.2.0",
        );
        detected(
            "https://people.gnome.org/~newren/eg/download/1.7.5.2/eg",
            "1.7.5.2",
        );
        detected(
            "https://www.antlr.org/download/antlr-3.4-complete.jar",
            "3.4",
        );
        detected(
            "https://cdn.nuxeo.com/nuxeo-9.2/nuxeo-server-9.2-tomcat.zip",
            "9.2",
        );
        detected(
            "https://search.maven.org/remotecontent?filepath=com/facebook/presto/presto-cli/0.181/presto-cli-0.181-executable.jar",
            "0.181",
        );
        detected(
            "https://search.maven.org/remotecontent?filepath=org/apache/orc/orc-tools/1.2.3/orc-tools-1.2.3-uber.jar",
            "1.2.3",
        );
        detected(
            "https://www.apache.org/dyn/closer.cgi?path=/cassandra/1.2.0/apache-cassandra-1.2.0-rc2-bin.tar.gz",
            "1.2.0-rc2",
        );
        detected("https://www.ijg.org/files/jpegsrc.v8d.tar.gz", "8d");
        detected(
            "https://www.haskell.org/ghc/dist/7.0.4/ghc-7.0.4-x86_64-apple-darwin.tar.bz2",
            "7.0.4",
        );
        detected(
            "https://www.haskell.org/ghc/dist/7.0.4/ghc-7.0.4-i386-apple-darwin.tar.bz2",
            "7.0.4",
        );
        detected("https://pypy.org/download/pypy-1.4.1-osx.tar.bz2", "1.4.1");
        detected(
            "https://www.openssl.org/source/openssl-0.9.8s.tar.gz",
            "0.9.8s",
        );
        detected(
            "ftp://ftp.visi.com/users/hawkeyd/X/Xaw3d-1.5E.tar.gz",
            "1.5E",
        );
        detected(
            "https://downloads.sourceforge.net/project/assimp/assimp-2.0/assimp--2.0.863-sdk.zip",
            "2.0.863",
        );
        detected(
            "https://common-lisp.net/project/cmucl/downloads/release/20c/cmucl-20c-x86-darwin.tar.bz2",
            "20c",
        );
        detected(
            "https://downloads.sourceforge.net/project/fann/fann/2.1.0beta/fann-2.1.0beta.zip",
            "2.1.0beta",
        );
        detected(
            "ftp://iges.org/grads/2.0/grads-2.0.1-bin-darwin9.8-intel.tar.gz",
            "2.0.1",
        );
        detected("https://haxe.org/file/haxe-2.08-osx.tar.gz", "2.08");
        detected(
            "ftp://ftp.cac.washington.edu/imap/imap-2007f.tar.gz",
            "2007f",
        );
        detected(
            "https://downloads.sourceforge.net/project/x3270/x3270/3.3.12ga7/suite3270-3.3.12ga7-src.tgz",
            "3.3.12ga7",
        );
        detected(
            "http://www.gedanken.demon.co.uk/download-wwwoffle/wwwoffle-2.9h.tgz",
            "2.9h",
        );
        detected(
            "http://synergy.googlecode.com/files/synergy-1.3.6p2-MacOSX-Universal.zip",
            "1.3.6p2",
        );
        detected(
            "https://downloads.sourceforge.net/project/fontforge/fontforge-source/fontforge_full-20120731-b.tar.bz2",
            "20120731",
        );
        detected(
            "https://github.com/downloads/ezsystems/ezpublish-legacy/ezpublish_community_project-2011.10-with_ezc.tar.bz2",
            "2011.10",
        );
        detected(
            "http://loop-aes.sourceforge.net/aespipe/aespipe-v2.4c.tar.bz2",
            "2.4c",
        );
        detected(
            "https://ftpmirror.gnu.org/libmicrohttpd/libmicrohttpd-0.9.17-w32.zip",
            "0.9.17",
        );
        detected(
            "https://ftpmirror.gnu.org/libidn/libidn-1.29-win64.zip",
            "1.29",
        );
        detected(
            "https://github.com/barricklab/breseq/releases/download/v0.35.1/breseq-0.35.1.Source.tar.gz",
            "0.35.1",
        );
        detected(
            "https://download.jboss.org/wildfly/20.0.1.Final/wildfly-20.0.1.Final.tar.gz",
            "20.0.1",
        );
        detected(
            "https://github.com/trinityrnaseq/trinityrnaseq/releases/download/v2.10.0/trinityrnaseq-v2.10.0.FULL.tar.gz",
            "2.10.0",
        );
        detected(
            "https://ftpmirror.gnu.org/mtools/mtools-4.0.18-1.i686.rpm",
            "4.0.18-1",
        );
        detected(
            "https://ftpmirror.gnu.org/autogen/autogen-5.5.7-5.i386.rpm",
            "5.5.7-5",
        );
        detected(
            "https://ftpmirror.gnu.org/libtasn1/libtasn1-2.8-x86.zip",
            "2.8",
        );
        detected(
            "https://ftpmirror.gnu.org/libtasn1/libtasn1-2.8-x64.zip",
            "2.8",
        );
        detected(
            "https://ftpmirror.gnu.org/mtools/mtools_4.0.18_i386.deb",
            "4.0.18",
        );
        detected(
            "https://opam.ocaml.org/archives/lablgtk.2.18.3+opam.tar.gz",
            "2.18.3",
        );
        detected("https://opam.ocaml.org/archives/sha.1.9+opam.tar.gz", "1.9");
        detected(
            "https://opam.ocaml.org/archives/ppx_tools.0.99.2+opam.tar.gz",
            "0.99.2",
        );
        detected(
            "https://opam.ocaml.org/archives/easy-format.1.0.2+opam.tar.gz",
            "1.0.2",
        );
        detected("https://waf.io/waf-1.8.12", "1.8.12");
        detected("https://my.datomic.com/downloads/free/0.9.1234", "0.9.1234");
        detected("https://my.datomic.com/downloads/free/1.2.3", "1.2.3");
        detected(
            "ftp://gcc.gnu.org/pub/gcc/snapshots/6-20151227/gcc-6-20151227.tar.bz2",
            "6-20151227",
        );
        detected(
            "https://php.net/get/php-7.1.10.tar.gz/from/this/mirror",
            "7.1.10",
        );
    }

    #[test]
    fn detect_from_tag() {
        assert_eq!(
            Version::detect("https://github.com/foo/bar.git", Some("v1.2.3-stable"))
                .unwrap()
                .as_str(),
            "1.2.3"
        );
        assert_eq!(
            Version::detect("https://github.com/foo/bar.git", Some("v1.2.3-beta1"))
                .unwrap()
                .as_str(),
            "1.2.3-beta1"
        );
    }
}
