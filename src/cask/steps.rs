//! Declarative cask install steps (`preflight_steps`, `postflight_steps`,
//! `uninstall_preflight_steps`, `uninstall_postflight_steps`).
//!
//! Port of the cask-relevant subset of `Library/Homebrew/install_steps.rb`
//! (`Homebrew::InstallSteps::Runner`). Steps arrive from the API with Ruby
//! symbol keys (`":type"`, `":path"`); [`normalise`] strips the colons so the
//! executor sees the same string keys the Ruby runner does.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Map, Value};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::output;

use super::config::CaskDirs;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Install,
    Uninstall,
}

/// Everything a step can resolve a path or template token against.
pub struct StepContext<'a> {
    pub cfg: &'a Config,
    pub dirs: &'a CaskDirs,
    pub token: String,
    pub name: String,
    pub version: String,
    pub arch: String,
    pub staged_path: PathBuf,
    pub caskroom_path: PathBuf,
    pub verbose: bool,
}

/// One normalised step: string keys, no leading colons.
pub type Step = Map<String, Value>;

/// Strip Ruby symbol colons from every hash key, recursively.
pub fn normalise(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (crate::model::sym(k).to_string(), normalise(v)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(normalise).collect()),
        other => other.clone(),
    }
}

/// Normalise the `{":steps": [..]}` argument of a `*_steps` artifact.
pub fn steps_from_args(args: &[Value]) -> Vec<Step> {
    args.iter()
        .filter_map(|arg| {
            let normalised = normalise(arg);
            normalised
                .get("steps")
                .and_then(Value::as_array)
                .map(|a| a.to_vec())
        })
        .flatten()
        .filter_map(|s| s.as_object().cloned())
        .collect()
}

impl StepContext<'_> {
    /// `Runner#template_token_value`.
    fn token_value(&self, token: &str) -> Option<String> {
        let path = |p: &Path| Some(p.to_string_lossy().into_owned());
        match token {
            "HOMEBREW_PREFIX" => path(&self.cfg.prefix),
            "HOMEBREW_CELLAR" => path(&self.cfg.cellar),
            "HOMEBREW_BREW_FILE" => path(&self.cfg.prefix.join("bin/brew")),
            "name" | "formula_name" => Some(self.name.clone()),
            "token" => Some(self.token.clone()),
            "arch" => Some(self.arch.clone()),
            "user" => std::env::var("USER").ok(),
            "version" => Some(self.version.clone()),
            "version.major" => Some(version_major(&self.version)),
            "version.major_minor" => Some(version_major_minor(&self.version)),
            "staged_path" => path(&self.staged_path),
            "caskroom_path" => path(&self.caskroom_path),
            "temp" => path(&self.cfg.temp),
            "appdir" => path(&self.dirs.appdir),
            "bash_completion" => path(&self.dirs.bash_completion),
            "zsh_completion" => path(&self.dirs.zsh_completion),
            "fish_completion" => path(&self.dirs.fish_completion),
            "pwsh_completion" => path(&self.cfg.prefix.join("share/pwsh/completions")),
            _ => None,
        }
    }

    /// `Runner#expand_template_tokens`: `{{token}}`, leaving unknown tokens alone.
    pub fn expand_tokens(&self, content: &str) -> String {
        let mut out = String::with_capacity(content.len());
        let bytes = content.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'{'
                && i + 1 < bytes.len()
                && bytes[i + 1] == b'{'
                && let Some(end) = content[i + 2..].find("}}")
            {
                let token = &content[i + 2..i + 2 + end];
                let valid = !token.is_empty()
                    && token.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                    && token
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.');
                if valid {
                    match self.token_value(token) {
                        Some(value) => out.push_str(&value),
                        None => out.push_str(&content[i..i + 4 + end]),
                    }
                    i += 4 + end;
                    continue;
                }
            }
            out.push(content[i..].chars().next().unwrap_or('{'));
            i += content[i..].chars().next().map(char::len_utf8).unwrap_or(1);
        }
        out
    }

    /// `Runner#root_path`: the directory a `base:` value names.
    fn root_path(&self, base: &str) -> Result<PathBuf> {
        match base {
            "home" => Ok(self.cfg.home.clone()),
            "temp" => Ok(self.cfg.temp.clone()),
            "homebrew_prefix" => Ok(self.cfg.prefix.clone()),
            "staged_path" => Ok(self.staged_path.clone()),
            "caskroom_path" => Ok(self.caskroom_path.clone()),
            other => self
                .dirs
                .base_for(other)
                .map(Path::to_path_buf)
                .ok_or_else(|| Error::user(format!("unknown install step base: {other}"))),
        }
    }

    /// `Runner#resolve_path`.
    pub fn resolve_path(&self, spec: &Value) -> Result<PathBuf> {
        let (raw, base) = path_spec(spec)?;
        let expanded = self.expand_tokens(&raw);
        match base.as_deref() {
            None | Some("") | Some("absolute") => Ok(expand_user(self.cfg, &expanded)),
            Some("relative") => Ok(PathBuf::from(expanded)),
            Some(base) => Ok(self.root_path(base)?.join(expanded)),
        }
    }

    /// `Runner#link_source`: a `relative` base keeps the literal link target.
    fn link_source(&self, spec: &Value) -> Result<String> {
        let (raw, base) = path_spec(spec)?;
        if base.as_deref() == Some("relative") {
            return Ok(self.expand_tokens(&raw));
        }
        Ok(self.resolve_path(spec)?.to_string_lossy().into_owned())
    }

    /// `Runner#expand_path_glob`.
    pub fn expand_glob(&self, spec: &Value) -> Result<Vec<PathBuf>> {
        let (raw, base) = path_spec(spec)?;
        if matches!(base.as_deref(), Some("search_path") | Some("path")) {
            let path = self.expand_tokens(&raw);
            let mut out = Vec::new();
            for dir in std::env::var("PATH").unwrap_or_default().split(':') {
                let candidate = Path::new(dir).join(&path);
                out.extend(glob_or_literal(&candidate));
            }
            return Ok(out);
        }
        Ok(glob_or_literal(&self.resolve_path(spec)?))
    }

    fn path_spec_exists(&self, spec: &Value) -> bool {
        self.expand_glob(spec)
            .map(|paths| paths.iter().any(|p| p.exists() || p.is_symlink()))
            .unwrap_or(false)
    }
}

fn glob_or_literal(path: &Path) -> Vec<PathBuf> {
    let text = path.to_string_lossy();
    if !text.contains(['?', '*', '[', '{']) {
        return vec![path.to_path_buf()];
    }
    glob::glob(&text)
        .map(|paths| paths.flatten().collect())
        .unwrap_or_else(|_| vec![path.to_path_buf()])
}

/// `DSL.normalise_path_value`: a string, or `{path:, base:}`.
fn path_spec(spec: &Value) -> Result<(String, Option<String>)> {
    match spec {
        Value::String(s) => Ok((s.clone(), None)),
        Value::Object(map) => {
            let path = map
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::user("install step path spec without a path".to_string()))?;
            Ok((
                path.to_string(),
                map.get("base").and_then(Value::as_str).map(str::to_string),
            ))
        }
        _ => Err(Error::user(
            "install step path spec must be a string or object".to_string(),
        )),
    }
}

/// `Pathname#expand_path` on an already token-expanded string.
fn expand_user(cfg: &Config, value: &str) -> PathBuf {
    super::config::expand_path(cfg, value)
}

/// Leading numeric token of a version (`Version#major`).
pub fn version_major(version: &str) -> String {
    version
        .split(|c: char| !c.is_ascii_digit())
        .find(|s| !s.is_empty())
        .unwrap_or("")
        .to_string()
}

/// `major.minor` (`Version#major_minor`).
pub fn version_major_minor(version: &str) -> String {
    let mut parts = version.split('.');
    let major = parts
        .next()
        .map(|p| {
            p.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
        })
        .unwrap_or_default();
    match parts.next() {
        Some(minor) => {
            let minor: String = minor.chars().take_while(char::is_ascii_digit).collect();
            if minor.is_empty() {
                major
            } else {
                format!("{major}.{minor}")
            }
        }
        None => major,
    }
}

/// Run every step of a `*_steps` artifact for `phase`.
pub fn run(ctx: &StepContext<'_>, steps: &[Step], phase: Phase) -> Result<()> {
    let mut guard_cache: BTreeMap<String, bool> = BTreeMap::new();
    for step in steps {
        if phase != Phase::Uninstall && !guards_match(ctx, step, &mut guard_cache) {
            continue;
        }
        match phase {
            Phase::Install => run_install_step(ctx, step)?,
            Phase::Uninstall => run_uninstall_step(ctx, step)?,
        }
    }
    Ok(())
}

fn guards_match(ctx: &StepContext<'_>, step: &Step, cache: &mut BTreeMap<String, bool>) -> bool {
    let Some(guards) = step.get("guards").and_then(Value::as_array) else {
        return true;
    };
    guards.iter().all(|guard| {
        let key = guard.to_string();
        if let Some(cached) = cache.get(&key) {
            return *cached;
        }
        let condition = guard.get("condition").and_then(Value::as_str).unwrap_or("");
        let matches = match condition {
            "if_exists" => ctx.path_spec_exists(guard),
            "unless_exists" => !ctx.path_spec_exists(guard),
            "on" => match guard.get("value").and_then(Value::as_str) {
                Some("macos") => cfg!(target_os = "macos"),
                Some("linux") => cfg!(target_os = "linux"),
                _ => false,
            },
            _ => false,
        };
        cache.insert(key, matches);
        matches
    })
}

fn field<'a>(step: &'a Step, key: &str) -> Result<&'a Value> {
    step.get(key)
        .ok_or_else(|| Error::user(format!("install step is missing '{key}'")))
}

fn flag(step: &Step, key: &str) -> bool {
    step.get(key) == Some(&Value::Bool(true))
}

fn needs_sudo(step: &Step, target_dir: &Path) -> bool {
    match step.get("sudo") {
        Some(Value::Bool(true)) => true,
        Some(Value::String(s)) if s == "if_needed" => !is_writable(target_dir),
        _ => false,
    }
}

fn is_writable(path: &Path) -> bool {
    // `Pathname#writable?` via `access(2)`.
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    // SAFETY: `c` is a valid NUL-terminated path.
    unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
}

fn run_install_step(ctx: &StepContext<'_>, step: &Step) -> Result<()> {
    let kind = field(step, "type")?.as_str().unwrap_or_default();
    match kind {
        "mkdir" => {
            std::fs::create_dir(ctx.resolve_path(field(step, "path")?)?)?;
        }
        "mkdir_p" => {
            std::fs::create_dir_all(ctx.resolve_path(field(step, "path")?)?)?;
        }
        "touch" => {
            let path = ctx.resolve_path(field(step, "path")?)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if !path.exists() {
                std::fs::File::create(&path)?;
            } else {
                filetime::set_file_mtime(&path, filetime::FileTime::now())?;
            }
        }
        "move" => {
            let source = resolve_source(ctx, step)?;
            let target = ctx.resolve_path(field(step, "target")?)?;
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let destination = destination(&source, &target);
            let overwrite =
                !step.contains_key("overwrite") || flag(step, "overwrite") || flag(step, "force");
            if destination.exists() && !overwrite {
                return Err(Error::user(format!(
                    "File exists - {}",
                    destination.display()
                )));
            }
            super::unpack::move_path(&source, &destination)?;
        }
        "move_children" | "move_contents" => {
            let source = ctx.resolve_path(field(step, "source")?)?;
            let target = ctx.resolve_path(field(step, "target")?)?;
            std::fs::create_dir_all(&target)?;
            let children: Vec<PathBuf> = std::fs::read_dir(&source)?
                .flatten()
                .map(|e| e.path())
                .filter(|p| *p != target)
                .collect();
            for child in children {
                let dst = target.join(child.file_name().unwrap_or_default());
                super::unpack::move_path(&child, &dst)?;
            }
        }
        "copy" => {
            let source = resolve_source(ctx, step)?;
            let target = ctx.resolve_path(field(step, "target")?)?;
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let destination = destination(&source, &target);
            let overwrite = step.get("overwrite") != Some(&Value::Bool(false));
            if destination.exists() && !overwrite {
                return Err(Error::user(format!(
                    "File exists - {}",
                    destination.display()
                )));
            }
            // `copy source/. target` copies the contents, like `FileUtils.cp_r`.
            let source = if source.file_name().map(|n| n == ".").unwrap_or(false) {
                source.parent().unwrap_or(&source).to_path_buf()
            } else {
                source
            };
            if source.is_dir() && destination.exists() && overwrite {
                super::unpack::remove_path(&destination)?;
            }
            super::unpack::copy_path(&source, &destination)?;
        }
        "remove" => {
            let mut paths: Vec<PathBuf> = Vec::new();
            for spec in field(step, "paths")?.as_array().unwrap_or(&vec![]) {
                paths.extend(ctx.expand_glob(spec)?);
            }
            if let Some(needle) = step.get("symlink_target_contains").and_then(Value::as_str) {
                paths.retain(|p| {
                    p.is_symlink()
                        && std::fs::read_link(p)
                            .map(|t| t.to_string_lossy().contains(needle))
                            .unwrap_or(false)
                });
            }
            if let Some(needle) = step.get("content_contains").and_then(Value::as_str) {
                paths.retain(|p| {
                    std::fs::read_to_string(p)
                        .map(|t| t.contains(needle))
                        .unwrap_or(false)
                });
            }
            for path in paths {
                let parent = path.parent().unwrap_or(Path::new("/")).to_path_buf();
                if needs_sudo(step, &parent) {
                    sudo_remove(&path)?;
                } else if flag(step, "recursive") {
                    super::unpack::remove_path(&path)?;
                } else {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        "symlink" => {
            let target = ctx.resolve_path(field(step, "target")?)?;
            if flag(step, "source_glob") {
                let sources = ctx.expand_glob(field(step, "source")?)?;
                if sources.is_empty() {
                    return Ok(());
                }
                if sources.len() > 1 || target.is_dir() {
                    std::fs::create_dir_all(&target)?;
                    for source in sources {
                        let link = target.join(source.file_name().unwrap_or_default());
                        create_symlink(&source.to_string_lossy(), &link, step)?;
                    }
                } else {
                    create_symlink(&sources[0].to_string_lossy(), &target, step)?;
                }
            } else {
                let source = ctx.link_source(field(step, "source")?)?;
                create_symlink(&source, &target, step)?;
            }
        }
        "write" => {
            let content = step
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::user("install step write requires content".to_string()))?;
            let path = ctx.resolve_path(field(step, "path")?)?;
            if flag(step, "overwrite") || !path.exists() {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                crate::keg::atomic_write(&path, ctx.expand_tokens(content).as_bytes())?;
            }
        }
        "inreplace" => {
            let path = ctx.resolve_path(field(step, "path")?)?;
            let before = ctx.expand_tokens(field(step, "before")?.as_str().unwrap_or_default());
            let after = ctx.expand_tokens(field(step, "after")?.as_str().unwrap_or_default());
            let text = std::fs::read_to_string(&path)?;
            let replaced = if flag(step, "first_only") {
                text.replacen(&before, &after, 1)
            } else {
                text.replace(&before, &after)
            };
            crate::keg::atomic_write(&path, replaced.as_bytes())?;
        }
        "run" => run_command_step(ctx, step)?,
        "terminate_process" => terminate_process(ctx, step)?,
        "set_permissions" => {
            let paths = existing_paths(ctx, step)?;
            if paths.is_empty() {
                return Ok(());
            }
            let mut args: Vec<String> = Vec::new();
            if !flag(step, "non_recursive") {
                args.push("-R".to_string());
            }
            args.push("--".to_string());
            args.push(
                field(step, "permissions")?
                    .as_str()
                    .unwrap_or("u+w")
                    .to_string(),
            );
            run_tool("chmod", &args, &paths, false)?;
        }
        "set_ownership" => {
            let paths = existing_paths(ctx, step)?;
            if paths.is_empty() {
                return Ok(());
            }
            output::ohai(&format!(
                "Changing ownership of paths required by {} with `sudo` (which may request your password)...",
                ctx.token
            ));
            let user = step
                .get("user")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(current_user);
            let group = step.get("group").and_then(Value::as_str).unwrap_or("staff");
            let mut args: Vec<String> = Vec::new();
            if !flag(step, "non_recursive") {
                args.push("-R".to_string());
            }
            args.push("--".to_string());
            args.push(format!("{user}:{group}"));
            run_tool("chown", &args, &paths, true)?;
        }
        "warn" => {
            output::opoo(&ctx.expand_tokens(field(step, "message")?.as_str().unwrap_or_default()));
        }
        "delete_keychain_certificate" => delete_keychain_certificate(ctx, step)?,
        other => {
            return Err(Error::user(format!(
                "Unknown cask install step '{other}'. Run `brew {} <cask>` instead.",
                if matches!(other, "" | "run") {
                    "reinstall --cask"
                } else {
                    "install --cask"
                }
            )));
        }
    }
    Ok(())
}

/// `Runner#run_uninstall_step`: only `symlink` steps flagged `uninstall: true`.
fn run_uninstall_step(ctx: &StepContext<'_>, step: &Step) -> Result<()> {
    if field(step, "type")?.as_str() != Some("symlink") || !flag(step, "uninstall") {
        return Ok(());
    }
    let target = ctx.resolve_path(field(step, "target")?)?;
    if !target.is_symlink() {
        return Ok(());
    }
    let source = ctx.link_source(field(step, "source")?)?;
    let links_to_source = std::fs::read_link(&target)
        .map(|t| t.as_os_str() == std::ffi::OsStr::new(&source))
        .unwrap_or(false);
    if !links_to_source {
        return Ok(());
    }
    let parent = target.parent().unwrap_or(Path::new("/")).to_path_buf();
    if needs_sudo(step, &parent) {
        sudo_remove(&target)?;
    } else {
        let _ = std::fs::remove_file(&target);
    }
    Ok(())
}

fn resolve_source(ctx: &StepContext<'_>, step: &Step) -> Result<PathBuf> {
    let spec = field(step, "source")?;
    let source = ctx.resolve_path(spec)?;
    if !flag(step, "source_glob") {
        return Ok(source);
    }
    let mut matches: Vec<PathBuf> = ctx
        .expand_glob(spec)?
        .into_iter()
        .filter(|p| p.exists() || p.is_symlink())
        .collect();
    matches.dedup();
    if matches.len() != 1 {
        return Err(Error::user(format!(
            "install step source glob must match exactly one path: {}",
            source.display()
        )));
    }
    Ok(matches.remove(0))
}

/// `Runner#step_destination`: a directory target keeps the source basename.
fn destination(source: &Path, target: &Path) -> PathBuf {
    if target.is_dir() {
        target.join(source.file_name().unwrap_or_default())
    } else {
        target.to_path_buf()
    }
}

fn existing_paths(ctx: &StepContext<'_>, step: &Step) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for spec in field(step, "paths")?.as_array().unwrap_or(&vec![]) {
        out.extend(
            ctx.expand_glob(spec)?
                .into_iter()
                .filter(|p| p.exists() || p.is_symlink()),
        );
    }
    Ok(out)
}

fn create_symlink(source: &str, target: &Path, step: &Step) -> Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let parent = target.parent().unwrap_or(Path::new("/")).to_path_buf();
    if needs_sudo(step, &parent) {
        let mut args = vec!["-s".to_string()];
        if flag(step, "force") {
            args.push("-f".to_string());
        }
        args.push(source.to_string());
        args.push(target.to_string_lossy().into_owned());
        return run_tool("/bin/ln", &args, &[], true);
    }
    if flag(step, "force") {
        let _ = std::fs::remove_file(target);
    }
    std::os::unix::fs::symlink(source, target)?;
    Ok(())
}

/// The `run` step: `Runner#run_serialised_command`.
fn run_command_step(ctx: &StepContext<'_>, step: &Step) -> Result<()> {
    let spec = field(step, "command")?;
    let (raw, base) = path_spec(spec)?;
    let program = match base.as_deref() {
        None | Some("") | Some("relative") => ctx.expand_tokens(&raw),
        _ => ctx.resolve_path(spec)?.to_string_lossy().into_owned(),
    };

    let args: Vec<String> = step
        .get("args")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(|s| ctx.expand_tokens(s))
                .collect()
        })
        .unwrap_or_default();

    let sudo = step.get("sudo") == Some(&Value::Bool(true));
    let mut cmd = if sudo {
        let mut c = Command::new("/usr/bin/sudo");
        c.arg("-E").arg("--").arg(&program);
        c
    } else {
        Command::new(&program)
    };
    cmd.args(&args);

    if let Some(env) = step.get("env").and_then(Value::as_object) {
        for (key, value) in env {
            cmd.env(key, ctx.expand_tokens(value.as_str().unwrap_or_default()));
        }
    }
    if let Some(chdir) = step.get("chdir") {
        cmd.current_dir(ctx.resolve_path(chdir)?);
    }
    if let Some(stdin_path) = step.get("stdin_path") {
        let path = ctx.resolve_path(stdin_path)?;
        cmd.stdin(Stdio::from(std::fs::File::open(path)?));
    } else {
        cmd.stdin(Stdio::null());
    }

    let capture_stdout = step.contains_key("stdout_path");
    if capture_stdout || !flag(step, "print_stdout") {
        cmd.stdout(Stdio::piped());
    }
    if step.get("suppress_stderr") != Some(&Value::Bool(false)) && !ctx.verbose {
        cmd.stderr(Stdio::null());
    }

    let output = cmd.output()?;
    if !output.status.success() && !flag(step, "allow_failure") {
        return Err(Error::user(format!(
            "Failure while executing; `{program}` exited with {}.",
            output.status.code().unwrap_or(-1)
        )));
    }
    if capture_stdout && output.status.success() {
        let path = ctx.resolve_path(field(step, "stdout_path")?)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, &output.stdout)?;
    }
    Ok(())
}

fn terminate_process(ctx: &StepContext<'_>, step: &Step) -> Result<()> {
    if let Some(notices) = step.get("notices").and_then(Value::as_array) {
        for notice in notices.iter().filter_map(Value::as_str) {
            output::ohai(&ctx.expand_tokens(notice));
        }
    }
    let name = ctx.expand_tokens(field(step, "name")?.as_str().unwrap_or_default());
    let (program, args) = if step.get("match").and_then(Value::as_str) == Some("full") {
        ("/usr/bin/pkill", vec!["-f".to_string(), name])
    } else {
        ("/usr/bin/killall", vec![name])
    };
    let sudo = step.get("sudo") == Some(&Value::Bool(true));
    let attempts = step
        .get("attempts")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .max(1);

    for attempt in 0..attempts {
        if run_tool(program, &args, &[], sudo).is_ok() {
            return Ok(());
        }
        if attempt + 1 < attempts {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
    if let Some(message) = step.get("failure_message").and_then(Value::as_str) {
        output::opoo(&ctx.expand_tokens(message));
    }
    if flag(step, "must_succeed") {
        return Err(Error::user(format!(
            "Failed to terminate process {program}"
        )));
    }
    Ok(())
}

fn delete_keychain_certificate(ctx: &StepContext<'_>, step: &Step) -> Result<()> {
    let name = ctx.expand_tokens(field(step, "name")?.as_str().unwrap_or_default());
    let mut wanted: Option<String> = None;
    if let Some(spec) = step.get("matching_certificate") {
        let certificate = ctx.resolve_path(spec)?;
        if !certificate.exists() {
            return Ok(());
        }
        let out = Command::new("/usr/bin/openssl")
            .args(["x509", "-fingerprint", "-sha256", "-noout", "-in"])
            .arg(&certificate)
            .output()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let hash = text
            .lines()
            .next()
            .and_then(|l| l.split_once('='))
            .map(|(_, v)| v.replace(':', "").trim().to_ascii_uppercase())
            .unwrap_or_default();
        if hash.is_empty() {
            return Ok(());
        }
        wanted = Some(hash);
    }

    let listed = Command::new("/usr/bin/sudo")
        .args([
            "-E",
            "--",
            "/usr/bin/security",
            "find-certificate",
            "-a",
            "-c",
        ])
        .arg(&name)
        .arg("-Z")
        .output()?;
    let hashes: Vec<String> = String::from_utf8_lossy(&listed.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("SHA-256 hash:"))
        .map(|h| h.trim().to_ascii_uppercase())
        .collect();

    for hash in hashes {
        if wanted.as_ref().is_some_and(|w| *w != hash) {
            continue;
        }
        run_tool(
            "/usr/bin/security",
            &["delete-certificate".to_string(), "-Z".to_string(), hash],
            &[],
            true,
        )?;
    }
    Ok(())
}

fn current_user() -> String {
    std::env::var("USER").unwrap_or_else(|_| "root".to_string())
}

fn sudo_remove(path: &Path) -> Result<()> {
    let recursive = path.is_dir() && !path.is_symlink();
    let mut args = vec![];
    if recursive {
        args.push("-R".to_string());
    }
    args.push("-f".to_string());
    args.push("--".to_string());
    run_tool(
        "/bin/rm",
        &args,
        std::slice::from_ref(&path.to_path_buf()),
        true,
    )
}

/// Run `program` with `args` followed by `paths`, optionally under `sudo`.
pub fn run_tool(program: &str, args: &[String], paths: &[PathBuf], sudo: bool) -> Result<()> {
    let mut cmd = if sudo {
        let mut c = Command::new("/usr/bin/sudo");
        c.arg("-E").arg("--").arg(program);
        c
    } else {
        Command::new(program)
    };
    cmd.args(args).args(paths);
    cmd.stdin(Stdio::null());
    let status = cmd.status()?;
    if !status.success() {
        return Err(Error::user(format!(
            "Failure while executing; `{program}` exited with {}.",
            status.code().unwrap_or(-1)
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context<'a>(cfg: &'a Config, dirs: &'a CaskDirs, staged: PathBuf) -> StepContext<'a> {
        StepContext {
            cfg,
            dirs,
            token: "demo".into(),
            name: "Demo".into(),
            version: "1.2.3".into(),
            arch: "arm64".into(),
            caskroom_path: staged.parent().unwrap_or(Path::new("/")).to_path_buf(),
            staged_path: staged,
            verbose: false,
        }
    }

    #[test]
    fn strips_symbol_keys() {
        let raw = serde_json::json!({":type": "mkdir_p", ":path": {":base": "home", ":path": "x"}});
        assert_eq!(
            normalise(&raw),
            serde_json::json!({"type": "mkdir_p", "path": {"base": "home", "path": "x"}})
        );
    }

    #[test]
    fn expands_tokens_and_bases() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::cask::tests_support::config(tmp.path());
        let dirs = CaskDirs::resolve(&cfg, &[]);
        let ctx = context(&cfg, &dirs, tmp.path().join("Caskroom/demo/1.2.3"));

        assert_eq!(
            ctx.expand_tokens("{{HOMEBREW_PREFIX}}/share/x"),
            format!("{}/share/x", cfg.prefix.display())
        );
        assert_eq!(ctx.expand_tokens("v{{version.major}}"), "v1");
        assert_eq!(ctx.expand_tokens("v{{version.major_minor}}"), "v1.2");
        assert_eq!(ctx.expand_tokens("{{token}}"), "demo");
        // Unknown tokens survive untouched.
        assert_eq!(ctx.expand_tokens("{{nope}}"), "{{nope}}");

        let spec = serde_json::json!({"base": "homebrew_prefix", "path": "share/x"});
        assert_eq!(ctx.resolve_path(&spec).unwrap(), cfg.prefix.join("share/x"));
        let spec = serde_json::json!({"base": "staged_path", "path": "a/b"});
        assert_eq!(
            ctx.resolve_path(&spec).unwrap(),
            ctx.staged_path.join("a/b")
        );
        let spec = serde_json::json!({"base": "home", "path": "Library/X"});
        assert_eq!(ctx.resolve_path(&spec).unwrap(), cfg.home.join("Library/X"));
        let spec = serde_json::json!({"path": "~/abs"});
        assert_eq!(ctx.resolve_path(&spec).unwrap(), cfg.home.join("abs"));
    }

    #[test]
    fn runs_mkdir_copy_symlink_and_remove() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::cask::tests_support::config(tmp.path());
        let dirs = CaskDirs::resolve(&cfg, &[]);
        let staged = tmp.path().join("Caskroom/demo/1.2.3");
        std::fs::create_dir_all(staged.join("payload")).unwrap();
        std::fs::write(staged.join("payload/file.txt"), b"hi").unwrap();
        let ctx = context(&cfg, &dirs, staged.clone());

        let steps: Vec<Step> = serde_json::from_value::<Vec<Value>>(serde_json::json!([
            {"type": "mkdir_p", "path": {"base": "homebrew_prefix", "path": "share/demo"}},
            {"type": "copy", "source": {"base": "staged_path", "path": "payload/file.txt"},
             "target": {"base": "homebrew_prefix", "path": "share/demo/file.txt"}},
            {"type": "symlink", "source": {"base": "homebrew_prefix", "path": "share/demo/file.txt"},
             "target": {"base": "homebrew_prefix", "path": "share/demo/link.txt"}, "uninstall": true},
            {"type": "remove", "paths": [{"base": "staged_path", "path": "payload/file.txt"}]}
        ]))
        .unwrap()
        .into_iter()
        .map(|v| v.as_object().unwrap().clone())
        .collect();

        run(&ctx, &steps, Phase::Install).unwrap();
        assert!(cfg.prefix.join("share/demo/file.txt").is_file());
        assert!(cfg.prefix.join("share/demo/link.txt").is_symlink());
        assert!(!staged.join("payload/file.txt").exists());

        run(&ctx, &steps, Phase::Uninstall).unwrap();
        assert!(!cfg.prefix.join("share/demo/link.txt").is_symlink());
        assert!(cfg.prefix.join("share/demo/file.txt").is_file());
    }

    #[test]
    fn guards_skip_steps() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::cask::tests_support::config(tmp.path());
        let dirs = CaskDirs::resolve(&cfg, &[]);
        let ctx = context(&cfg, &dirs, tmp.path().join("Caskroom/demo/1.2.3"));

        let steps: Vec<Step> = serde_json::from_value::<Vec<Value>>(serde_json::json!([
            {"type": "mkdir_p", "path": {"base": "homebrew_prefix", "path": "skipped"},
             "guards": [{"condition": "if_exists", "path": "/definitely/not/here"}]},
            {"type": "mkdir_p", "path": {"base": "homebrew_prefix", "path": "made"},
             "guards": [{"condition": "unless_exists", "path": "/definitely/not/here"}]}
        ]))
        .unwrap()
        .into_iter()
        .map(|v| v.as_object().unwrap().clone())
        .collect();

        run(&ctx, &steps, Phase::Install).unwrap();
        assert!(!cfg.prefix.join("skipped").exists());
        assert!(cfg.prefix.join("made").is_dir());
    }

    #[test]
    fn version_parts() {
        assert_eq!(version_major("1.2.3"), "1");
        assert_eq!(version_major_minor("1.2.3"), "1.2");
        assert_eq!(version_major_minor("2025.0308"), "2025.0308");
        assert_eq!(version_major("v3"), "3");
    }
}
