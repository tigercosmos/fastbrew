//! Declarative formula `post_install_steps` (`docs/DESIGN.md` 7).
//!
//! Port of the formula half of `Library/Homebrew/install_steps.rb` plus
//! `install_steps/formula_actions.rb`. The cask half lives in
//! `crate::cask::steps`; the two runners share the step grammar (path specs
//! with a `base`, `{{token}}` templates, `guards`) but resolve bases against
//! different roots and implement different step types, so only the small
//! helpers (`normalise`, `run_tool`, the version splitters) are shared.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Map, Value};

use crate::cask::steps::{normalise, run_tool, version_major, version_major_minor};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::keg::Keg;
use crate::model::FormulaEntry;
use crate::output;

/// One normalised step: string keys, no leading colons.
pub type Step = Map<String, Value>;

/// Roots a formula step's `base:` and `{{token}}` can name.
pub struct StepContext<'a> {
    pub cfg: &'a Config,
    pub name: String,
    pub version: String,
    /// `$CELLAR/<name>/<version>`.
    pub prefix: PathBuf,
    /// `$PREFIX/opt/<name>`.
    pub opt_prefix: PathBuf,
    pub verbose: bool,
}

impl<'a> StepContext<'a> {
    pub fn new(cfg: &'a Config, formula: &FormulaEntry, keg: &Keg) -> StepContext<'a> {
        StepContext {
            cfg,
            name: formula.name.clone(),
            version: keg.version.version.to_string(),
            prefix: keg.path.clone(),
            opt_prefix: cfg.opt_record(&formula.name),
            verbose: cfg.verbose,
        }
    }

    /// `Runner#root_path` plus `Formula`'s path accessors.
    fn root_path(&self, base: &str) -> Result<PathBuf> {
        let prefix = &self.cfg.prefix;
        // `Formula#pkgetc` and `#pkgshare` drop a tap prefix from the name.
        let short = crate::deps::short_name(&self.name);
        Ok(match base {
            "homebrew_prefix" => prefix.clone(),
            "homebrew_cellar" => self.cfg.cellar.clone(),
            "home" => self.cfg.home.clone(),
            "temp" => self.cfg.temp.clone(),
            "rack" => self.cfg.rack(&self.name),
            "prefix" => self.prefix.clone(),
            "opt_prefix" => self.opt_prefix.clone(),
            "bin" => self.prefix.join("bin"),
            "sbin" => self.prefix.join("sbin"),
            "lib" => self.prefix.join("lib"),
            "libexec" => self.prefix.join("libexec"),
            "include" => self.prefix.join("include"),
            "share" => self.prefix.join("share"),
            "pkgshare" => self.prefix.join("share").join(short),
            "frameworks" => self.prefix.join("Frameworks"),
            "elisp" => self.prefix.join("share/emacs/site-lisp").join(short),
            "info" => self.prefix.join("share/info"),
            "man" => self.prefix.join("share/man"),
            // `etc` and `var` are shared prefix directories, not keg ones.
            "etc" => prefix.join("etc"),
            "var" => prefix.join("var"),
            "pkgetc" | "formula_pkgetc" => prefix.join("etc").join(short),
            "formula_opt_prefix" => self.cfg.opt_record(short),
            "bash_completion" => prefix.join("etc/bash_completion.d"),
            "zsh_completion" => prefix.join("share/zsh/site-functions"),
            "fish_completion" => prefix.join("share/fish/vendor_completions.d"),
            "pwsh_completion" => prefix.join("share/pwsh/completions"),
            other => {
                return Err(Error::user(format!("unknown install step base: {other}")));
            }
        })
    }

    /// `Runner#template_token_value`.
    fn token_value(&self, token: &str) -> Option<String> {
        let path = |p: PathBuf| Some(p.to_string_lossy().into_owned());
        match token {
            "HOMEBREW_PREFIX" => path(self.cfg.prefix.clone()),
            "HOMEBREW_CELLAR" => path(self.cfg.cellar.clone()),
            "HOMEBREW_BREW_FILE" => path(self.cfg.prefix.join("bin/brew")),
            "name" | "formula_name" => Some(self.name.clone()),
            "arch" => Some(crate::platform::Host::detect().arch.as_str().to_string()),
            "user" => std::env::var("USER").ok(),
            "version" => Some(self.version.clone()),
            "version.major" => Some(version_major(&self.version)),
            "version.major_minor" => Some(version_major_minor(&self.version)),
            other => self.root_path(other).ok().and_then(path),
        }
    }

    /// `Runner#expand_template_tokens`: unknown tokens survive verbatim.
    pub fn expand_tokens(&self, content: &str) -> String {
        let mut out = String::with_capacity(content.len());
        let mut rest = content;
        while let Some(start) = rest.find("{{") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let Some(end) = after.find("}}") else {
                out.push_str(&rest[start..]);
                return out;
            };
            let token = &after[..end];
            let valid = !token.is_empty()
                && token.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                && token
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.');
            match valid.then(|| self.token_value(token)).flatten() {
                Some(value) => out.push_str(&value),
                None => out.push_str(&rest[start..start + 4 + end]),
            }
            rest = &after[end + 2..];
        }
        out.push_str(rest);
        out
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
                out.extend(glob_or_literal(&Path::new(dir).join(&path)));
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

    /// `Runner#run_formula_tool`: `<prefix>/opt/<formula>/bin/<tool>`.
    fn formula_tool(&self, formula: &str, executable: &str) -> Result<PathBuf> {
        let tool = self.cfg.opt_record(formula).join("bin").join(executable);
        if !is_executable(&tool) {
            return Err(Error::user(format!(
                "{formula} is missing required executable: {}",
                tool.display()
            )));
        }
        Ok(tool)
    }
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
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
    if let Some(rest) = value.strip_prefix("~/") {
        return cfg.home.join(rest);
    }
    if value == "~" {
        return cfg.home.clone();
    }
    PathBuf::from(value)
}

/// Normalise the API's `post_install_steps` array.
pub fn steps_from_value(values: &[Value]) -> Vec<Step> {
    values
        .iter()
        .map(normalise)
        .filter_map(|v| v.as_object().cloned())
        .collect()
}

/// Run every step of a formula's `post_install_steps`.
pub fn run(ctx: &StepContext<'_>, steps: &[Step]) -> Result<()> {
    let mut guard_cache: BTreeMap<String, bool> = BTreeMap::new();
    for step in steps {
        if !guards_match(ctx, step, &mut guard_cache) {
            continue;
        }
        run_step(ctx, step)?;
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

fn string(step: &Step, key: &str) -> Result<String> {
    Ok(field(step, key)?.as_str().unwrap_or_default().to_string())
}

fn flag(step: &Step, key: &str) -> bool {
    step.get(key) == Some(&Value::Bool(true))
}

/// `Runner#run_install_step`, formula steps only.
fn run_step(ctx: &StepContext<'_>, step: &Step) -> Result<()> {
    let kind = field(step, "type")?
        .as_str()
        .unwrap_or_default()
        .to_string();
    match kind.as_str() {
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
            if path.exists() {
                filetime::set_file_mtime(&path, filetime::FileTime::now())?;
            } else {
                std::fs::File::create(&path)?;
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
            if overwrite
                && destination != source
                && (destination.exists() || destination.is_symlink())
            {
                remove_path(&destination)?;
            }
            move_path(&source, &destination)?;
        }
        "move_children" | "move_contents" => {
            let source = ctx.resolve_path(field(step, "source")?)?;
            let target = ctx.resolve_path(field(step, "target")?)?;
            std::fs::create_dir_all(&target)?;
            for child in children(&source) {
                if child == target {
                    continue;
                }
                let dst = target.join(child.file_name().unwrap_or_default());
                move_path(&child, &dst)?;
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
            if overwrite && (destination.exists() || destination.is_symlink()) {
                remove_path(&destination)?;
            }
            copy_path(&source, &destination)?;
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
                if flag(step, "recursive") {
                    remove_path(&path)?;
                } else {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        "inreplace" => {
            let path = ctx.resolve_path(field(step, "path")?)?;
            let before = ctx.expand_tokens(&string(step, "before")?);
            let after = ctx.expand_tokens(&string(step, "after")?);
            let text = std::fs::read_to_string(&path)?;
            let replaced = if flag(step, "regexp") {
                let re = regex::Regex::new(&before)
                    .map_err(|e| Error::user(format!("invalid inreplace pattern {before}: {e}")))?;
                if flag(step, "first_only") {
                    re.replace(&text, after.as_str()).into_owned()
                } else {
                    re.replace_all(&text, after.as_str()).into_owned()
                }
            } else if flag(step, "first_only") {
                text.replacen(&before, &after, 1)
            } else {
                text.replace(&before, &after)
            };
            crate::keg::atomic_write(&path, replaced.as_bytes())?;
        }
        "link_dir" => link_dir(ctx, step)?,
        "link_children" => {
            let target = ctx.resolve_path(field(step, "target")?)?;
            std::fs::create_dir_all(&target)?;
            let prefix = ctx.expand_tokens(
                step.get("prefix")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            );
            let suffix = ctx.expand_tokens(
                step.get("suffix")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            );
            let source = ctx.resolve_path(field(step, "source")?)?;
            for child in children(&source) {
                let base = child.file_name().unwrap_or_default().to_string_lossy();
                let link = target.join(format!("{prefix}{base}{suffix}"));
                let _ = std::fs::remove_file(&link);
                std::os::unix::fs::symlink(&child, &link)?;
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
        "run" => run_command_step(ctx, step)?,
        "init_data_dir" => init_data_dir(ctx, step)?,
        "terminate_process" => terminate_process(ctx, step)?,
        "change_dylib_id" => {
            let source = ctx.resolve_path(field(step, "source")?)?;
            let source = if flag(step, "resolve_source") {
                std::fs::canonicalize(&source).unwrap_or(source)
            } else {
                source
            };
            let id = ctx.expand_tokens(&string(step, "id")?);
            // `MachO::Tools.change_dylib_id` followed by `MachO.codesign!`.
            crate::bottle::with_writable(&source, || {
                run_tool(
                    "/usr/bin/install_name_tool",
                    &["-id".to_string(), id.clone()],
                    std::slice::from_ref(&source),
                    false,
                )?;
                crate::bottle::codesign::codesign_one(&source);
                Ok(())
            })?;
        }
        "warn" => output::opoo(&ctx.expand_tokens(&string(step, "message")?)),
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
            args.push(string(step, "permissions")?);
            run_tool("/bin/chmod", &args, &paths, false)?;
        }
        "set_ownership" => {
            let paths = existing_paths(ctx, step)?;
            if paths.is_empty() {
                return Ok(());
            }
            output::ohai(&format!(
                "Changing ownership of paths required by {} with `sudo` (which may request your password)...",
                ctx.name
            ));
            let user = step
                .get("user")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| std::env::var("USER").unwrap_or_else(|_| "root".into()));
            let group = step.get("group").and_then(Value::as_str).unwrap_or("staff");
            let mut args: Vec<String> = Vec::new();
            if !flag(step, "non_recursive") {
                args.push("-R".to_string());
            }
            args.push("--".to_string());
            args.push(format!("{user}:{group}"));
            run_tool("/usr/sbin/chown", &args, &paths, true)?;
        }
        "install_gzipped_executable" => install_gzipped_executable(ctx, step)?,
        "compile_gsettings_schemas" => {
            let path = ctx.resolve_path(field(step, "path")?)?;
            let tool = ctx.formula_tool("glib", "glib-compile-schemas")?;
            run_tool(&tool.to_string_lossy(), &[], &[path], false)?;
        }
        "gio_querymodules" => {
            let path = ctx.resolve_path(field(step, "path")?)?;
            let tool = ctx.formula_tool("glib", "gio-querymodules")?;
            run_tool(&tool.to_string_lossy(), &[], &[path], false)?;
        }
        "gdk_pixbuf_query_loaders" => {
            let tool = ctx.formula_tool("gdk-pixbuf", "gdk-pixbuf-query-loaders")?;
            run_tool(
                &tool.to_string_lossy(),
                &["--update-cache".to_string()],
                &[],
                false,
            )?;
        }
        "gtk_update_icon_cache" => {
            let path = ctx.resolve_path(field(step, "path")?)?;
            let (formula, exe) = if ctx.cfg.rack("gtk4").is_dir() {
                ("gtk4", "gtk4-update-icon-cache")
            } else {
                ("gtk+3", "gtk3-update-icon-cache")
            };
            let tool = ctx.formula_tool(formula, exe)?;
            let args = ["-q", "-t", "-f"].map(str::to_string);
            run_tool(&tool.to_string_lossy(), &args, &[path], false)?;
        }
        "update_mime_database" => {
            let path = ctx.resolve_path(field(step, "path")?)?;
            let tool = ctx.formula_tool("shared-mime-info", "update-mime-database")?;
            run_tool(&tool.to_string_lossy(), &[], &[path], false)?;
        }
        "update_desktop_database" => {
            let path = ctx.resolve_path(field(step, "path")?)?;
            let tool = ctx.formula_tool("desktop-file-utils", "update-desktop-database")?;
            run_tool(&tool.to_string_lossy(), &[], &[path], false)?;
        }
        "configure_gcc_runtime" => {
            // `run_configure_gcc_runtime` returns immediately unless the system
            // is Linux, so on macOS there is nothing to do.
            if cfg!(target_os = "linux") {
                return Err(unsupported(&ctx.name, &kind));
            }
        }
        "configure_clang_system" => configure_clang_system(ctx)?,
        "configure_glibc_runtime"
        | "configure_php"
        | "bootstrap_cpython"
        | "bootstrap_pypy"
        | "delete_keychain_certificate" => return Err(unsupported(&ctx.name, &kind)),
        other => {
            return Err(Error::user(format!(
                "Unknown post-install step '{other}' for {}.\nYou can run it manually using:\n  brew postinstall {}",
                ctx.name, ctx.name
            )));
        }
    }
    Ok(())
}

/// A step fastbrew deliberately leaves to the Ruby `brew`.
fn unsupported(name: &str, kind: &str) -> Error {
    Error::user(format!(
        "The `{kind}` post-install step needs the Ruby formula DSL.\nYou can run it manually using:\n  brew postinstall {name}"
    ))
}

fn children(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    out.sort();
    out
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

fn create_symlink(source: &str, target: &Path, step: &Step) -> Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if flag(step, "force") {
        let _ = std::fs::remove_file(target);
    }
    std::os::unix::fs::symlink(source, target)?;
    Ok(())
}

/// `link_dir`: mirror a directory tree with symlinks to its files.
fn link_dir(ctx: &StepContext<'_>, step: &Step) -> Result<()> {
    let source_dir = ctx.resolve_path(field(step, "source")?)?;
    let target_dir = ctx.resolve_path(field(step, "target")?)?;
    let mut stack = vec![source_dir.clone()];
    while let Some(source) = stack.pop() {
        let relative = source.strip_prefix(&source_dir).unwrap_or(Path::new(""));
        let link_target = target_dir.join(relative);
        let base = source.file_name().unwrap_or_default();
        if base == std::ffi::OsStr::new(".DS_Store") {
            continue;
        }
        let is_dir = source.is_dir() && !source.is_symlink();
        if !(link_target.is_dir() && !link_target.is_symlink()) {
            if link_target.exists() || link_target.is_symlink() {
                remove_path(&link_target)?;
            }
            if let Some(parent) = link_target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if is_dir {
                std::fs::create_dir_all(&link_target)?;
            } else {
                std::os::unix::fs::symlink(&source, &link_target)?;
            }
        }
        if is_dir {
            stack.extend(children(&source));
        }
    }
    Ok(())
}

/// `run_install_gzipped_executable`.
fn install_gzipped_executable(ctx: &StepContext<'_>, step: &Step) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let source = ctx.resolve_path(field(step, "source")?)?;
    if !source.exists() {
        return Ok(());
    }
    let target = ctx.resolve_path(field(step, "target")?)?;
    let parent = target.parent().unwrap_or(Path::new("/")).to_path_buf();
    std::fs::create_dir_all(&parent)?;
    let base = target.file_name().unwrap_or_default().to_string_lossy();
    let temporary = parent.join(format!(".{base}.install-step"));
    let _ = std::fs::remove_file(&temporary);

    let file = std::fs::File::open(&source)?;
    let mut decoder = flate2::read::GzDecoder::new(std::io::BufReader::new(file));
    let mut bytes = Vec::new();
    decoder.read_to_end(&mut bytes)?;
    std::fs::write(&temporary, &bytes)?;
    let _ = std::fs::remove_file(&target);
    std::fs::rename(&temporary, &target)?;
    std::fs::remove_file(&source)?;
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// `run_init_data_dir`: `postgresql_initdb`, `mysql_initialize`, `mariadb_install_db`.
fn init_data_dir(ctx: &StepContext<'_>, step: &Step) -> Result<()> {
    let using = string(step, "using")?;
    let marker = match using.as_str() {
        "postgresql_initdb" => "PG_VERSION",
        "mysql_initialize" => "mysql/general_log.CSM",
        "mariadb_install_db" => "mysql/user.frm",
        other => {
            return Err(Error::user(format!(
                "unknown data directory initialiser: {other}"
            )));
        }
    };
    let path = ctx.resolve_path(field(step, "path")?)?;
    std::fs::create_dir_all(&path)?;
    if std::env::var_os("HOMEBREW_GITHUB_ACTIONS").is_some() || path.join(marker).exists() {
        return Ok(());
    }
    let bin = ctx.prefix.join("bin");
    let prefix = ctx.prefix.display().to_string();
    let data = path.display().to_string();
    let user = std::env::var("USER").unwrap_or_else(|_| "root".into());
    match using.as_str() {
        "postgresql_initdb" => {
            let locale = step
                .get("locale")
                .and_then(Value::as_str)
                .unwrap_or("en_US.UTF-8");
            run_tool(
                &bin.join("initdb").to_string_lossy(),
                &[
                    format!("--locale={locale}"),
                    "-E".into(),
                    "UTF-8".into(),
                    data,
                ],
                &[],
                false,
            )
        }
        "mysql_initialize" => run_tool(
            &bin.join("mysqld").to_string_lossy(),
            &[
                "--initialize-insecure".into(),
                format!("--user={user}"),
                format!("--basedir={prefix}"),
                format!("--datadir={data}"),
                "--tmpdir=/tmp".into(),
            ],
            &[],
            false,
        ),
        _ => run_tool(
            &bin.join("mysql_install_db").to_string_lossy(),
            &[
                "--verbose".into(),
                format!("--user={user}"),
                format!("--basedir={prefix}"),
                format!("--datadir={data}"),
                "--tmpdir=/tmp".into(),
            ],
            &[],
            false,
        ),
    }
}

/// `Runner#run_serialised_command`.
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

    let sudo = flag(step, "sudo");
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
        cmd.stdin(Stdio::from(std::fs::File::open(
            ctx.resolve_path(stdin_path)?,
        )?));
    } else {
        cmd.stdin(Stdio::null());
    }
    let capture_stdout = step.contains_key("stdout_path");
    if capture_stdout || !flag(step, "print_stdout") {
        cmd.stdout(Stdio::piped());
    }
    if step.get("suppress_stderr") == Some(&Value::Bool(true)) && !ctx.verbose {
        cmd.stderr(Stdio::null());
    }

    let output = cmd.output().map_err(|e| {
        Error::user(format!(
            "Failure while executing; `{program}` could not be run: {e}"
        ))
    })?;
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

/// `Runner#run_terminate_process`.
fn terminate_process(ctx: &StepContext<'_>, step: &Step) -> Result<()> {
    if let Some(notices) = step.get("notices").and_then(Value::as_array) {
        for notice in notices.iter().filter_map(Value::as_str) {
            output::ohai(&ctx.expand_tokens(notice));
        }
    }
    let name = ctx.expand_tokens(&string(step, "name")?);
    let (program, args) = if step.get("match").and_then(Value::as_str) == Some("full") {
        ("/usr/bin/pkill", vec!["-f".to_string(), name])
    } else {
        ("/usr/bin/killall", vec![name])
    };
    let sudo = flag(step, "sudo");
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

/// `run_configure_clang_system` plus `Utils::Clang.write_system_config_files`.
fn configure_clang_system(ctx: &StepContext<'_>) -> Result<()> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }
    let host = crate::platform::Host::detect();
    let Some(macos) = host.macos else {
        return Ok(());
    };
    let Some(kernel) = kernel_major() else {
        return Err(Error::user(
            "Clang system configuration requires a kernel version".to_string(),
        ));
    };
    let macos_version = macos.major.to_string();
    let config_dir = ctx.prefix.join("etc/clang");
    let arches = ["arm64", "x86_64", "aarch64", host.arch.as_str()];
    let mut arches: Vec<&str> = arches.to_vec();
    arches.dedup();

    let systems = [
        ("darwin", kernel.to_string()),
        ("macosx", macos_version.clone()),
    ];
    let all_present = arches.iter().all(|a| {
        systems.iter().all(|(system, version)| {
            config_dir
                .join(format!("{a}-apple-{system}{version}.cfg"))
                .exists()
        })
    });
    if all_present {
        return Ok(());
    }

    const CLT_PKG_PATH: &str = "/Library/Developer/CommandLineTools";
    let specific = format!("{CLT_PKG_PATH}/SDKs/MacOSX{macos_version}.sdk");
    let sysroot = if Path::new(&specific).exists() {
        specific
    } else {
        format!("{CLT_PKG_PATH}/SDKs/MacOSX.sdk")
    };
    std::fs::create_dir_all(&config_dir)?;
    for (system, version) in &systems {
        for arch in &arches {
            crate::keg::atomic_write(
                &config_dir.join(format!("{arch}-apple-{system}{version}.cfg")),
                format!("-isysroot {sysroot}\n").as_bytes(),
            )?;
        }
    }
    Ok(())
}

/// `OS.kernel_version.major` from `uname -r`.
fn kernel_major() -> Option<u32> {
    let out = Command::new("/usr/bin/uname").arg("-r").output().ok()?;
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .split('.')
        .next()?
        .parse()
        .ok()
}

// ------------------------------------------------------------ file helpers

/// `FileUtils.mv` across devices.
pub fn move_path(source: &Path, destination: &Path) -> Result<()> {
    if std::fs::rename(source, destination).is_ok() {
        return Ok(());
    }
    copy_path(source, destination)?;
    remove_path(source)
}

/// `FileUtils.cp_r`, preserving symlinks and modes.
pub fn copy_path(source: &Path, destination: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(source)?;
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(source)?;
        let _ = std::fs::remove_file(destination);
        std::os::unix::fs::symlink(target, destination)?;
        return Ok(());
    }
    if meta.is_dir() {
        std::fs::create_dir_all(destination)?;
        for child in children(source) {
            copy_path(
                &child,
                &destination.join(child.file_name().unwrap_or_default()),
            )?;
        }
        return Ok(());
    }
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(source, destination)?;
    Ok(())
}

/// `FileUtils.rm_rf`.
pub fn remove_path(path: &Path) -> Result<()> {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Ok(());
    };
    if meta.is_dir() && !meta.file_type().is_symlink() {
        std::fs::remove_dir_all(path)?;
    } else {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(root: &Path) -> (Config, FormulaEntry, Keg) {
        let cfg = Config::for_test(root);
        let formula = FormulaEntry {
            name: "demo".into(),
            stable_version: Some("1.2.3".into()),
            ..Default::default()
        };
        let keg = Keg::new(&cfg, "demo", "1.2.3");
        std::fs::create_dir_all(keg.path.join("bin")).unwrap();
        std::fs::create_dir_all(cfg.prefix.join("etc")).unwrap();
        (cfg, formula, keg)
    }

    fn steps(value: serde_json::Value) -> Vec<Step> {
        steps_from_value(value.as_array().unwrap())
    }

    #[test]
    fn resolves_formula_bases_and_tokens() {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg, formula, keg) = setup(tmp.path());
        let ctx = StepContext::new(&cfg, &formula, &keg);
        assert_eq!(
            ctx.resolve_path(&serde_json::json!({"base": "var", "path": "demo"}))
                .unwrap(),
            cfg.prefix.join("var/demo")
        );
        assert_eq!(
            ctx.resolve_path(&serde_json::json!({"base": "libexec", "path": "x"}))
                .unwrap(),
            keg.path.join("libexec/x")
        );
        assert_eq!(
            ctx.resolve_path(&serde_json::json!({"base": "pkgetc", "path": "a.conf"}))
                .unwrap(),
            cfg.prefix.join("etc/demo/a.conf")
        );
        assert_eq!(ctx.expand_tokens("v{{version.major_minor}}"), "v1.2");
        assert_eq!(ctx.expand_tokens("{{formula_name}}"), "demo");
        assert_eq!(
            ctx.expand_tokens("{{opt_prefix}}/bin"),
            format!("{}/bin", cfg.opt_record("demo").display())
        );
        assert_eq!(ctx.expand_tokens("{{nope}}"), "{{nope}}");
    }

    #[test]
    fn runs_mkdir_write_symlink_and_link_children() {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg, formula, keg) = setup(tmp.path());
        let ctx = StepContext::new(&cfg, &formula, &keg);
        std::fs::write(keg.path.join("bin/demo"), "#!/bin/sh\n").unwrap();

        run(
            &ctx,
            &steps(serde_json::json!([
                {"type": "mkdir_p", "path": {"base": "var", "path": "demo"}},
                {"type": "write", "path": {"base": "etc", "path": "demo.conf"},
                 "content": "prefix = {{HOMEBREW_PREFIX}}\n", "overwrite": true},
                {"type": "touch", "path": {"base": "var", "path": "demo/stamp"}},
                {"type": "symlink", "source": {"base": "prefix", "path": "bin/demo"},
                 "target": {"base": "var", "path": "demo/demo"}, "force": true},
                {"type": "link_children", "source": {"base": "prefix", "path": "bin"},
                 "target": {"base": "homebrew_prefix", "path": "bin"}, "suffix": "-{{version.major}}"}
            ])),
        )
        .unwrap();

        assert!(cfg.prefix.join("var/demo").is_dir());
        assert_eq!(
            std::fs::read_to_string(cfg.prefix.join("etc/demo.conf")).unwrap(),
            format!("prefix = {}\n", cfg.prefix.display())
        );
        assert!(cfg.prefix.join("var/demo/stamp").is_file());
        assert!(cfg.prefix.join("var/demo/demo").is_symlink());
        assert!(cfg.prefix.join("bin/demo-1").is_symlink());
    }

    #[test]
    fn guards_skip_steps_and_inreplace_rewrites() {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg, formula, keg) = setup(tmp.path());
        let ctx = StepContext::new(&cfg, &formula, &keg);
        std::fs::write(cfg.prefix.join("etc/demo.cfg"), "root = /nowhere/demo/1\n").unwrap();

        run(
            &ctx,
            &steps(serde_json::json!([
                {"type": "mkdir_p", "path": {"base": "var", "path": "skipped"},
                 "guards": [{"condition": "if_exists", "path": "/definitely/not/here"}]},
                {"type": "mkdir_p", "path": {"base": "var", "path": "made"},
                 "guards": [{"condition": "unless_exists", "path": "/definitely/not/here"}]},
                {"type": "inreplace", "path": {"base": "etc", "path": "demo.cfg"},
                 "regexp": true, "before": "/nowhere/demo/[^/\\n]", "after": "{{opt_prefix}}"}
            ])),
        )
        .unwrap();
        assert!(!cfg.prefix.join("var/skipped").exists());
        assert!(cfg.prefix.join("var/made").is_dir());
        assert_eq!(
            std::fs::read_to_string(cfg.prefix.join("etc/demo.cfg")).unwrap(),
            format!("root = {}\n", cfg.opt_record("demo").display())
        );
    }

    #[test]
    fn unknown_steps_point_at_brew_postinstall() {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg, formula, keg) = setup(tmp.path());
        let ctx = StepContext::new(&cfg, &formula, &keg);
        let err = run(&ctx, &steps(serde_json::json!([{"type": "wat"}]))).unwrap_err();
        assert!(err.to_string().contains("brew postinstall demo"), "{err}");
        let err = run(&ctx, &steps(serde_json::json!([{"type": "configure_php"}]))).unwrap_err();
        assert!(err.to_string().contains("brew postinstall demo"), "{err}");
    }

    #[test]
    fn installs_a_gzipped_executable() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let (cfg, formula, keg) = setup(tmp.path());
        let ctx = StepContext::new(&cfg, &formula, &keg);
        std::fs::create_dir_all(keg.path.join("libexec/bin")).unwrap();
        let gz = keg.path.join("libexec/bin/tool.gz");
        let mut encoder = flate2::write::GzEncoder::new(
            std::fs::File::create(&gz).unwrap(),
            flate2::Compression::default(),
        );
        encoder.write_all(b"#!/bin/sh\necho tool\n").unwrap();
        encoder.finish().unwrap();

        run(
            &ctx,
            &steps(serde_json::json!([
                {"type": "install_gzipped_executable",
                 "source": {"base": "libexec", "path": "bin/tool.gz"},
                 "target": {"base": "libexec", "path": "bin/tool"}}
            ])),
        )
        .unwrap();
        let tool = keg.path.join("libexec/bin/tool");
        assert_eq!(
            std::fs::read_to_string(&tool).unwrap(),
            "#!/bin/sh\necho tool\n"
        );
        assert!(!gz.exists(), "the gzipped source is removed");
        assert!(is_executable(&tool));
    }
}
