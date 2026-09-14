//! Formula file parsing: `class Foo < Formula ... end` into a [`FormulaEntry`].

use std::collections::BTreeMap;

use serde_json::{Map, Value as Json};

use crate::model::FormulaEntry;
use crate::platform::BottleTag;

use super::host::HostCtx;
use super::scanner::{Line, find_block_end, logical_lines};
use super::value::{Args, RValue, parse_args};

/// `service do` fields in the order `Service#to_hash` serialises them.
const SERVICE_KEY_ORDER: [&str; 19] = [
    "run_type",
    "interval",
    "cron",
    "keep_alive",
    "launch_only_once",
    "require_root",
    "environment_variables",
    "working_dir",
    "root_dir",
    "input_path",
    "log_path",
    "error_log_path",
    "restart_delay",
    "throttle_interval",
    "stop_timeout",
    "nice",
    "process_type",
    "macos_legacy_timers",
    "sockets",
];

/// Result of parsing one formula file.
#[derive(Debug, Clone)]
pub struct ParsedFormula {
    pub entry: FormulaEntry,
    pub bottle_root_url: Option<String>,
    pub has_install_method: bool,
    /// `true` when a `class ... < Formula` header was found.
    pub is_formula: bool,
}

struct Parser<'a> {
    host: &'a HostCtx,
    vars: BTreeMap<String, String>,
    entry: FormulaEntry,
    bottle_root_url: Option<String>,
    has_install: bool,
    saw_class: bool,
    /// `sha256` inside `bottle do` must not overwrite the source checksum.
    service: ServiceBuilder,
}

#[derive(Default)]
struct ServiceBuilder {
    args: Vec<(String, Json)>,
    run: Option<Json>,
    run_kwargs: Option<Json>,
    name: Option<Json>,
}

/// Base variables every formula file resolves against.
pub fn formula_vars(name: &str, version: Option<&str>) -> BTreeMap<String, String> {
    let opt = format!("$HOMEBREW_PREFIX/opt/{name}");
    let mut vars = BTreeMap::from([
        ("name".to_string(), name.to_string()),
        (
            "HOMEBREW_PREFIX".to_string(),
            "$HOMEBREW_PREFIX".to_string(),
        ),
        (
            "HOMEBREW_CELLAR".to_string(),
            "$HOMEBREW_CELLAR".to_string(),
        ),
        ("prefix".to_string(), opt.clone()),
        ("opt_prefix".to_string(), opt.clone()),
        ("bin".to_string(), format!("{opt}/bin")),
        ("opt_bin".to_string(), format!("{opt}/bin")),
        ("sbin".to_string(), format!("{opt}/sbin")),
        ("opt_sbin".to_string(), format!("{opt}/sbin")),
        ("libexec".to_string(), format!("{opt}/libexec")),
        ("opt_libexec".to_string(), format!("{opt}/libexec")),
        ("lib".to_string(), format!("{opt}/lib")),
        ("include".to_string(), format!("{opt}/include")),
        ("share".to_string(), format!("{opt}/share")),
        ("pkgshare".to_string(), format!("{opt}/share/{name}")),
        ("opt_pkgshare".to_string(), format!("{opt}/share/{name}")),
        ("man".to_string(), format!("{opt}/share/man")),
        ("etc".to_string(), "$HOMEBREW_PREFIX/etc".to_string()),
        ("var".to_string(), "$HOMEBREW_PREFIX/var".to_string()),
        ("HOME".to_string(), "/$HOME".to_string()),
        (
            "std_service_path_env".to_string(),
            "$HOMEBREW_PREFIX/bin:$HOMEBREW_PREFIX/sbin:/usr/bin:/bin:/usr/sbin:/sbin".to_string(),
        ),
    ]);
    if let Some(v) = version {
        vars.insert("version".to_string(), v.to_string());
    }
    vars
}

/// Parse formula source. `name` comes from the file path, not the class name.
pub fn parse_formula(source: &str, name: &str, tap: &str, tag: &BottleTag) -> ParsedFormula {
    let host = HostCtx::from_tag(tag);
    let lines = logical_lines(source);

    // First pass: find the declared version (or detect one from the URL) so the
    // second pass can resolve `#{version}` interpolations.
    let first = run_pass(&lines, name, tap, &host, formula_vars(name, None));
    let version = first
        .entry
        .stable_version
        .clone()
        .or_else(|| {
            first
                .entry
                .stable_url()
                .and_then(crate::version::Version::detect_from_url)
                .map(|v| v.to_string())
        })
        .or_else(|| {
            first
                .entry
                .stable_url_args
                .get(1)
                .and_then(|v| v.get(":tag"))
                .and_then(Json::as_str)
                .map(|t| t.trim_start_matches('v').to_string())
        });

    let mut out = run_pass(
        &lines,
        name,
        tap,
        &host,
        formula_vars(name, version.as_deref()),
    );
    if out.entry.stable_version.is_none() {
        out.entry.stable_version = version;
    }
    out
}

fn run_pass(
    lines: &[Line],
    name: &str,
    tap: &str,
    host: &HostCtx,
    vars: BTreeMap<String, String>,
) -> ParsedFormula {
    let mut entry = FormulaEntry {
        name: name.to_string(),
        tap: tap.to_string(),
        ..Default::default()
    };
    entry.name = name.to_string();
    let mut parser = Parser {
        host,
        vars,
        entry,
        bottle_root_url: None,
        has_install: false,
        saw_class: false,
        service: ServiceBuilder::default(),
    };
    parser.walk(lines, 0, lines.len());
    parser.finish_service();
    ParsedFormula {
        entry: parser.entry,
        bottle_root_url: parser.bottle_root_url,
        has_install_method: parser.has_install,
        is_formula: parser.saw_class,
    }
}

impl Parser<'_> {
    fn args(&self, line: &Line) -> Args {
        parse_args(line.args(), &self.vars)
    }

    fn walk(&mut self, lines: &[Line], start: usize, end: usize) {
        let mut i = start;
        while i < end {
            let line = &lines[i];
            let head = line.head();
            if line.block_delta() > 0 {
                let close = find_block_end(lines, i).min(end.saturating_sub(1).max(i));
                self.block(lines, i, close, head);
                i = close + 1;
                continue;
            }
            self.statement(line, head);
            i += 1;
        }
    }

    fn block(&mut self, lines: &[Line], open: usize, close: usize, head: &str) {
        let line = &lines[open];
        match head {
            "class" => {
                if line.text.contains("< Formula") || line.text.contains("<Formula") {
                    self.saw_class = true;
                }
                self.walk(lines, open + 1, close);
            }
            "module" | "begin" | "stable" => self.walk(lines, open + 1, close),
            "if" | "unless" => self.if_chain(lines, open, close),
            "bottle" => self.bottle_block(lines, open + 1, close),
            "service" => self.service_block(lines, open + 1, close),
            "caveats" => {
                if let Some(text) = lines[open..=close].iter().find_map(|l| l.heredocs.first()) {
                    self.entry.caveats = Some(self.interpolated(text));
                }
            }
            "def" => {
                let what = line.text.trim_start_matches("def").trim();
                if what.starts_with("install") {
                    self.has_install = true;
                } else if what.starts_with("post_install") {
                    // Only the Ruby `brew` can run this; `ops::postinstall`
                    // hands it over rather than silently skipping it.
                    self.entry.post_install_defined = true;
                } else if what.starts_with("caveats")
                    && let Some(text) = lines[open..=close].iter().find_map(|l| l.heredocs.first())
                {
                    self.entry.caveats = Some(self.interpolated(text));
                }
            }
            // `livecheck`, `test`, `resource`, `patch`, `head`, `pour_bottle?`,
            // `plist_options` and everything else are skipped.
            _ => {
                if head.starts_with("on_")
                    && self.host.on_block_active(head, line.args()) == Some(true)
                {
                    self.walk(lines, open + 1, close);
                }
            }
        }
    }

    /// `if` / `elsif` / `else` at the same level: keep the first matching branch.
    fn if_chain(&mut self, lines: &[Line], open: usize, close: usize) {
        let head = lines[open].head();
        let first_cond = lines[open].args().to_string();
        let negate = head == "unless";

        // (condition, body start, body end); `None` marks the `else` branch.
        let mut branches: Vec<(Option<String>, usize, usize)> = vec![];
        let mut cond: Option<Option<String>> = Some(Some(first_cond));
        let mut body_start = open + 1;
        let mut depth = 0i32;
        for (k, line) in lines.iter().enumerate().take(close).skip(open + 1) {
            if depth == 0 {
                let h = line.head();
                if h == "elsif" || h == "else" {
                    branches.push((cond.take().flatten(), body_start, k));
                    cond = Some((h == "elsif").then(|| line.args().to_string()));
                    body_start = k + 1;
                    continue;
                }
            }
            depth += line.block_delta();
        }
        branches.push((cond.flatten(), body_start, close));

        for (index, (cond, s, e)) in branches.iter().enumerate() {
            let taken = match cond {
                None => true,
                Some(expr) => {
                    let value = self.host.eval(expr);
                    let value = if index == 0 && negate {
                        value.map(|b| !b)
                    } else {
                        value
                    };
                    value == Some(true)
                }
            };
            if taken {
                self.walk(lines, *s, *e);
                return;
            }
        }
    }

    fn interpolated(&self, text: &str) -> String {
        super::value::unescape(&text.replace('\\', "\\\\"), true, &self.vars)
    }

    // -----------------------------------------------------------------------
    // statements
    // -----------------------------------------------------------------------

    fn statement(&mut self, line: &Line, head: &str) {
        let args = self.args(line);
        match head {
            "desc" => self.entry.desc = args.first_str(),
            "homepage" => self.entry.homepage = args.first_str(),
            "license" => self.entry.license = Some(license_json(&args)),
            "url" => self.entry.stable_url_args = url_args(&args),
            "head" => self.entry.head_url_args = url_args(&args),
            "version" => self.entry.stable_version = args.first_str(),
            "sha256" => {
                if let Some(s) = args.first_str() {
                    self.entry.stable_checksum = Some(s);
                }
            }
            "revision" => {
                self.entry.revision = args.first().and_then(RValue::as_int).unwrap_or(0) as u32
            }
            "version_scheme" => {
                self.entry.version_scheme =
                    args.first().and_then(RValue::as_int).unwrap_or(0) as u32
            }
            "depends_on" => {
                if let Some(v) = depends_on_json(&args) {
                    self.entry.stable_dependencies.push(v);
                }
            }
            "uses_from_macos" => {
                if let Some(v) = uses_from_macos_json(&args) {
                    self.entry.stable_uses_from_macos.push(v);
                }
            }
            "keg_only" => self.entry.keg_only_args = keg_only_args(&args),
            "conflicts_with" => {
                for v in conflicts_json(&args) {
                    self.entry.conflicts.push(v);
                }
            }
            "deprecate!" => self.entry.deprecate_args = Some(kwargs_json(&args)),
            "disable!" => self.entry.disable_args = Some(kwargs_json(&args)),
            "link_overwrite" => {
                for v in &args.positional {
                    if let Some(s) = v.to_text() {
                        self.entry.link_overwrite_paths.push(s);
                    }
                }
            }
            "caveats" => {
                if let Some(text) = line.heredocs.first() {
                    self.entry.caveats = Some(self.interpolated(text));
                } else if let Some(s) = args.first_str() {
                    self.entry.caveats = Some(s);
                }
            }
            // `mirror`, `option`, `bottle :unneeded`, `patch`, `pour_bottle?` and
            // anything else are irrelevant to the metadata fastbrew needs.
            _ => {}
        }
    }

    // -----------------------------------------------------------------------
    // bottle do
    // -----------------------------------------------------------------------

    fn bottle_block(&mut self, lines: &[Line], start: usize, end: usize) {
        let host_tag = self.host.tag.to_string();
        let mut block_cellar: Option<String> = None;
        let mut found: Option<(String, Option<String>, bool)> = None; // (sha, cellar, is_all)

        let mut i = start;
        while i < end {
            let line = &lines[i];
            if line.block_delta() > 0 {
                i = find_block_end(lines, i) + 1;
                continue;
            }
            let args = self.args(line);
            match line.head() {
                "root_url" => self.bottle_root_url = args.first_str(),
                "rebuild" => {
                    self.entry.bottle_rebuild =
                        args.first().and_then(RValue::as_int).unwrap_or(0) as u32
                }
                "cellar" => block_cellar = cellar_text(args.first()),
                "sha256" => {
                    let mut line_cellar = block_cellar.clone();
                    let mut tags: Vec<(String, String)> = vec![];
                    // `sha256 "abc" => :arm64_tahoe` (legacy) and
                    // `sha256 cellar: :any, arm64_tahoe: "abc"`.
                    for (key, value) in &args.kwargs {
                        if key == "cellar" {
                            line_cellar = cellar_text(Some(value));
                        } else if is_hex_digest(key) {
                            if let Some(tag) = value.as_sym() {
                                tags.push((tag.to_string(), key.clone()));
                            }
                        } else if let Some(sha) = value.as_str() {
                            tags.push((key.clone(), sha.to_string()));
                        }
                    }
                    for (tag, sha) in tags {
                        if tag == host_tag {
                            found = Some((sha, line_cellar.clone(), false));
                        } else if tag == "all" && found.is_none() {
                            found = Some((sha, line_cellar.clone(), true));
                        }
                    }
                }
                _ => {}
            }
            i += 1;
        }

        if let Some((sha, cellar, is_all)) = found {
            self.entry.bottle_checksum = Some(sha);
            // An absent cellar means `:any_skip_relocation` (COMPAT 1.3).
            self.entry.bottle_cellar = match cellar.as_deref() {
                None | Some(":any_skip_relocation") => None,
                Some(other) => Some(other.to_string()),
            };
            if is_all {
                self.entry.bottle_tag = Some(":all".to_string());
            }
        }
    }

    // -----------------------------------------------------------------------
    // service do
    // -----------------------------------------------------------------------

    fn service_block(&mut self, lines: &[Line], start: usize, end: usize) {
        let mut i = start;
        while i < end {
            let line = &lines[i];
            if line.block_delta() > 0 {
                i = find_block_end(lines, i) + 1;
                continue;
            }
            let args = self.args(line);
            let head = line.head();
            match head {
                "run" => {
                    if !args.positional.is_empty() {
                        self.service.run = Some(args.positional[0].to_json());
                    } else if !args.kwargs.is_empty() {
                        self.service.run_kwargs = Some(RValue::Hash(args.kwargs.clone()).to_json());
                    }
                }
                "name" => self.service.name = Some(RValue::Hash(args.kwargs.clone()).to_json()),
                "keep_alive" => {
                    let value = if let Some(b) = args.first().and_then(RValue::as_bool) {
                        Json::Object(Map::from_iter([(":always".to_string(), Json::Bool(b))]))
                    } else {
                        RValue::Hash(args.kwargs.clone()).to_json()
                    };
                    self.service.args.push(("keep_alive".into(), value));
                }
                "environment_variables" => {
                    let mut map = Map::new();
                    for (k, v) in &args.kwargs {
                        let text = match v {
                            RValue::Sym(s) if s == "std_service_path_env" => {
                                self.vars.get("std_service_path_env").cloned()
                            }
                            other => other.to_text(),
                        };
                        if let Some(text) = text {
                            map.insert(format!(":{k}"), Json::String(text));
                        }
                    }
                    self.service
                        .args
                        .push(("environment_variables".into(), Json::Object(map)));
                }
                "sockets" => {
                    let value = if let Some(s) = args.first_str() {
                        Json::String(s)
                    } else {
                        RValue::Hash(args.kwargs.clone()).to_json()
                    };
                    self.service.args.push(("sockets".into(), value));
                }
                "run_type" | "process_type" => {
                    if let Some(s) = args.first().and_then(RValue::as_sym) {
                        self.service
                            .args
                            .push((head.to_string(), Json::String(format!(":{s}"))));
                    }
                }
                "working_dir" | "root_dir" | "input_path" | "log_path" | "error_log_path"
                | "cron" => {
                    if let Some(s) = args.first_str() {
                        self.service.args.push((head.to_string(), Json::String(s)));
                    }
                }
                "interval" | "restart_delay" | "throttle_interval" | "stop_timeout" | "nice" => {
                    if let Some(n) = args.first().and_then(RValue::as_int) {
                        self.service
                            .args
                            .push((head.to_string(), Json::Number(n.into())));
                    }
                }
                "require_root" | "launch_only_once" | "macos_legacy_timers" | "run_at_load" => {
                    let b = args.first().and_then(RValue::as_bool).unwrap_or(true);
                    self.service.args.push((head.to_string(), Json::Bool(b)));
                }
                _ => {}
            }
            i += 1;
        }
    }

    /// Emit the collected `service` block in the API's key order.
    fn finish_service(&mut self) {
        if let Some(name) = self.service.name.take() {
            self.entry.service_name_args = vec![name];
        }
        match (self.service.run.take(), self.service.run_kwargs.take()) {
            (Some(run), _) => self.entry.service_run_args = vec![run],
            (None, Some(kwargs)) => self.entry.service_run_kwargs = Some(kwargs),
            (None, None) => {
                // Without a run command the block only documents a service file.
                self.service.args.clear();
            }
        }
        if self.service.args.is_empty() {
            return;
        }
        let mut out: Vec<Json> = vec![];
        for key in SERVICE_KEY_ORDER {
            for (k, v) in &self.service.args {
                if k == key && !is_blank(v) {
                    out.push(Json::Array(vec![Json::String(format!(":{k}")), v.clone()]));
                }
            }
        }
        // `run_type` defaults to `:immediate` and is always serialised.
        if !out
            .iter()
            .any(|v| v.get(0).and_then(Json::as_str) == Some(":run_type"))
        {
            out.insert(
                0,
                Json::Array(vec![
                    Json::String(":run_type".into()),
                    Json::String(":immediate".into()),
                ]),
            );
        }
        self.entry.service_args = out;
    }
}

/// Ruby's `compact_blank`: drop nil, false, empty strings, arrays and hashes.
fn is_blank(v: &Json) -> bool {
    match v {
        Json::Null => true,
        Json::Bool(b) => !*b,
        Json::String(s) => s.is_empty(),
        Json::Array(a) => a.is_empty(),
        Json::Object(o) => o.is_empty(),
        _ => false,
    }
}

fn cellar_text(v: Option<&RValue>) -> Option<String> {
    match v? {
        RValue::Sym(s) => Some(format!(":{s}")),
        RValue::Str(s) => Some(s.clone()),
        _ => None,
    }
}

fn is_hex_digest(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn license_json(args: &Args) -> Json {
    if let Some(v) = args.first() {
        return v.to_json();
    }
    RValue::Hash(args.kwargs.clone()).to_json()
}

/// `[url]` or `[url, {":tag": .., ":revision": .., ":using": ..}]`.
fn url_args(args: &Args) -> Vec<Json> {
    let Some(url) = args.first_str() else {
        return vec![];
    };
    let mut out = vec![Json::String(url)];
    let kwargs: Vec<(String, RValue)> = args
        .kwargs
        .iter()
        .filter(|(k, _)| {
            matches!(
                k.as_str(),
                "tag" | "revision" | "using" | "branch" | "specs" | "verified"
            )
        })
        .cloned()
        .collect();
    if !kwargs.is_empty() {
        out.push(RValue::Hash(kwargs).to_json());
    }
    out
}

fn tag_json(v: &RValue) -> Json {
    match v {
        RValue::Sym(s) => Json::String(format!(":{s}")),
        RValue::Array(items) => Json::Array(items.iter().map(tag_json).collect()),
        other => other.to_json(),
    }
}

/// `"x"` | `{"x": ":build"}` | `{"x": [":build", ":test"]}`; `None` for requirements.
fn depends_on_json(args: &Args) -> Option<Json> {
    if let Some(name) = args.first().and_then(RValue::as_str) {
        return Some(Json::String(name.to_string()));
    }
    // `depends_on "x" => :build` lands in kwargs as ("x", :build).
    for (key, value) in &args.kwargs {
        // `macos:`, `arch:`, `xcode:`, `maximum_macos:` are requirements, not deps.
        if matches!(
            key.as_str(),
            "macos" | "arch" | "xcode" | "maximum_macos" | "linux" | "codesign" | "java"
        ) {
            continue;
        }
        let mut map = Map::new();
        map.insert(key.clone(), tag_json(value));
        return Some(Json::Object(map));
    }
    None
}

/// `["x"]` | `["x", {":since": ":sequoia"}]` | `[{"x": ":build"}]`.
fn uses_from_macos_json(args: &Args) -> Option<Json> {
    let since = args
        .kwarg("since")
        .map(|v| Json::Object(Map::from_iter([(":since".to_string(), tag_json(v))])));
    let dep = if let Some(name) = args.first().and_then(RValue::as_str) {
        Json::String(name.to_string())
    } else {
        let (key, value) = args
            .kwargs
            .iter()
            .find(|(k, _)| k != "since" && k != "bounds")?;
        Json::Object(Map::from_iter([(key.clone(), tag_json(value))]))
    };
    let mut out = vec![dep];
    if let Some(since) = since {
        out.push(since);
    }
    Some(Json::Array(out))
}

fn keg_only_args(args: &Args) -> Vec<Json> {
    let mut out: Vec<Json> = vec![];
    for v in &args.positional {
        match v {
            RValue::Sym(s) => out.push(Json::String(format!(":{s}"))),
            RValue::Str(s) => out.push(Json::String(s.clone())),
            _ => {}
        }
    }
    out
}

/// `[["name", {":because": "reason"}]]`.
fn conflicts_json(args: &Args) -> Vec<Json> {
    let because = args
        .kwarg("because")
        .and_then(RValue::to_text)
        .map(|r| Json::Object(Map::from_iter([(":because".to_string(), Json::String(r))])));
    args.positional
        .iter()
        .filter_map(RValue::as_str)
        .map(|name| {
            let mut item = vec![Json::String(name.to_string())];
            if let Some(b) = &because {
                item.push(b.clone());
            }
            Json::Array(item)
        })
        .collect()
}

/// Keyword arguments as a `{":key": value}` object (`deprecate!`, `disable!`).
fn kwargs_json(args: &Args) -> Json {
    RValue::Hash(args.kwargs.clone()).to_json()
}
