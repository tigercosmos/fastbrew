//! Generate `homebrew.mxcl.<name>.plist` and `homebrew.<name>.service` from
//! the API `service_args`/`service_run_args` (port of `service.rb#to_plist`
//! and `#to_systemd_unit`). Placeholders `$HOMEBREW_PREFIX`, `$HOMEBREW_CELLAR`
//! and `/$HOME` are replaced first.
//!
//! The XML is written by hand so that it matches the Ruby `plist` gem
//! (`plist-3.7.2/lib/plist/generator.rb`) byte for byte: a tab per nesting
//! level, dictionary keys sorted by their string form, `<true/>`/`<false/>`
//! for booleans and `CGI.escapeHTML` for text.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config::Config;
use crate::error::Result;
use crate::model::{FormulaEntry, sym};

/// Legacy label prefix. Homebrew still accepts (and looks for) this name and
/// every keg installed before Homebrew 5 uses it; see `docs/COMPAT.md` 7.
pub const PLIST_PREFIX: &str = "homebrew.mxcl.";
/// Canonical label prefix used by current Homebrew (`Service#canonical_plist_name`).
pub const CANONICAL_PREFIX: &str = "sh.brew.";
/// Prefix of the generated systemd unit name (`Service#legacy_service_name`).
pub const UNIT_PREFIX: &str = "homebrew.";

const SESSION_TYPES: [&str; 5] = ["Aqua", "Background", "LoginWindow", "StandardIO", "System"];

// ---------------------------------------------------------------------------
// plist values
// ---------------------------------------------------------------------------

/// A property list value, emitted exactly like Ruby's `plist` gem does.
#[derive(Debug, Clone, PartialEq)]
pub enum PlistValue {
    Bool(bool),
    Integer(i64),
    String(String),
    Array(Vec<PlistValue>),
    /// `BTreeMap` reproduces the gem's `sort_by { |k, _| k.to_s }`.
    Dict(BTreeMap<String, PlistValue>),
}

impl PlistValue {
    pub fn string(s: impl Into<String>) -> Self {
        PlistValue::String(s.into())
    }

    /// The complete XML document, including the header and `<plist>` envelope.
    pub fn to_xml(&self) -> String {
        let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        out.push_str(
            "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
             \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
        );
        out.push_str("<plist version=\"1.0\">\n");
        self.emit(0, &mut out);
        out.push_str("</plist>\n");
        out
    }

    fn emit(&self, level: usize, out: &mut String) {
        let pad = "\t".repeat(level);
        match self {
            PlistValue::Bool(b) => {
                out.push_str(&pad);
                out.push_str(if *b { "<true/>\n" } else { "<false/>\n" });
            }
            PlistValue::Integer(i) => {
                out.push_str(&pad);
                out.push_str(&format!("<integer>{i}</integer>\n"));
            }
            PlistValue::String(s) => {
                out.push_str(&pad);
                // An empty string renders as `<string/>` in the gem (`contents.to_s.empty?`).
                if s.is_empty() {
                    out.push_str("<string/>\n");
                } else {
                    out.push_str(&format!("<string>{}</string>\n", escape_html(s)));
                }
            }
            PlistValue::Array(items) => {
                out.push_str(&pad);
                if items.is_empty() {
                    out.push_str("<array/>\n");
                    return;
                }
                out.push_str("<array>\n");
                for item in items {
                    item.emit(level + 1, out);
                }
                out.push_str(&pad);
                out.push_str("</array>\n");
            }
            PlistValue::Dict(map) => {
                out.push_str(&pad);
                if map.is_empty() {
                    out.push_str("<dict/>\n");
                    return;
                }
                out.push_str("<dict>\n");
                for (k, v) in map {
                    out.push_str(&"\t".repeat(level + 1));
                    out.push_str(&format!("<key>{}</key>\n", escape_html(k)));
                    v.emit(level + 1, out);
                }
                out.push_str(&pad);
                out.push_str("</dict>\n");
            }
        }
    }
}

/// `CGI.escapeHTML`: `&`, `"`, `'`, `<`, `>`.
fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// service definition
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunType {
    #[default]
    Immediate,
    Interval,
    Cron,
}

impl RunType {
    fn parse(s: &str) -> RunType {
        match sym(s) {
            "interval" => RunType::Interval,
            "cron" => RunType::Cron,
            _ => RunType::Immediate,
        }
    }

    pub fn is_timed(self) -> bool {
        matches!(self, RunType::Interval | RunType::Cron)
    }
}

/// `Service#parse_cron` result. `None` in a field means `*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cron {
    pub minute: Option<i64>,
    pub hour: Option<i64>,
    pub day: Option<i64>,
    pub month: Option<i64>,
    pub weekday: Option<i64>,
}

impl Cron {
    pub fn is_empty(&self) -> bool {
        self.minute.is_none()
            && self.hour.is_none()
            && self.day.is_none()
            && self.month.is_none()
            && self.weekday.is_none()
    }

    /// The `cron` string as the API stores it (`Minute Hour Day Month Weekday`).
    pub fn to_api_string(self) -> String {
        let f = |v: Option<i64>| v.map(|n| n.to_string()).unwrap_or_else(|| "*".into());
        format!(
            "{} {} {} {} {}",
            f(self.minute),
            f(self.hour),
            f(self.day),
            f(self.month),
            f(self.weekday)
        )
    }
}

/// Port of `Service#parse_cron`. Returns `None` for a statement Homebrew rejects.
pub fn parse_cron(statement: &str) -> Option<Cron> {
    let mut c = Cron::default();
    match statement.trim() {
        "@hourly" => c.minute = Some(0),
        "@daily" => {
            c.minute = Some(0);
            c.hour = Some(0);
        }
        "@weekly" => {
            c.minute = Some(0);
            c.hour = Some(0);
            c.weekday = Some(0);
        }
        "@monthly" => {
            c.minute = Some(0);
            c.hour = Some(0);
            c.day = Some(1);
        }
        "@yearly" | "@annually" => {
            c.minute = Some(0);
            c.hour = Some(0);
            c.day = Some(1);
            c.month = Some(1);
        }
        other => {
            let parts: Vec<&str> = other.split_whitespace().collect();
            if parts.len() != 5 {
                return None;
            }
            let slots: [&mut Option<i64>; 5] = [
                &mut c.minute,
                &mut c.hour,
                &mut c.day,
                &mut c.month,
                &mut c.weekday,
            ];
            for (slot, part) in slots.into_iter().zip(parts) {
                if part == "*" {
                    continue;
                }
                *slot = Some(part.parse().ok()?);
            }
        }
    }
    Some(c)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeepAlive {
    pub always: Option<bool>,
    pub successful_exit: Option<bool>,
    pub crashed: Option<bool>,
    pub path: Option<String>,
}

impl KeepAlive {
    fn is_empty(&self) -> bool {
        self.always.is_none()
            && self.successful_exit.is_none()
            && self.crashed.is_none()
            && self.path.is_none()
    }

    /// `Service#keep_alive?`.
    pub fn enabled(&self) -> bool {
        !self.is_empty() && self.always != Some(false)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Socket {
    pub name: String,
    pub host: String,
    pub port: String,
    pub kind: String,
}

/// Everything a formula's `service` block declares, with placeholders resolved.
#[derive(Debug, Clone)]
pub struct ServiceDef {
    pub name: String,
    /// launchd label (`homebrew.mxcl.<name>` unless the formula overrides it).
    pub plist_name: String,
    /// systemd unit base name (`homebrew.<name>` unless overridden).
    pub service_name: String,
    pub run: Vec<String>,
    pub run_at_load: bool,
    pub run_type: RunType,
    pub interval: Option<i64>,
    pub cron: Option<Cron>,
    pub keep_alive: KeepAlive,
    pub launch_only_once: bool,
    pub require_root: bool,
    /// Insertion order preserved; the plist sorts, the systemd unit does not.
    pub environment_variables: Vec<(String, String)>,
    pub working_dir: Option<String>,
    pub root_dir: Option<String>,
    pub input_path: Option<String>,
    pub log_path: Option<String>,
    pub error_log_path: Option<String>,
    pub restart_delay: Option<i64>,
    pub throttle_interval: Option<i64>,
    pub stop_timeout: Option<i64>,
    pub nice: Option<i64>,
    pub process_type: Option<String>,
    pub macos_legacy_timers: bool,
    pub sockets: Vec<Socket>,
}

impl Default for ServiceDef {
    fn default() -> Self {
        ServiceDef {
            name: String::new(),
            plist_name: String::new(),
            service_name: String::new(),
            run: vec![],
            run_at_load: true,
            run_type: RunType::Immediate,
            interval: None,
            cron: None,
            keep_alive: KeepAlive::default(),
            launch_only_once: false,
            require_root: false,
            environment_variables: vec![],
            working_dir: None,
            root_dir: None,
            input_path: None,
            log_path: None,
            error_log_path: None,
            restart_delay: None,
            throttle_interval: None,
            stop_timeout: None,
            nice: None,
            process_type: None,
            macos_legacy_timers: false,
            sockets: vec![],
        }
    }
}

impl ServiceDef {
    /// Build from an API entry. `None` when the formula declares no run command
    /// (a `service do name macos: "..." end` block documents an existing file).
    pub fn from_formula(cfg: &Config, formula: &FormulaEntry) -> Option<ServiceDef> {
        let mut def = ServiceDef {
            name: formula.name.clone(),
            plist_name: format!("{PLIST_PREFIX}{}", formula.name),
            service_name: format!("{UNIT_PREFIX}{}", formula.name),
            ..Default::default()
        };

        // `service_name_args` is a hash in the payload; the model normalises it
        // to a one-element array (see `model::formula`).
        if let Some(names) = formula.service_name_args.first() {
            if let Some(v) = names.get(":macos").and_then(Value::as_str) {
                def.plist_name = v.to_string();
            }
            if let Some(v) = names.get(":linux").and_then(Value::as_str) {
                def.service_name = v.to_string();
            }
        }

        let run_value: Option<Value> = match &formula.service_run_kwargs {
            // `run macos: [...], linux: [...]` (`Service#on_system_conditional`).
            Some(kw) => {
                let key = if cfg!(target_os = "linux") {
                    ":linux"
                } else {
                    ":macos"
                };
                kw.get(key).or_else(|| kw.get(other_os_key(key))).cloned()
            }
            None => formula.service_run_args.first().cloned(),
        };
        match run_value {
            Some(Value::String(s)) => def.run = vec![cfg.expand_placeholders(&s)],
            Some(Value::Array(a)) => {
                def.run = a
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|s| cfg.expand_placeholders(s))
                    .collect()
            }
            _ => {}
        }

        for pair in &formula.service_args {
            let Some(arr) = pair.as_array() else { continue };
            let (Some(key), Some(value)) = (arr.first().and_then(Value::as_str), arr.get(1)) else {
                continue;
            };
            let as_str = |v: &Value| v.as_str().map(|s| cfg.expand_placeholders(s));
            match key {
                ":run_type" => {
                    if let Some(s) = value.as_str() {
                        def.run_type = RunType::parse(s);
                    }
                }
                ":run_at_load" => def.run_at_load = value.as_bool().unwrap_or(true),
                ":interval" => def.interval = value.as_i64(),
                ":cron" => def.cron = value.as_str().and_then(parse_cron),
                ":keep_alive" => def.keep_alive = parse_keep_alive(value, cfg),
                ":launch_only_once" => def.launch_only_once = value.as_bool().unwrap_or(false),
                ":require_root" => def.require_root = value.as_bool().unwrap_or(false),
                ":environment_variables" => {
                    if let Some(map) = value.as_object() {
                        def.environment_variables = map
                            .iter()
                            .filter_map(|(k, v)| {
                                Some((sym(k).to_string(), cfg.expand_placeholders(v.as_str()?)))
                            })
                            .collect();
                    }
                }
                ":working_dir" => def.working_dir = as_str(value),
                ":root_dir" => def.root_dir = as_str(value),
                ":input_path" => def.input_path = as_str(value),
                ":log_path" => def.log_path = as_str(value),
                ":error_log_path" => def.error_log_path = as_str(value),
                ":restart_delay" => def.restart_delay = value.as_i64(),
                ":throttle_interval" => def.throttle_interval = value.as_i64(),
                ":stop_timeout" => def.stop_timeout = value.as_i64(),
                ":nice" => def.nice = value.as_i64(),
                ":process_type" => def.process_type = value.as_str().map(|s| sym(s).to_string()),
                ":macos_legacy_timers" => {
                    def.macos_legacy_timers = value.as_bool().unwrap_or(false)
                }
                ":sockets" => def.sockets = parse_sockets(value),
                _ => {}
            }
        }

        if def.run.is_empty() {
            return None;
        }
        Some(def)
    }

    /// `Service#command`: only arguments starting with `~` are expanded.
    pub fn command(&self, cfg: &Config) -> Vec<String> {
        self.run
            .iter()
            .map(|a| {
                if a.starts_with('~') {
                    expand_path(cfg, a)
                } else {
                    a.clone()
                }
            })
            .collect()
    }

    pub fn is_timed(&self) -> bool {
        self.run_type.is_timed()
    }

    /// `Service#path_dirs`: directories that must exist before loading.
    pub fn path_dirs(&self, cfg: &Config) -> Vec<PathBuf> {
        let dir = |p: &Option<String>| -> Option<PathBuf> {
            let p = p.as_deref()?;
            if p.is_empty() || !(p.starts_with('/') || p.starts_with('~')) {
                return None;
            }
            Some(PathBuf::from(expand_path(cfg, p)))
        };
        let mut out: Vec<PathBuf> = vec![];
        let push = |out: &mut Vec<PathBuf>, p: Option<PathBuf>| {
            if let Some(p) = p
                && !out.contains(&p)
            {
                out.push(p);
            }
        };
        let working = dir(&self.working_dir);
        let root = dir(&self.root_dir);
        let parents = [&self.input_path, &self.log_path, &self.error_log_path]
            .map(|p| dir(p).and_then(|p| p.parent().map(Path::to_path_buf)));
        push(&mut out, working);
        push(&mut out, root);
        for p in parents {
            push(&mut out, p);
        }
        out
    }

    /// `Service#manual_command`: what to run by hand instead of the service.
    pub fn manual_command(&self, cfg: &Config) -> String {
        let mut parts: Vec<String> = self
            .environment_variables
            .iter()
            .filter(|(k, _)| k != "PATH")
            .map(|(k, v)| format!("{k}=\"{v}\""))
            .collect();
        parts.extend(self.command(cfg).iter().map(|a| sh_quote(a)));
        parts.join(" ")
    }

    /// Port of `Service#to_plist`.
    pub fn to_plist(&self, cfg: &Config) -> String {
        let mut base: BTreeMap<String, PlistValue> = BTreeMap::new();
        base.insert("Label".into(), PlistValue::string(&self.plist_name));
        base.insert(
            "ProgramArguments".into(),
            PlistValue::Array(
                self.command(cfg)
                    .into_iter()
                    .map(PlistValue::String)
                    .collect(),
            ),
        );
        base.insert("RunAtLoad".into(), PlistValue::Bool(self.run_at_load));

        if self.launch_only_once {
            base.insert("LaunchOnlyOnce".into(), PlistValue::Bool(true));
        }
        if self.macos_legacy_timers {
            base.insert("LegacyTimers".into(), PlistValue::Bool(true));
        }
        if let Some(v) = self.stop_timeout {
            base.insert("ExitTimeOut".into(), PlistValue::Integer(v));
        }
        if let Some(v) = self.restart_delay {
            base.insert("TimeOut".into(), PlistValue::Integer(v));
        }
        if let Some(v) = self.throttle_interval {
            base.insert("ThrottleInterval".into(), PlistValue::Integer(v));
        }
        if let Some(p) = self.process_type.as_deref().filter(|s| !s.is_empty()) {
            base.insert("ProcessType".into(), PlistValue::string(capitalize(p)));
        }
        if let Some(v) = self.nice {
            base.insert("Nice".into(), PlistValue::Integer(v));
        }
        if self.run_type == RunType::Interval
            && let Some(v) = self.interval
        {
            base.insert("StartInterval".into(), PlistValue::Integer(v));
        }
        for (key, value) in [
            ("WorkingDirectory", &self.working_dir),
            ("RootDirectory", &self.root_dir),
            ("StandardInPath", &self.input_path),
            ("StandardOutPath", &self.log_path),
            ("StandardErrorPath", &self.error_log_path),
        ] {
            if let Some(v) = value.as_deref().filter(|s| !s.is_empty()) {
                base.insert(key.to_string(), PlistValue::string(expand_path(cfg, v)));
            }
        }

        if !self.environment_variables.is_empty() {
            let env: BTreeMap<String, PlistValue> = self
                .environment_variables
                .iter()
                .map(|(k, v)| (k.clone(), PlistValue::string(v)))
                .collect();
            base.insert("EnvironmentVariables".into(), PlistValue::Dict(env));
        }

        if self.keep_alive.enabled() {
            let ka = &self.keep_alive;
            let value = if ka.always == Some(true) {
                Some(PlistValue::Bool(true))
            } else if let Some(b) = ka.successful_exit {
                Some(PlistValue::Dict(BTreeMap::from([(
                    "SuccessfulExit".to_string(),
                    PlistValue::Bool(b),
                )])))
            } else if let Some(b) = ka.crashed {
                Some(PlistValue::Dict(BTreeMap::from([(
                    "Crashed".to_string(),
                    PlistValue::Bool(b),
                )])))
            } else {
                ka.path.as_deref().filter(|p| !p.is_empty()).map(|p| {
                    PlistValue::Dict(BTreeMap::from([(
                        "PathState".to_string(),
                        PlistValue::string(p),
                    )]))
                })
            };
            if let Some(v) = value {
                base.insert("KeepAlive".into(), v);
            }
        }

        if !self.sockets.is_empty() {
            let mut socks: BTreeMap<String, PlistValue> = BTreeMap::new();
            for s in &self.sockets {
                socks.insert(
                    s.name.clone(),
                    PlistValue::Dict(BTreeMap::from([
                        ("SockNodeName".to_string(), PlistValue::string(&s.host)),
                        ("SockServiceName".to_string(), PlistValue::string(&s.port)),
                        (
                            "SockProtocol".to_string(),
                            PlistValue::string(s.kind.to_uppercase()),
                        ),
                    ])),
                );
            }
            base.insert("Sockets".into(), PlistValue::Dict(socks));
        }

        if self.run_type == RunType::Cron
            && let Some(cron) = self.cron.filter(|c| !c.is_empty())
        {
            let mut dict: BTreeMap<String, PlistValue> = BTreeMap::new();
            for (key, value) in [
                ("Minute", cron.minute),
                ("Hour", cron.hour),
                ("Day", cron.day),
                ("Month", cron.month),
                ("Weekday", cron.weekday),
            ] {
                if let Some(v) = value {
                    dict.insert(key.to_string(), PlistValue::Integer(v));
                }
            }
            base.insert("StartCalendarInterval".into(), PlistValue::Dict(dict));
        }

        base.insert(
            "LimitLoadToSessionType".into(),
            PlistValue::Array(
                SESSION_TYPES
                    .iter()
                    .map(|s| PlistValue::string(*s))
                    .collect(),
            ),
        );

        PlistValue::Dict(base).to_xml()
    }

    /// Port of `Service#to_systemd_unit`.
    pub fn to_systemd_unit(&self, cfg: &Config) -> String {
        let cmd = self
            .command(cfg)
            .iter()
            .map(|a| systemd_quote(a))
            .collect::<Vec<_>>()
            .join(" ");
        let mut options: Vec<String> = vec![];
        options.push(format!(
            "Type={}",
            if self.launch_only_once {
                "oneshot"
            } else {
                "simple"
            }
        ));
        options.push(format!("ExecStart={cmd}"));
        let ka = &self.keep_alive;
        if !ka.is_empty() {
            if ka.always == Some(true) || ka.crashed == Some(true) {
                options.push("Restart=on-failure".into());
            } else if ka.successful_exit == Some(true) {
                options.push("Restart=on-success".into());
            }
        }
        if let Some(v) = self.restart_delay {
            options.push(format!("RestartSec={v}"));
        }
        if let Some(v) = self.stop_timeout {
            options.push(format!("TimeoutStopSec={v}"));
        }
        if let Some(v) = self.nice {
            options.push(format!("Nice={v}"));
        }
        for (key, value) in [
            ("WorkingDirectory=", &self.working_dir),
            ("RootDirectory=", &self.root_dir),
            ("StandardInput=file:", &self.input_path),
            ("StandardOutput=append:", &self.log_path),
            ("StandardError=append:", &self.error_log_path),
        ] {
            if let Some(v) = value.as_deref().filter(|s| !s.is_empty()) {
                options.push(format!("{key}{}", expand_path(cfg, v)));
            }
        }
        for (k, v) in &self.environment_variables {
            options.push(format!("Environment=\"{k}={v}\""));
        }

        format!(
            "[Unit]\nDescription=Homebrew generated unit for {}\n\n\
             [Install]\nWantedBy=default.target\n\n\
             [Service]\n{}\n",
            self.name,
            options.join("\n")
        )
    }

    /// Port of `Service#to_systemd_timer`.
    pub fn to_systemd_timer(&self) -> String {
        let mut options: Vec<String> = vec![];
        if self.run_type == RunType::Cron {
            options.push("Persistent=true".into());
        }
        if self.run_type == RunType::Interval
            && let Some(v) = self.interval
        {
            options.push(format!("OnUnitActiveSec={v}"));
        }
        if self.run_type == RunType::Cron {
            let cron = self.cron.unwrap_or_default();
            let two = |v: Option<i64>| {
                v.map(|n| format!("{n:02}"))
                    .unwrap_or_else(|| "*".to_string())
            };
            let star = |v: Option<i64>| v.map(|n| n.to_string()).unwrap_or_else(|| "*".to_string());
            options.push(format!(
                "OnCalendar={}*-{}-{} {}:{}:00",
                weekday_prefix(cron.weekday),
                star(cron.month),
                star(cron.day),
                two(cron.hour),
                two(cron.minute)
            ));
        }
        format!(
            "[Unit]\nDescription=Homebrew generated timer for {}\n\n\
             [Install]\nWantedBy=timers.target\n\n\
             [Timer]\nUnit={}.service\n{}\n",
            self.name,
            self.service_name,
            options.join("\n")
        )
    }
}

fn other_os_key(key: &str) -> &'static str {
    if key == ":linux" { ":macos" } else { ":linux" }
}

fn weekday_prefix(weekday: Option<i64>) -> String {
    const ABBR: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    match weekday {
        Some(n) if (0..=7).contains(&n) => format!("{} ", ABBR[(n % 7) as usize]),
        _ => String::new(),
    }
}

fn parse_keep_alive(value: &Value, cfg: &Config) -> KeepAlive {
    let mut ka = KeepAlive::default();
    match value {
        Value::Bool(b) => ka.always = Some(*b),
        Value::Object(map) => {
            for (k, v) in map {
                match sym(k) {
                    "always" => ka.always = v.as_bool(),
                    "successful_exit" => ka.successful_exit = v.as_bool(),
                    "crashed" => ka.crashed = v.as_bool(),
                    "path" => ka.path = v.as_str().map(|s| cfg.expand_placeholders(s)),
                    _ => {}
                }
            }
        }
        _ => {}
    }
    ka
}

fn parse_sockets(value: &Value) -> Vec<Socket> {
    let pairs: Vec<(String, String)> = match value {
        Value::String(s) => vec![("listeners".to_string(), s.clone())],
        Value::Object(map) => map
            .iter()
            .filter_map(|(k, v)| Some((sym(k).to_string(), v.as_str()?.to_string())))
            .collect(),
        _ => vec![],
    };
    pairs
        .into_iter()
        .filter_map(|(name, spec)| {
            // `SOCKET_STRING_REGEX`: `<type>://<host>:<port>`.
            let (kind, rest) = spec.split_once("://")?;
            let (host, port) = rest.rsplit_once(':')?;
            if kind.is_empty()
                || host.is_empty()
                || port.is_empty()
                || !port.bytes().all(|b| b.is_ascii_digit())
                || !kind.bytes().all(|b| b.is_ascii_alphabetic())
            {
                return None;
            }
            Some(Socket {
                name,
                host: host.to_string(),
                port: port.to_string(),
                kind: kind.to_string(),
            })
        })
        .collect()
}

fn capitalize(s: &str) -> String {
    let mut cs = s.chars();
    match cs.next() {
        Some(c) => c.to_uppercase().collect::<String>() + &cs.as_str().to_lowercase(),
        None => String::new(),
    }
}

/// `File.expand_path`: `~` expansion, absolutisation and lexical `.`/`..` cleanup.
pub fn expand_path(cfg: &Config, path: &str) -> String {
    let mut p = path.to_string();
    if p == "~" {
        p = cfg.home.to_string_lossy().into_owned();
    } else if let Some(rest) = p.strip_prefix("~/") {
        p = format!("{}/{rest}", cfg.home.to_string_lossy());
    }
    if !p.starts_with('/') {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        p = format!("{}/{p}", cwd.to_string_lossy());
    }
    let mut parts: Vec<&str> = vec![];
    for c in p.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    format!("/{}", parts.join("/"))
}

/// `Utils::Service.systemd_quote`.
pub fn systemd_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\u{7}' => out.push_str("\\a"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{b}' => out.push_str("\\v"),
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `Utils::Shell.sh_quote`: leave safe words alone, single-quote the rest.
fn sh_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.,:/@\n+=".contains(&b))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

// ---------------------------------------------------------------------------
// public API
// ---------------------------------------------------------------------------

/// launchd label, default `homebrew.mxcl.<name>`.
pub fn plist_label(formula: &FormulaEntry) -> String {
    formula
        .service_name_args
        .first()
        .and_then(|v| v.get(":macos"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("{PLIST_PREFIX}{}", formula.name))
}

/// Every label Homebrew looks for, in priority order (`Formula#plist_names`).
pub fn plist_labels(formula: &FormulaEntry) -> Vec<String> {
    let mut out = vec![plist_label(formula)];
    for candidate in [
        format!("{CANONICAL_PREFIX}{}", formula.name),
        format!("{PLIST_PREFIX}{}", formula.name),
    ] {
        if !out.contains(&candidate) {
            out.push(candidate);
        }
    }
    out
}

/// systemd unit base name, default `homebrew.<name>`.
pub fn unit_name(formula: &FormulaEntry) -> String {
    formula
        .service_name_args
        .first()
        .and_then(|v| v.get(":linux"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("{UNIT_PREFIX}{}", formula.name))
}

/// XML plist text, or `None` when the formula has no runnable service.
pub fn to_plist(cfg: &Config, formula: &FormulaEntry) -> Result<Option<String>> {
    Ok(ServiceDef::from_formula(cfg, formula).map(|d| d.to_plist(cfg)))
}

pub fn to_systemd_unit(cfg: &Config, formula: &FormulaEntry) -> Result<Option<String>> {
    Ok(ServiceDef::from_formula(cfg, formula).map(|d| d.to_systemd_unit(cfg)))
}

/// Write both files into the keg (called at install time).
///
/// Port of `FormulaInstaller#install_service`: the systemd unit (and timer for
/// timed services) first, then the plist, both mode 0644; `<prefix>/var/log`
/// is created when the plist mentions it.
pub fn install_service_files(
    cfg: &Config,
    formula: &FormulaEntry,
    keg_path: &std::path::Path,
) -> Result<()> {
    let Some(def) = ServiceDef::from_formula(cfg, formula) else {
        return Ok(());
    };

    let unit_path = keg_path.join(format!("{}.service", def.service_name));
    write_0644(&unit_path, def.to_systemd_unit(cfg).as_bytes())?;
    if def.is_timed() {
        let timer_path = keg_path.join(format!("{}.timer", def.service_name));
        write_0644(&timer_path, def.to_systemd_timer().as_bytes())?;
    }

    let plist = def.to_plist(cfg);
    let plist_path = keg_path.join(format!("{}.plist", def.plist_name));
    write_0644(&plist_path, plist.as_bytes())?;

    let log = cfg.prefix.join("var/log");
    if plist.contains(&*log.to_string_lossy()) {
        std::fs::create_dir_all(&log)?;
    }
    Ok(())
}

fn write_0644(path: &std::path::Path, data: &[u8]) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    crate::keg::atomic_write(path, data)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::test_support::config_with_prefix;
    use std::collections::BTreeMap as Map;

    fn test_config() -> Config {
        config_with_prefix("/opt/homebrew", "/Users/fastbrew")
    }

    #[derive(Clone, serde::Deserialize)]
    struct Golden {
        #[serde(flatten)]
        entry: FormulaEntry,
        expected_plist: String,
        expected_systemd: String,
        expected_label: String,
    }

    fn goldens() -> Map<String, Golden> {
        serde_json::from_str(include_str!("testdata/service_golden.json")).expect("golden fixtures")
    }

    #[test]
    fn matches_homebrew_plists_and_units() {
        let cfg = test_config();
        for (name, golden) in goldens() {
            let mut entry = golden.entry.clone();
            entry.name = name.clone();
            let def = ServiceDef::from_formula(&cfg, &entry)
                .unwrap_or_else(|| panic!("{name} has a runnable service"));
            assert_eq!(def.plist_name, golden.expected_label, "label for {name}");
            pretty_assertions::assert_eq!(
                def.to_plist(&cfg),
                golden.expected_plist,
                "plist for {}",
                name
            );
            pretty_assertions::assert_eq!(
                def.to_systemd_unit(&cfg),
                golden.expected_systemd,
                "systemd unit for {}",
                name
            );
        }
    }

    #[test]
    fn cron_parsing() {
        assert_eq!(
            parse_cron("@daily").unwrap(),
            Cron {
                minute: Some(0),
                hour: Some(0),
                ..Default::default()
            }
        );
        assert_eq!(
            parse_cron("@weekly").unwrap(),
            Cron {
                minute: Some(0),
                hour: Some(0),
                weekday: Some(0),
                ..Default::default()
            }
        );
        assert_eq!(
            parse_cron("@monthly").unwrap(),
            Cron {
                minute: Some(0),
                hour: Some(0),
                day: Some(1),
                ..Default::default()
            }
        );
        assert_eq!(
            parse_cron("@yearly").unwrap(),
            parse_cron("@annually").unwrap()
        );
        assert_eq!(parse_cron("@hourly").unwrap().minute, Some(0));
        let c = parse_cron("25 6 * * *").unwrap();
        assert_eq!((c.minute, c.hour), (Some(25), Some(6)));
        assert_eq!((c.day, c.month, c.weekday), (None, None, None));
        assert_eq!(parse_cron("0 4 * *"), None);
        assert_eq!(parse_cron("x 4 * * *"), None);
        assert_eq!(
            parse_cron("0 4 * * *").unwrap().to_api_string(),
            "0 4 * * *"
        );
    }

    #[test]
    fn plist_escaping_and_shapes() {
        let v = PlistValue::Dict(BTreeMap::from([
            ("b".into(), PlistValue::String("x & <y>\"'".into())),
            ("a".into(), PlistValue::Array(vec![])),
            ("c".into(), PlistValue::Integer(-3)),
            ("d".into(), PlistValue::Bool(false)),
        ]));
        let xml = v.to_xml();
        assert!(xml.contains("<key>a</key>\n\t<array/>\n"), "{xml}");
        assert!(
            xml.contains("<string>x &amp; &lt;y&gt;&quot;&#39;</string>"),
            "{xml}"
        );
        assert!(xml.contains("<integer>-3</integer>"));
        assert!(xml.contains("<false/>"));
        let keys: Vec<_> = xml.match_indices("<key>").map(|(i, _)| i).collect();
        assert_eq!(keys.len(), 4);
    }

    #[test]
    fn expands_paths_and_quotes() {
        let cfg = test_config();
        assert_eq!(expand_path(&cfg, "~/x"), "/Users/fastbrew/x");
        assert_eq!(expand_path(&cfg, "/a/b/../c//d/"), "/a/c/d");
        assert_eq!(systemd_quote("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(sh_quote("daemon off;"), "'daemon off;'");
        assert_eq!(sh_quote("/opt/x"), "/opt/x");
    }

    #[test]
    fn timer_for_cron_service() {
        let cfg = test_config();
        let goldens = goldens();
        let mut entry = goldens["logrotate"].entry.clone();
        entry.name = "logrotate".into();
        let def = ServiceDef::from_formula(&cfg, &entry).unwrap();
        assert!(def.is_timed());
        let timer = def.to_systemd_timer();
        assert!(timer.contains("Unit=homebrew.logrotate.service"), "{timer}");
        assert!(timer.contains("OnCalendar=*-*-* 06:25:00"), "{timer}");
        assert!(timer.contains("Persistent=true"));
    }
}
