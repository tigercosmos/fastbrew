//! Cask file parsing: `cask "token" do ... end` into a [`CaskEntry`].

use std::collections::BTreeMap;

use serde_json::{Map, Value as Json};

use crate::model::CaskEntry;
use crate::platform::BottleTag;

use super::host::HostCtx;
use super::scanner::{Line, find_block_end, logical_lines};
use super::value::{Args, RValue, parse_args};

/// Stanzas that take a list of paths plus optional keyword arguments.
const PATH_ARTIFACTS: [&str; 22] = [
    "app",
    "binary",
    "manpage",
    "bash_completion",
    "zsh_completion",
    "fish_completion",
    "font",
    "suite",
    "artifact",
    "pkg",
    "qlplugin",
    "prefpane",
    "screen_saver",
    "service",
    "input_method",
    "dictionary",
    "colorpicker",
    "mdimporter",
    "vst_plugin",
    "vst3_plugin",
    "audio_unit_plugin",
    "keyboard_layout",
];

/// Stanzas whose single argument is a hash of directives.
const HASH_ARTIFACTS: [&str; 3] = ["uninstall", "zap", "installer"];

/// Blocks of arbitrary Ruby that fastbrew cannot evaluate.
const RUBY_BLOCKS: [&str; 4] = [
    "preflight",
    "postflight",
    "uninstall_preflight",
    "uninstall_postflight",
];

#[derive(Debug, Clone)]
pub struct ParsedCask {
    pub entry: CaskEntry,
    pub is_cask: bool,
    /// A `preflight`/`postflight` block needs the Ruby `brew`.
    pub has_ruby_blocks: bool,
}

/// Base variables a cask file resolves against.
pub fn cask_vars(token: &str, version: Option<&str>, host: &HostCtx) -> BTreeMap<String, String> {
    let mut vars = BTreeMap::from([
        ("token".to_string(), token.to_string()),
        ("appdir".to_string(), "$APPDIR".to_string()),
        (
            "HOMEBREW_PREFIX".to_string(),
            "$HOMEBREW_PREFIX".to_string(),
        ),
        (
            "HOMEBREW_CELLAR".to_string(),
            "$HOMEBREW_CELLAR".to_string(),
        ),
        ("HOME".to_string(), "/$HOME".to_string()),
        (
            "arch".to_string(),
            if host.arm { "arm64" } else { "x86_64" }.to_string(),
        ),
        (
            "Hardware::CPU.arch".to_string(),
            if host.arm { "arm64" } else { "x86_64" }.to_string(),
        ),
    ]);
    if let Some(v) = version {
        vars.insert("version".to_string(), v.to_string());
    }
    vars
}

pub fn parse_cask(source: &str, token: &str, tap: &str, tag: &BottleTag) -> ParsedCask {
    let host = HostCtx::from_tag(tag);
    let lines = logical_lines(source);
    let first = run_pass(&lines, token, tap, &host, cask_vars(token, None, &host));
    let version = first.entry.version.clone();
    run_pass(
        &lines,
        token,
        tap,
        &host,
        cask_vars(token, version.as_deref(), &host),
    )
}

fn run_pass(
    lines: &[Line],
    token: &str,
    tap: &str,
    host: &HostCtx,
    vars: BTreeMap<String, String>,
) -> ParsedCask {
    let mut parser = Parser {
        host,
        vars,
        entry: CaskEntry {
            token: token.to_string(),
            tap_string: Some(tap.to_string()),
            ..Default::default()
        },
        is_cask: false,
        has_ruby_blocks: false,
    };
    parser.walk(lines, 0, lines.len());
    ParsedCask {
        entry: parser.entry,
        is_cask: parser.is_cask,
        has_ruby_blocks: parser.has_ruby_blocks,
    }
}

struct Parser<'a> {
    host: &'a HostCtx,
    vars: BTreeMap<String, String>,
    entry: CaskEntry,
    is_cask: bool,
    has_ruby_blocks: bool,
}

impl Parser<'_> {
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
            "cask" => {
                self.is_cask = true;
                self.walk(lines, open + 1, close);
            }
            "if" | "unless" => self.if_chain(lines, open, close),
            "caveats" => {
                if let Some(text) = lines[open..=close].iter().find_map(|l| l.heredocs.first()) {
                    self.entry.raw_caveats = Some(Json::String(text.clone()));
                }
            }
            _ if RUBY_BLOCKS.contains(&head) => self.has_ruby_blocks = true,
            // `livecheck` and `language` bodies are not metadata.
            _ => {
                if head.starts_with("on_")
                    && self.host.on_block_active(head, line.args()) == Some(true)
                {
                    self.walk(lines, open + 1, close);
                }
            }
        }
    }

    fn if_chain(&mut self, lines: &[Line], open: usize, close: usize) {
        let negate = lines[open].head() == "unless";
        let mut branches: Vec<(Option<String>, usize, usize)> = vec![];
        let mut cond: Option<Option<String>> = Some(Some(lines[open].args().to_string()));
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

    fn statement(&mut self, line: &Line, head: &str) {
        let args = parse_args(line.args(), &self.vars);
        match head {
            "version" => {
                self.entry.version = match args.first() {
                    Some(RValue::Sym(s)) => Some(s.clone()),
                    other => other.and_then(RValue::to_text),
                }
            }
            "sha256" => {
                self.entry.sha256 = match args.first() {
                    Some(RValue::Sym(s)) => Some(format!(":{s}")),
                    other => other.and_then(RValue::to_text),
                }
            }
            "url" => {
                if let Some(url) = args.first_str() {
                    self.entry.url_args = vec![Json::String(url)];
                }
                if !args.kwargs.is_empty() {
                    self.entry.url_kwargs = Some(RValue::Hash(args.kwargs.clone()).to_json());
                }
            }
            "name" => {
                if let Some(n) = args.first_str() {
                    self.entry.names.push(n);
                }
            }
            "desc" => self.entry.desc = args.first_str(),
            "homepage" => self.entry.homepage = args.first_str(),
            "auto_updates" => {
                self.entry.auto_updates = args.first().and_then(RValue::as_bool).unwrap_or(true)
            }
            "depends_on" => {
                let merged = merge_object(self.entry.depends_on_args.take(), &args);
                self.entry.depends_on_args = Some(merged);
            }
            "conflicts_with" => {
                let merged = merge_object(self.entry.conflicts_with_args.take(), &args);
                self.entry.conflicts_with_args = Some(merged);
            }
            "container" => {
                self.entry.container_args = Some(RValue::Hash(args.kwargs.clone()).to_json())
            }
            "deprecate!" => {
                self.entry.deprecate_args = Some(RValue::Hash(args.kwargs.clone()).to_json())
            }
            "disable!" => {
                self.entry.disable_args = Some(RValue::Hash(args.kwargs.clone()).to_json())
            }
            "language" => {
                if let Some(l) = args.first_str() {
                    self.entry.languages.push(l);
                }
            }
            "caveats" => {
                if let Some(text) = line.heredocs.first() {
                    self.entry.raw_caveats = Some(Json::String(text.clone()));
                } else if let Some(s) = args.first_str() {
                    self.entry.raw_caveats = Some(Json::String(s));
                }
            }
            _ if HASH_ARTIFACTS.contains(&head) => {
                let value = RValue::Hash(args.kwargs.clone()).to_json();
                self.entry
                    .raw_artifacts
                    .push(Json::Array(vec![Json::String(format!(":{head}")), value]));
            }
            _ if PATH_ARTIFACTS.contains(&head) => {
                let mut items: Vec<Json> = args
                    .positional
                    .iter()
                    .filter_map(RValue::to_text)
                    .map(Json::String)
                    .collect();
                if items.is_empty() {
                    return;
                }
                if !args.kwargs.is_empty() {
                    items.push(RValue::Hash(args.kwargs.clone()).to_json());
                }
                self.entry.raw_artifacts.push(Json::Array(vec![
                    Json::String(format!(":{head}")),
                    Json::Array(items),
                ]));
            }
            _ => {}
        }
    }
}

/// Merge repeated `depends_on`/`conflicts_with` stanzas into one object.
fn merge_object(existing: Option<Json>, args: &Args) -> Json {
    let mut map = match existing {
        Some(Json::Object(m)) => m,
        _ => Map::new(),
    };
    for (key, value) in &args.kwargs {
        map.insert(format!(":{key}"), depends_value(value));
    }
    Json::Object(map)
}

/// `macos: :ventura` -> `":ventura"`, `macos: ">= :ventura"` -> `{">=": [":ventura"]}`.
fn depends_value(v: &RValue) -> Json {
    match v {
        RValue::Str(s) => {
            let t = s.trim();
            for op in [">=", "<=", "==", "!=", ">", "<"] {
                if let Some(rest) = t.strip_prefix(op) {
                    return Json::Object(Map::from_iter([(
                        op.to_string(),
                        Json::Array(vec![Json::String(rest.trim().to_string())]),
                    )]));
                }
            }
            Json::String(s.clone())
        }
        other => other.to_json(),
    }
}
