//! Cask artifact install/uninstall phases (port of `Library/Homebrew/cask/artifact/*`).
//!
//! Supported kinds are listed in `docs/DESIGN.md` 5. `uninstall_artifacts_json`
//! produces the v2-style list written into the cask receipt.
//!
//! Artifacts are handled as [`ArtifactSpec`]s: the kind plus its raw API
//! arguments with Ruby symbol keys stripped and the `$APPDIR`,
//! `$HOMEBREW_PREFIX`, `$HOMEBREW_CELLAR` and `/$HOME` placeholders expanded,
//! which is exactly the form Homebrew stores in `uninstall_artifacts`. The same
//! representation therefore drives an install from the API and an uninstall
//! from an installed receipt.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Map, Value};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::model::{CaskEntry, sym};
use crate::output;

use super::config::CaskDirs;
use super::steps::{self, Phase, StepContext};

#[derive(Debug, Clone, Copy, Default)]
pub struct ArtifactOptions {
    pub force: bool,
    pub adopt: bool,
    pub verbose: bool,
    pub dry_run: bool,
    pub skip_binaries: bool,
}

/// One artifact stanza, normalised.
#[derive(Debug, Clone, PartialEq)]
pub struct ArtifactSpec {
    pub kind: String,
    pub args: Vec<Value>,
}

/// Identity of the cask an artifact belongs to.
#[derive(Debug, Clone)]
pub struct CaskContext {
    pub token: String,
    pub name: String,
    pub version: String,
    pub staged_path: PathBuf,
    pub caskroom_path: PathBuf,
    /// `url only_path:`: the subdirectory of the staged container holding the artifacts.
    pub only_path: Option<String>,
    pub auto_updates: bool,
}

impl CaskContext {
    pub fn from_entry(cfg: &Config, cask: &CaskEntry) -> CaskContext {
        let version = cask.version.clone().unwrap_or_else(|| "latest".to_string());
        CaskContext {
            token: cask.token.clone(),
            name: cask.display_name().to_string(),
            caskroom_path: super::caskroom_path(cfg, &cask.token),
            staged_path: super::staged_path(cfg, &cask.token, &version),
            version,
            only_path: super::download::UrlKwargs::from_value(cask.url_kwargs.as_ref()).only_path,
            auto_updates: cask.auto_updates,
        }
    }

    /// `Relocated#source`'s base: the staged path, plus `only_path`.
    fn source_base(&self) -> PathBuf {
        match &self.only_path {
            Some(only) if !only.is_empty() => self.staged_path.join(only),
            _ => self.staged_path.clone(),
        }
    }
}

// --------------------------------------------------------------- kinds

/// Artifacts moved into a configured directory (`Cask::Artifact::Moved`).
pub const MOVED_KINDS: &[&str] = &[
    "app",
    "app_image",
    "appimage",
    "suite",
    "artifact",
    "colorpicker",
    "prefpane",
    "qlplugin",
    "mdimporter",
    "dictionary",
    "font",
    "service",
    "input_method",
    "internet_plugin",
    "keyboard_layout",
    "audio_unit_plugin",
    "vst_plugin",
    "vst3_plugin",
    "screen_saver",
];

/// Artifacts symlinked into the prefix (`Cask::Artifact::Symlinked`).
pub const SYMLINKED_KINDS: &[&str] = &[
    "binary",
    "command_wrapper",
    "manpage",
    "bash_completion",
    "zsh_completion",
    "fish_completion",
];

/// Declarative install-step artifacts.
pub const STEP_KINDS: &[&str] = &[
    "preflight_steps",
    "postflight_steps",
    "uninstall_preflight_steps",
    "uninstall_postflight_steps",
];

/// `AbstractArtifact#sort_order`: artifacts run in this order, ties keeping
/// their declaration order.
pub fn sort_index(kind: &str) -> u32 {
    match kind {
        "preflight_steps" => 0,
        "uninstall_preflight_steps" => 1,
        "preflight" => 2,
        "uninstall" => 3,
        "generated_script" => 4,
        "installer" => 5,
        "pkg" => 6,
        k if MOVED_KINDS.contains(&k) => 7,
        "binary" | "command_wrapper" => 8,
        "manpage" => 9,
        "bash_completion" | "fish_completion" | "zsh_completion" => 10,
        "generate_completions_from_executable" => 11,
        "postflight_steps" => 12,
        "uninstall_postflight_steps" => 13,
        "postflight" => 14,
        "zap" => 15,
        _ => 16,
    }
}

/// `AbstractArtifact.english_name`, used in Homebrew's messages.
pub fn english_name(kind: &str) -> String {
    match kind {
        "app" => "App".into(),
        "suite" => "App Suite".into(),
        "artifact" => "Generic Artifact".into(),
        "qlplugin" => "Quick Look Plugin".into(),
        "mdimporter" => "Spotlight metadata importer".into(),
        "prefpane" => "Preference Pane".into(),
        "vst_plugin" => "VST Plugin".into(),
        "vst3_plugin" => "VST3 Plugin".into(),
        "app_image" | "appimage" => "App Image".into(),
        other => other
            .split('_')
            .map(|word| {
                let mut chars = word.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// `AbstractArtifact.english_article`.
fn english_article(name: &str) -> &'static str {
    if name.starts_with(['a', 'e', 'i', 'o', 'u', 'A', 'E', 'I', 'O', 'U']) {
        "an"
    } else {
        "a"
    }
}

/// Whether the kind runs during uninstall (and so belongs in the receipt's
/// `uninstall_artifacts`).
pub fn has_uninstall_phase(kind: &str) -> bool {
    MOVED_KINDS.contains(&kind)
        || SYMLINKED_KINDS.contains(&kind)
        || STEP_KINDS.contains(&kind)
        || matches!(
            kind,
            "uninstall" | "zap" | "generate_completions_from_executable"
        )
}

// --------------------------------------------------- spec construction

/// Expand `$APPDIR`, `$HOMEBREW_PREFIX`, `$HOMEBREW_CELLAR` and `/$HOME` and
/// drop Ruby symbol colons from hash keys (`CaskStruct#deep_remove_placeholders`).
pub fn normalise_value(cfg: &Config, dirs: &CaskDirs, value: &Value) -> Value {
    match value {
        Value::String(s) => Value::String(
            cfg.expand_placeholders(s)
                .replace("$APPDIR", &dirs.appdir.to_string_lossy()),
        ),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|v| normalise_value(cfg, dirs, v))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (sym(k).to_string(), normalise_value(cfg, dirs, v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// `[":app", ["Demo.app", {..}]]` carries its arguments in one nested array
/// (Ruby splats them into `Artifact.from_args`); a hash payload such as
/// `[":uninstall", {..}]` is a single argument.
fn spread(args: &[Value]) -> Vec<Value> {
    match args {
        [Value::Array(items)] => items.clone(),
        other => other.to_vec(),
    }
}

/// Artifacts of an API cask entry, in Homebrew's run order.
pub fn artifact_specs(cfg: &Config, dirs: &CaskDirs, cask: &CaskEntry) -> Vec<ArtifactSpec> {
    let mut specs: Vec<ArtifactSpec> = cask
        .artifacts()
        .into_iter()
        .map(|a| ArtifactSpec {
            kind: a.kind,
            args: spread(&a.args)
                .iter()
                .map(|v| normalise_value(cfg, dirs, v))
                .collect(),
        })
        .collect();
    specs.sort_by_key(|s| sort_index(&s.kind));
    specs
}

/// Artifacts recovered from a receipt's `uninstall_artifacts` list
/// (`[{"app": ["Demo.app"]}, ...]`), in Homebrew's run order.
pub fn artifact_specs_from_receipt(
    cfg: &Config,
    dirs: &CaskDirs,
    list: &[Value],
) -> Vec<ArtifactSpec> {
    let mut specs: Vec<ArtifactSpec> = list
        .iter()
        .filter_map(Value::as_object)
        .flat_map(|entry| {
            entry
                .iter()
                .filter(|(key, _)| sym(key) != "target")
                .map(|(key, value)| ArtifactSpec {
                    kind: sym(key).to_string(),
                    args: match normalise_value(cfg, dirs, value) {
                        Value::Array(items) => items,
                        Value::Null => vec![],
                        other => vec![other],
                    },
                })
                .collect::<Vec<_>>()
        })
        .collect();
    specs.sort_by_key(|s| sort_index(&s.kind));
    specs
}

/// `Cask#artifacts_list(uninstall_only: true)`: what the receipt records.
pub fn specs_to_json(specs: &[ArtifactSpec]) -> Vec<Value> {
    specs
        .iter()
        .filter(|s| has_uninstall_phase(&s.kind))
        .map(|s| {
            let args: Vec<Value> = s.args.iter().filter(|v| !is_blank(v)).cloned().collect();
            let mut map = Map::new();
            map.insert(s.kind.clone(), Value::Array(args));
            Value::Object(map)
        })
        .collect()
}

/// `Object#blank?` for the values `compact_blank` drops.
fn is_blank(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(s) => s.trim().is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        Value::Bool(b) => !b,
        _ => false,
    }
}

// -------------------------------------------------- source and target

/// Positional string argument `index` of an artifact.
fn arg_str(spec: &ArtifactSpec, index: usize) -> Option<&str> {
    spec.args.get(index).and_then(Value::as_str)
}

/// The `{target: ..}` option hash of a relocated artifact.
fn target_option(spec: &ArtifactSpec) -> Option<&str> {
    spec.args
        .iter()
        .filter_map(Value::as_object)
        .find_map(|m| m.get("target"))
        .and_then(Value::as_str)
}

/// `Relocated#source`.
fn artifact_source(ctx: &CaskContext, spec: &ArtifactSpec) -> Result<PathBuf> {
    let source = arg_str(spec, 0).ok_or_else(|| {
        Error::user(format!(
            "No source provided for {}.",
            english_name(&spec.kind)
        ))
    })?;
    Ok(ctx.source_base().join(source))
}

/// `Relocated#resolve_target` and the per-kind overrides of `manpage` and the
/// shell completions.
fn artifact_target(
    cfg: &Config,
    dirs: &CaskDirs,
    ctx: &CaskContext,
    spec: &ArtifactSpec,
) -> Result<PathBuf> {
    let source = artifact_source(ctx, spec)?;
    let basename = source
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let target = target_option(spec)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .unwrap_or(basename);

    match spec.kind.as_str() {
        "manpage" => {
            let source_name = arg_str(spec, 0).unwrap_or_default();
            let section = manpage_section(source_name).ok_or_else(|| {
                Error::user(format!("'{source_name}' is not a valid man page name"))
            })?;
            Ok(dirs.manpagedir.join(format!("man{section}")).join(target))
        }
        "bash_completion" => Ok(dirs.bash_completion.join(strip_extension(&target))),
        "zsh_completion" => Ok(dirs.zsh_completion.join(if target.starts_with('_') {
            target.clone()
        } else {
            format!("_{}", strip_extension(&target))
        })),
        "fish_completion" => Ok(dirs.fish_completion.join(if target.ends_with(".fish") {
            target.clone()
        } else {
            format!("{}.fish", strip_extension(&target))
        })),
        _ => {
            let path = PathBuf::from(&target);
            if path.is_absolute() {
                return Ok(path);
            }
            if target == "~" || target.starts_with("~/") {
                return Ok(super::config::expand_path(cfg, &target));
            }
            match dirs.dir_for_kind(&spec.kind) {
                Some(base) => Ok(base.join(path)),
                // `artifact` has no default directory and keeps a relative target.
                None => Ok(path),
            }
        }
    }
}

/// `Manpage.from_args`: the section from a `.1`/`.5`/`.n`/`.l` suffix.
pub fn manpage_section(source: &str) -> Option<char> {
    let trimmed = source.strip_suffix(".gz").unwrap_or(source);
    let last = trimmed.rsplit('.').next()?;
    let mut chars = last.chars();
    let section = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    matches!(section, '1'..='8' | 'n' | 'l').then_some(section)
}

fn strip_extension(name: &str) -> String {
    match name.rfind('.') {
        Some(0) | None => name.to_string(),
        Some(idx) => name[..idx].to_string(),
    }
}

/// `Relocated#printable_target`: `$HOME` shortened back to `~`.
fn printable(cfg: &Config, path: &Path) -> String {
    let text = path.to_string_lossy().into_owned();
    let home = cfg.home.to_string_lossy();
    if text == home.as_ref() {
        return "~".to_string();
    }
    match text.strip_prefix(&format!("{home}/")) {
        Some(rest) => format!("~/{rest}"),
        None => text,
    }
}

// ------------------------------------------------------------ install

/// Install the artifacts of an API cask entry.
pub fn install_artifacts(
    cfg: &Config,
    dirs: &CaskDirs,
    cask: &CaskEntry,
    staged: &Path,
    opts: ArtifactOptions,
) -> Result<()> {
    let mut ctx = CaskContext::from_entry(cfg, cask);
    ctx.staged_path = staged.to_path_buf();
    install_specs(cfg, dirs, &artifact_specs(cfg, dirs, cask), &ctx, opts)
}

/// Install `specs` in order, reverting the ones already installed on failure
/// (`Cask::Installer#install_artifacts`).
pub fn install_specs(
    cfg: &Config,
    dirs: &CaskDirs,
    specs: &[ArtifactSpec],
    ctx: &CaskContext,
    opts: ArtifactOptions,
) -> Result<()> {
    let mut installed: Vec<&ArtifactSpec> = Vec::new();
    for spec in specs {
        if spec.kind == "binary" && opts.skip_binaries {
            continue;
        }
        if let Err(error) = install_one(cfg, dirs, spec, ctx, opts) {
            for done in installed.iter().rev() {
                let _ = uninstall_one(cfg, dirs, done, ctx, opts, false);
            }
            return Err(error);
        }
        installed.push(spec);
    }
    Ok(())
}

fn install_one(
    cfg: &Config,
    dirs: &CaskDirs,
    spec: &ArtifactSpec,
    ctx: &CaskContext,
    opts: ArtifactOptions,
) -> Result<()> {
    let kind = spec.kind.as_str();
    if MOVED_KINDS.contains(&kind) {
        return move_artifact(cfg, dirs, spec, ctx, opts);
    }
    if kind == "command_wrapper" {
        write_command_wrapper(spec, ctx)?;
        return link_artifact(cfg, dirs, spec, ctx, opts);
    }
    if SYMLINKED_KINDS.contains(&kind) {
        return link_artifact(cfg, dirs, spec, ctx, opts);
    }
    match kind {
        "pkg" => run_pkg(spec, ctx, opts),
        "installer" => run_installer(cfg, spec, ctx, opts),
        "generated_script" => write_generated_script(spec, ctx),
        "preflight_steps" | "postflight_steps" => run_steps(cfg, dirs, spec, ctx, Phase::Install),
        "generate_completions_from_executable" => generate_completions(cfg, dirs, spec, ctx),
        // `uninstall`, `zap` and the `uninstall_*_steps` have no install phase.
        "uninstall"
        | "zap"
        | "uninstall_preflight_steps"
        | "uninstall_postflight_steps"
        | "stage_only"
        | "preflight"
        | "postflight" => Ok(()),
        other => Err(Error::user(format!(
            "Cask artifact '{other}' is not supported by fastbrew."
        ))),
    }
}

// ------------------------------------------------------- moved (app, font, ..)

fn move_artifact(
    cfg: &Config,
    dirs: &CaskDirs,
    spec: &ArtifactSpec,
    ctx: &CaskContext,
    opts: ArtifactOptions,
) -> Result<()> {
    let name = english_name(&spec.kind);
    let source = artifact_source(ctx, spec)?;
    let target = artifact_target(cfg, dirs, ctx, spec)?;

    if !source.exists() && !source.is_symlink() {
        return Err(Error::user(format!(
            "It seems the {name} source '{}' is not there.",
            source.display()
        )));
    }

    if path_occupied(&target) {
        if opts.adopt {
            output::ohai(&format!(
                "Adopting existing {name} at '{}'",
                target.display()
            ));
            if !ctx.auto_updates && !same_bundle(&source, &target)? {
                return Err(Error::user(format!(
                    "It seems the existing {name} is different from the one being installed."
                )));
            }
            super::unpack::remove_path(&source)?;
            return post_move(&source, &target);
        }
        let message = format!(
            "It seems there is already {} {name} at '{}'",
            english_article(&name),
            target.display()
        );
        if !opts.force {
            return Err(Error::user(format!("{message}.")));
        }
        output::opoo(&format!("{message}; overwriting."));
        delete_target(&target, &name)?;
    }

    output::ohai(&format!(
        "Moving {name} '{}' to '{}'",
        source.file_name().unwrap_or_default().to_string_lossy(),
        target.display()
    ));
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    super::unpack::move_path(&source, &target)?;
    post_move(&source, &target)
}

/// `Moved#post_move`: leave a symlink where the artifact was staged.
fn post_move(source: &Path, target: &Path) -> Result<()> {
    if let Some(parent) = source.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(source);
    std::os::unix::fs::symlink(target, source)?;
    add_altname_metadata(target, source);
    Ok(())
}

/// `Relocated#add_altname_metadata`: make the artifact searchable under the
/// name it had inside the container.
fn add_altname_metadata(file: &Path, source: &Path) {
    const ALT_NAME_ATTRIBUTE: &str = "com.apple.metadata:kMDItemAlternateNames";
    let altname = source.file_name().unwrap_or_default().to_string_lossy();
    let basename = file.file_name().unwrap_or_default().to_string_lossy();
    if altname.eq_ignore_ascii_case(basename.as_ref()) {
        return;
    }
    let existing = xattr::get(file, ALT_NAME_ATTRIBUTE)
        .ok()
        .flatten()
        .map(|v| String::from_utf8_lossy(&v).into_owned())
        .unwrap_or_default();
    let inner = existing
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or(&existing)
        .to_string();
    let mut value = inner;
    if !value.is_empty() {
        value.push_str(", ");
    }
    value.push_str(&format!("\"{altname}\""));
    let _ = xattr::set(file, ALT_NAME_ATTRIBUTE, format!("({value})").as_bytes());
}

/// `--adopt`: compare `CFBundleShortVersionString`/`CFBundleVersion`, falling
/// back to a recursive diff.
fn same_bundle(source: &Path, target: &Path) -> Result<bool> {
    let source_plist = source.join("Contents/Info.plist");
    let target_plist = target.join("Contents/Info.plist");
    if source_plist.is_file() && target_plist.is_file() {
        let source_version = bundle_version(&source_plist);
        let target_version = bundle_version(&target_plist);
        if let (Some(a), Some(b)) = (source_version, target_version) {
            if a.0 != b.0 {
                output::onoe(&format!(
                    "The bundle short version of {} is {} but is {} for {}!",
                    source.display(),
                    a.0.unwrap_or_default(),
                    b.0.unwrap_or_default(),
                    target.display()
                ));
                return Ok(false);
            }
            if a.1 != b.1 {
                output::onoe(&format!(
                    "The bundle version of {} is {} but is {} for {}!",
                    source.display(),
                    a.1.unwrap_or_default(),
                    b.1.unwrap_or_default(),
                    target.display()
                ));
                return Ok(false);
            }
            return Ok(true);
        }
    }
    let status = Command::new("/usr/bin/diff")
        .arg("--recursive")
        .arg("--brief")
        .arg(source)
        .arg(target)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    Ok(status.success())
}

/// `(CFBundleShortVersionString, CFBundleVersion)`.
fn bundle_version(plist_path: &Path) -> Option<(Option<String>, Option<String>)> {
    let value = plist::Value::from_file(plist_path).ok()?;
    let dict = value.as_dictionary()?;
    let read = |key: &str| {
        dict.get(key)
            .and_then(plist::Value::as_string)
            .map(str::to_string)
    };
    Some((read("CFBundleShortVersionString"), read("CFBundleVersion")))
}

/// `Moved#move_back`: restore the artifact into the staged directory, then
/// delete the target.
fn move_back(
    cfg: &Config,
    dirs: &CaskDirs,
    spec: &ArtifactSpec,
    ctx: &CaskContext,
    opts: ArtifactOptions,
    skip: bool,
) -> Result<()> {
    let name = english_name(&spec.kind);
    let source = artifact_source(ctx, spec)?;
    let target = artifact_target(cfg, dirs, ctx, spec)?;

    // The install left a symlink behind; remove it before restoring.
    if source.is_symlink()
        && std::fs::read_link(&source)
            .map(|t| source.parent().unwrap_or(Path::new("/")).join(&t) == target || t == target)
            .unwrap_or(false)
    {
        let _ = std::fs::remove_file(&source);
    }

    if path_occupied(&source) {
        let message = format!(
            "It seems there is already {} {name} at '{}'",
            english_article(&name),
            source.display()
        );
        if !opts.force && !opts.adopt {
            return Err(Error::user(format!("{message}.")));
        }
        output::opoo(&format!("{message}; overwriting."));
        super::unpack::remove_path(&source)?;
    }

    if !target.exists() && !target.is_symlink() {
        if skip || opts.force {
            return Ok(());
        }
        return Err(Error::user(format!(
            "It seems the {name} source '{}' is not there.",
            target.display()
        )));
    }

    output::ohai(&format!(
        "Backing up {name} '{}' to '{}'",
        target.file_name().unwrap_or_default().to_string_lossy(),
        source.display()
    ));
    if let Some(parent) = source.parent() {
        std::fs::create_dir_all(parent)?;
    }
    super::unpack::copy_path(&target, &source)?;
    delete_target(&target, &name)
}

fn delete_target(target: &Path, name: &str) -> Result<()> {
    output::ohai(&format!("Removing {name} '{}'", target.display()));
    if undeletable(target) {
        return Err(Error::user(format!("Cannot remove undeletable {name}.")));
    }
    if !path_occupied(target) {
        return Ok(());
    }
    super::unpack::remove_path(target)
}

/// `Utils.path_occupied?`.
fn path_occupied(path: &Path) -> bool {
    path.exists() || path.is_symlink()
}

/// `Moved#undeletable?`: the parent directory is not writable.
fn undeletable(target: &Path) -> bool {
    let parent = target.parent().unwrap_or(Path::new("/"));
    let Ok(c) = std::ffi::CString::new(parent.as_os_str().as_encoded_bytes()) else {
        return true;
    };
    // SAFETY: `c` is a valid NUL-terminated path.
    unsafe { libc::access(c.as_ptr(), libc::W_OK) != 0 }
}

// ------------------------------------------------- symlinked (binary, manpage, ..)

fn link_artifact(
    cfg: &Config,
    dirs: &CaskDirs,
    spec: &ArtifactSpec,
    ctx: &CaskContext,
    opts: ArtifactOptions,
) -> Result<()> {
    let name = english_name(&spec.kind);
    let source = artifact_source(ctx, spec)?;
    let target = artifact_target(cfg, dirs, ctx, spec)?;

    if !opts.dry_run && !path_occupied(&source) {
        return Err(Error::user(format!(
            "It seems the symlink source '{}' is not there.",
            source.display()
        )));
    }

    if path_occupied(&target) {
        let links_to_source = target.is_symlink()
            && (std::fs::read_link(&target)
                .map(|t| t == source)
                .unwrap_or(false)
                || std::fs::canonicalize(&target).ok() == std::fs::canonicalize(&source).ok());
        let owned_by_caskroom = target
            .canonicalize()
            .map(|p| p.starts_with(&ctx.caskroom_path))
            .unwrap_or(false);
        if links_to_source {
            output::ohai(&format!(
                "{name} '{}' is already linked to '{}'",
                source.file_name().unwrap_or_default().to_string_lossy(),
                target.display()
            ));
            return finish_link(spec, &source, opts);
        }
        if (opts.force || opts.adopt) && target.is_symlink() && owned_by_caskroom {
            output::opoo(&format!(
                "It seems there is already {} {name} at '{}'; overwriting.",
                english_article(&name),
                target.display()
            ));
            super::unpack::remove_path(&target)?;
        } else if let Some(formula) = conflicting_formula(cfg, &target) {
            output::opoo(&format!(
                "It seems there is already {} {name} at '{}' from formula {formula}; skipping link.",
                english_article(&name),
                target.display()
            ));
            return Ok(());
        } else if opts.force {
            output::opoo(&format!(
                "It seems there is already {} {name} at '{}'; overwriting.",
                english_article(&name),
                target.display()
            ));
            super::unpack::remove_path(&target)?;
        } else {
            return Err(Error::user(format!(
                "It seems there is already {} {name} at '{}'.",
                english_article(&name),
                target.display()
            )));
        }
    }

    if opts.dry_run {
        println!("{}", target.display());
        return Ok(());
    }

    output::ohai(&format!(
        "Linking {name} '{}' to '{}'",
        source.file_name().unwrap_or_default().to_string_lossy(),
        target.display()
    ));
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(&target);
    std::os::unix::fs::symlink(&source, &target)?;
    finish_link(spec, &source, opts)
}

/// `Binary#link`: make the staged executable executable.
fn finish_link(spec: &ArtifactSpec, source: &Path, opts: ArtifactOptions) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if opts.dry_run || !matches!(spec.kind.as_str(), "binary" | "command_wrapper") {
        return Ok(());
    }
    let Ok(metadata) = std::fs::metadata(source) else {
        return Ok(());
    };
    let mode = metadata.permissions().mode();
    if mode & 0o111 == 0 {
        let _ = std::fs::set_permissions(source, std::fs::Permissions::from_mode(mode | 0o755));
    }
    Ok(())
}

/// `Symlinked#conflicting_formula`: a symlink into the Cellar belongs to a formula.
fn conflicting_formula(cfg: &Config, target: &Path) -> Option<String> {
    if !target.is_symlink() {
        return None;
    }
    let resolved = std::fs::canonicalize(target).ok()?;
    let relative = resolved
        .strip_prefix(std::fs::canonicalize(&cfg.cellar).ok()?)
        .ok()?;
    relative
        .components()
        .next()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
}

fn unlink_artifact(
    cfg: &Config,
    dirs: &CaskDirs,
    spec: &ArtifactSpec,
    ctx: &CaskContext,
) -> Result<()> {
    let target = artifact_target(cfg, dirs, ctx, spec)?;
    if !target.is_symlink() {
        return Ok(());
    }
    if conflicting_formula(cfg, &target).is_some() {
        return Ok(());
    }
    output::ohai(&format!(
        "Unlinking {} '{}'",
        english_name(&spec.kind),
        target.display()
    ));
    let _ = std::fs::remove_file(&target);
    Ok(())
}

/// `CommandWrapper#install_phase`: write the wrapper script that gets linked.
fn write_command_wrapper(spec: &ArtifactSpec, ctx: &CaskContext) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let options = spec
        .args
        .iter()
        .filter_map(Value::as_object)
        .next()
        .cloned()
        .unwrap_or_default();
    let name = arg_str(spec, 0).unwrap_or_default();
    let source = ctx
        .staged_path
        .join(".homebrew-command-wrappers")
        .join(name);
    std::fs::create_dir_all(source.parent().unwrap_or(Path::new("/")))?;

    let script = if let Some(content) = options.get("content").and_then(Value::as_str) {
        content.to_string()
    } else {
        let executable = options
            .get("executable")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                Error::user("'command_wrapper' requires content or executable".to_string())
            })?;
        let args: Vec<String> = options
            .get("args")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(shell_escape)
                    .collect()
            })
            .unwrap_or_default();
        let env: Vec<String> = options
            .get("env")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .map(|(k, v)| {
                        format!(
                            "export {k}={}\n",
                            shell_escape(v.as_str().unwrap_or_default())
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        format!(
            "#!/bin/bash\n{}exec {} {} \"$@\"\n",
            env.concat(),
            shell_escape(executable),
            args.join(" ")
        )
    };
    std::fs::write(&source, script)?;
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

fn shell_escape(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@".contains(c))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// `GeneratedScript#install_phase`.
fn write_generated_script(spec: &ArtifactSpec, ctx: &CaskContext) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let path = arg_str(spec, 0)
        .ok_or_else(|| Error::user("'generated_script' requires a path".to_string()))?;
    let content = spec
        .args
        .iter()
        .filter_map(Value::as_object)
        .find_map(|m| m.get("content"))
        .and_then(Value::as_str)
        .ok_or_else(|| Error::user("'generated_script' requires content".to_string()))?;
    let target = ctx.staged_path.join(path);
    if !target.starts_with(&ctx.staged_path) {
        return Err(Error::user(
            "'generated_script' requires a path within the staged cask".to_string(),
        ));
    }
    std::fs::create_dir_all(target.parent().unwrap_or(Path::new("/")))?;
    std::fs::write(&target, content)?;
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

// ---------------------------------------------------------------- pkg

/// The exact `installer` invocation Homebrew builds for a `pkg` artifact.
///
/// Returns `(program, args)`; the caller prepends `sudo`.
pub fn pkg_command(
    pkg_path: &Path,
    choices_path: Option<&Path>,
    allow_untrusted: bool,
    verbose: bool,
) -> (String, Vec<String>) {
    let mut args = vec![
        "-pkg".to_string(),
        pkg_path.to_string_lossy().into_owned(),
        "-target".to_string(),
        "/".to_string(),
    ];
    if verbose {
        args.push("-verboseR".to_string());
    }
    if allow_untrusted {
        args.push("-allowUntrusted".to_string());
    }
    if let Some(choices) = choices_path {
        args.push("-applyChoiceChangesXML".to_string());
        args.push(choices.to_string_lossy().into_owned());
    }
    ("/usr/sbin/installer".to_string(), args)
}

/// Serialize a `pkg choices:` array as the XML plist `installer` expects.
pub fn choices_xml(choices: &Value) -> Result<String> {
    let value = json_to_plist(choices);
    let mut buffer: Vec<u8> = Vec::new();
    plist::to_writer_xml(&mut buffer, &value)
        .map_err(|e| Error::user(format!("Could not build the pkg choices XML: {e}")))?;
    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

fn json_to_plist(value: &Value) -> plist::Value {
    match value {
        Value::Null => plist::Value::String(String::new()),
        Value::Bool(b) => plist::Value::Boolean(*b),
        Value::Number(n) => match n.as_i64() {
            Some(i) => plist::Value::Integer(i.into()),
            None => plist::Value::Real(n.as_f64().unwrap_or_default()),
        },
        Value::String(s) => plist::Value::String(s.clone()),
        Value::Array(a) => plist::Value::Array(a.iter().map(json_to_plist).collect()),
        Value::Object(o) => {
            let mut dict = plist::Dictionary::new();
            for (k, v) in o {
                dict.insert(sym(k).to_string(), json_to_plist(v));
            }
            plist::Value::Dictionary(dict)
        }
    }
}

fn run_pkg(spec: &ArtifactSpec, ctx: &CaskContext, opts: ArtifactOptions) -> Result<()> {
    let relative =
        arg_str(spec, 0).ok_or_else(|| Error::user("'pkg' stanza requires a path.".to_string()))?;
    let path = ctx.staged_path.join(relative);
    let options = spec
        .args
        .iter()
        .filter_map(Value::as_object)
        .next()
        .cloned()
        .unwrap_or_default();

    output::ohai(&format!(
        "Running installer for {} with `sudo` (which may request your password)...",
        ctx.token
    ));
    if !path.exists() {
        return Err(Error::user(format!(
            "Could not find PKG source file '{relative}'."
        )));
    }

    let choices_file = match options.get("choices") {
        Some(choices) if !is_blank(choices) => {
            let mut file = tempfile::Builder::new()
                .prefix("choices")
                .suffix(".xml")
                .tempfile()?;
            std::io::Write::write_all(&mut file, choices_xml(choices)?.as_bytes())?;
            Some(file)
        }
        _ => None,
    };
    let allow_untrusted = options.get("allow_untrusted") == Some(&Value::Bool(true));
    let (program, args) = pkg_command(
        &path,
        choices_file.as_ref().map(|f| f.path()),
        allow_untrusted,
        opts.verbose,
    );
    if opts.dry_run {
        println!("sudo {program} {}", args.join(" "));
        return Ok(());
    }
    steps::run_tool(&program, &args, &[], true)
}

// ---------------------------------------------------------- installer

fn run_installer(
    cfg: &Config,
    spec: &ArtifactSpec,
    ctx: &CaskContext,
    opts: ArtifactOptions,
) -> Result<()> {
    let args = spec
        .args
        .iter()
        .filter_map(Value::as_object)
        .next()
        .cloned()
        .unwrap_or_default();

    if let Some(manual) = args.get("manual").and_then(Value::as_str) {
        println!(
            "Cask {} only provides a manual installer. To run it and complete the installation:\n  open {}",
            ctx.token,
            shell_escape(&ctx.staged_path.join(manual).to_string_lossy())
        );
        return Ok(());
    }

    let script = args
        .get("script")
        .ok_or_else(|| Error::user("'installer' stanza requires an argument.".to_string()))?;
    let (executable, script_args, sudo) = match script {
        Value::String(s) => (s.clone(), vec![], false),
        Value::Object(map) => (
            map.get("executable")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::user("installer missing executable".to_string()))?
                .to_string(),
            map.get("args")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            map.get("sudo") == Some(&Value::Bool(true)),
        ),
        _ => return Err(Error::user("invalid 'installer' stanza".to_string())),
    };

    output::ohai(&format!("Running installer script '{executable}'"));
    let path = staged_path_join_executable(ctx, &executable);
    if opts.dry_run {
        println!(
            "{}{} {}",
            if sudo { "sudo " } else { "" },
            path.display(),
            script_args.join(" ")
        );
        return Ok(());
    }
    let path_env = format!(
        "{}:{}:{}",
        cfg.prefix.join("bin").display(),
        cfg.prefix.join("sbin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut command = if sudo {
        let mut c = Command::new("/usr/bin/sudo");
        c.arg("-E").arg("--").arg(&path);
        c
    } else {
        Command::new(&path)
    };
    command.args(&script_args).env("PATH", path_env);
    let status = command.status()?;
    if !status.success() {
        return Err(Error::user(format!(
            "Failure while executing; `{}` exited with {}.",
            path.display(),
            status.code().unwrap_or(-1)
        )));
    }
    Ok(())
}

/// `AbstractArtifact#staged_path_join_executable`.
fn staged_path_join_executable(ctx: &CaskContext, path: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let candidate = PathBuf::from(path);
    let absolute = if candidate.is_absolute() {
        candidate.clone()
    } else {
        ctx.staged_path.join(&candidate)
    };
    if absolute.exists() {
        if let Ok(metadata) = std::fs::metadata(&absolute) {
            let mode = metadata.permissions().mode();
            if mode & 0o111 == 0 {
                let _ = std::fs::set_permissions(
                    &absolute,
                    std::fs::Permissions::from_mode(mode | 0o755),
                );
            }
        }
        return absolute;
    }
    candidate
}

// ------------------------------------------------------ declarative steps

fn step_context<'a>(
    cfg: &'a Config,
    dirs: &'a CaskDirs,
    ctx: &CaskContext,
    verbose: bool,
) -> StepContext<'a> {
    StepContext {
        cfg,
        dirs,
        token: ctx.token.clone(),
        name: ctx.name.clone(),
        version: ctx.version.clone(),
        arch: crate::platform::Host::detect().arch.as_str().to_string(),
        staged_path: ctx.staged_path.clone(),
        caskroom_path: ctx.caskroom_path.clone(),
        verbose,
    }
}

fn run_steps(
    cfg: &Config,
    dirs: &CaskDirs,
    spec: &ArtifactSpec,
    ctx: &CaskContext,
    phase: Phase,
) -> Result<()> {
    let steps = steps::steps_from_args(&spec.args);
    let step_ctx = step_context(cfg, dirs, ctx, false);
    steps::run(&step_ctx, &steps, phase)
}

/// `generate_completions_from_executable`: run the executable once per shell
/// and write the output where the matching completion artifact would link it.
fn generate_completions(
    cfg: &Config,
    dirs: &CaskDirs,
    spec: &ArtifactSpec,
    ctx: &CaskContext,
) -> Result<()> {
    let commands: Vec<String> = spec
        .args
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    let Some(first) = commands.first() else {
        return Err(Error::user(format!(
            "'generate_completions_from_executable' requires at least one command in {}",
            ctx.token
        )));
    };
    let options = spec
        .args
        .iter()
        .filter_map(Value::as_object)
        .next()
        .cloned()
        .unwrap_or_default();
    let shells: Vec<String> = options
        .get("shells")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(|s| sym(s).to_string())
                .collect()
        })
        .unwrap_or_else(|| vec!["bash".into(), "zsh".into(), "fish".into()]);
    let executable = staged_path_join_executable(ctx, first);
    let base_name = options
        .get("base_name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            executable
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| ctx.token.clone())
        });
    let format = options
        .get("shell_parameter_format")
        .and_then(Value::as_str)
        .map(|s| sym(s).to_string());

    for shell in shells {
        let Some(output_path) = completion_path(cfg, dirs, &shell, &base_name) else {
            continue;
        };
        let parameter = shell_parameter(format.as_deref(), &shell);
        let mut command = Command::new(&executable);
        command.args(&commands[1..]);
        if let Some(parameter) = parameter {
            command.arg(parameter);
        }
        command.env("SHELL", &shell);
        let result = command.output();
        match result {
            Ok(out) if out.status.success() => {
                if let Some(parent) = output_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&output_path, out.stdout)?;
            }
            _ => output::opoo(&format!(
                "Failed to generate {shell} completions from {}",
                executable.display()
            )),
        }
    }
    Ok(())
}

fn completion_path(cfg: &Config, dirs: &CaskDirs, shell: &str, base_name: &str) -> Option<PathBuf> {
    match shell {
        "bash" => Some(dirs.bash_completion.join(base_name)),
        "zsh" => Some(dirs.zsh_completion.join(format!("_{base_name}"))),
        "fish" => Some(dirs.fish_completion.join(format!("{base_name}.fish"))),
        "pwsh" => Some(
            cfg.prefix
                .join("share/pwsh/completions")
                .join(format!("_{base_name}.ps1")),
        ),
        _ => None,
    }
}

/// `Utils::ShellCompletion.completion_shell_parameter`.
fn shell_parameter(format: Option<&str>, shell: &str) -> Option<String> {
    match format {
        None => Some(shell.to_string()),
        Some("flag") => Some(format!("--{shell}")),
        Some("arg") => Some(format!("--shell={shell}")),
        Some("none") => None,
        Some("click") => Some(format!("{}_SOURCE", shell.to_uppercase())),
        Some(other) => Some(other.replace("{shell}", shell)),
    }
}

// ----------------------------------------------------------- uninstall

/// Reverse the artifacts of an API cask entry.
pub fn uninstall_artifacts(
    cfg: &Config,
    dirs: &CaskDirs,
    cask: &CaskEntry,
    staged: &Path,
    zap: bool,
    opts: ArtifactOptions,
) -> Result<()> {
    let mut ctx = CaskContext::from_entry(cfg, cask);
    ctx.staged_path = staged.to_path_buf();
    uninstall_specs(cfg, dirs, &artifact_specs(cfg, dirs, cask), &ctx, zap, opts)
}

/// Reverse `specs`; `zap` additionally dispatches the `zap` stanza.
pub fn uninstall_specs(
    cfg: &Config,
    dirs: &CaskDirs,
    specs: &[ArtifactSpec],
    ctx: &CaskContext,
    zap: bool,
    opts: ArtifactOptions,
) -> Result<()> {
    let mut failures: Vec<String> = Vec::new();
    for spec in specs {
        if let Err(error) = uninstall_one(cfg, dirs, spec, ctx, opts, true) {
            failures.push(error.to_string());
        }
    }
    if zap {
        let zaps: Vec<&ArtifactSpec> = specs.iter().filter(|s| s.kind == "zap").collect();
        if zaps.is_empty() {
            output::opoo(&format!("No zap stanza present for Cask '{}'", ctx.token));
        } else {
            output::ohai("Dispatching zap stanza");
            for spec in zaps {
                if let Err(error) = dispatch_uninstall(cfg, spec, ctx, opts, DispatchMode::Zap) {
                    failures.push(error.to_string());
                }
            }
        }
    }
    match failures.first() {
        Some(first) => Err(Error::user(first.clone())),
        None => Ok(()),
    }
}

fn uninstall_one(
    cfg: &Config,
    dirs: &CaskDirs,
    spec: &ArtifactSpec,
    ctx: &CaskContext,
    opts: ArtifactOptions,
    skip: bool,
) -> Result<()> {
    let kind = spec.kind.as_str();
    if MOVED_KINDS.contains(&kind) {
        return move_back(cfg, dirs, spec, ctx, opts, skip);
    }
    if SYMLINKED_KINDS.contains(&kind) {
        return unlink_artifact(cfg, dirs, spec, ctx);
    }
    match kind {
        "uninstall" => {
            dispatch_uninstall(cfg, spec, ctx, opts, DispatchMode::Uninstall)?;
            dispatch_uninstall(cfg, spec, ctx, opts, DispatchMode::PostUninstall)
        }
        "preflight_steps" | "postflight_steps" => run_steps(cfg, dirs, spec, ctx, Phase::Uninstall),
        "uninstall_preflight_steps" | "uninstall_postflight_steps" => {
            run_steps(cfg, dirs, spec, ctx, Phase::Install)
        }
        "generate_completions_from_executable" => {
            let options = spec
                .args
                .iter()
                .filter_map(Value::as_object)
                .next()
                .cloned()
                .unwrap_or_default();
            let shells: Vec<String> = options
                .get("shells")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(|s| sym(s).to_string())
                        .collect()
                })
                .unwrap_or_else(|| vec!["bash".into(), "zsh".into(), "fish".into()]);
            let base_name = options
                .get("base_name")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| ctx.token.clone());
            for shell in shells {
                if let Some(path) = completion_path(cfg, dirs, &shell, &base_name) {
                    let _ = std::fs::remove_file(path);
                }
            }
            Ok(())
        }
        // `zap` runs only under `--zap`, handled by the caller.
        _ => Ok(()),
    }
}

// -------------------------------------------- uninstall/zap directives

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DispatchMode {
    /// `Uninstall#uninstall_phase`: everything except `rmdir`.
    Uninstall,
    /// `Uninstall#post_uninstall_phase`: `rmdir` only.
    PostUninstall,
    /// `Zap#zap_phase`: every directive.
    Zap,
}

/// `AbstractUninstall::ORDERED_DIRECTIVES`.
pub const ORDERED_DIRECTIVES: &[&str] = &[
    "early_script",
    "launchctl",
    "quit",
    "signal",
    "login_item",
    "kext",
    "script",
    "pkgutil",
    "delete",
    "trash",
    "rmdir",
];

/// The directives of an `uninstall`/`zap` stanza, in Homebrew's run order.
pub fn ordered_directives(spec: &ArtifactSpec) -> Vec<(String, Value)> {
    let Some(map) = spec.args.iter().filter_map(Value::as_object).next() else {
        return vec![];
    };
    ORDERED_DIRECTIVES
        .iter()
        .filter_map(|name| {
            map.get(*name)
                .map(|value| ((*name).to_string(), value.clone()))
        })
        .collect()
}

fn dispatch_uninstall(
    cfg: &Config,
    spec: &ArtifactSpec,
    ctx: &CaskContext,
    opts: ArtifactOptions,
    mode: DispatchMode,
) -> Result<()> {
    for (name, value) in ordered_directives(spec) {
        let wanted = match mode {
            DispatchMode::Uninstall => name != "rmdir",
            DispatchMode::PostUninstall => name == "rmdir",
            DispatchMode::Zap => true,
        };
        if !wanted {
            continue;
        }
        run_directive(cfg, &name, &value, ctx, opts)?;
    }
    Ok(())
}

fn as_list(value: &Value) -> Vec<Value> {
    match value {
        Value::Array(items) => items.clone(),
        Value::Null => vec![],
        other => vec![other.clone()],
    }
}

fn run_directive(
    cfg: &Config,
    name: &str,
    value: &Value,
    ctx: &CaskContext,
    opts: ArtifactOptions,
) -> Result<()> {
    match name {
        "early_script" | "script" => run_uninstall_script(cfg, value, ctx, opts),
        "launchctl" => {
            for service in as_list(value).iter().filter_map(Value::as_str) {
                output::ohai(&format!("Removing launchctl service {service}"));
                if opts.dry_run {
                    continue;
                }
                let _ = Command::new("/bin/launchctl")
                    .args(["remove", service])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                for dir in [
                    cfg.home.join("Library/LaunchAgents"),
                    PathBuf::from("/Library/LaunchAgents"),
                    PathBuf::from("/Library/LaunchDaemons"),
                ] {
                    let plist = dir.join(format!("{service}.plist"));
                    if plist.exists() {
                        let _ = std::fs::remove_file(plist);
                    }
                }
            }
            Ok(())
        }
        "quit" => {
            for bundle_id in as_list(value).iter().filter_map(Value::as_str) {
                quit_application(bundle_id, opts);
            }
            Ok(())
        }
        "signal" => {
            for pair in signal_pairs(value) {
                send_signal(&pair.0, &pair.1, opts);
            }
            Ok(())
        }
        "login_item" => {
            for item in as_list(value).iter().filter_map(Value::as_str) {
                output::ohai(&format!("Removing login item {item}"));
                if opts.dry_run {
                    continue;
                }
                let script = format!(
                    "tell application \"System Events\" to delete every login item whose name is \"{}\"",
                    item.replace('"', "\\\"")
                );
                let _ = Command::new("osascript")
                    .args(["-e", &script])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            Ok(())
        }
        "kext" => {
            for kext in as_list(value).iter().filter_map(Value::as_str) {
                output::ohai(&format!("Unloading kernel extension {kext}"));
                if opts.dry_run {
                    continue;
                }
                let _ = steps::run_tool(
                    "/sbin/kextunload",
                    &["-b".to_string(), kext.to_string()],
                    &[],
                    true,
                );
            }
            Ok(())
        }
        "pkgutil" => {
            output::ohai("Uninstalling packages with `sudo` (which may request your password)...");
            for pattern in as_list(value).iter().filter_map(Value::as_str) {
                for package_id in pkgutil_packages(pattern) {
                    println!("{package_id}");
                    if opts.dry_run {
                        continue;
                    }
                    forget_package(&package_id)?;
                }
            }
            Ok(())
        }
        "delete" => {
            let paths = resolve_directive_paths(cfg, "delete", value);
            if paths.is_empty() {
                return Ok(());
            }
            output::ohai("Removing files:");
            for path in paths {
                println!("{}", printable(cfg, &path));
                if !opts.dry_run {
                    let _ = super::unpack::remove_path(&path);
                }
            }
            Ok(())
        }
        "trash" => {
            let paths = resolve_directive_paths(cfg, "trash", value);
            if paths.is_empty() {
                return Ok(());
            }
            output::ohai("Trashing files:");
            for path in &paths {
                println!("{}", printable(cfg, path));
            }
            if opts.dry_run {
                return Ok(());
            }
            let untrashable = trash_paths(cfg, &paths);
            if !untrashable.is_empty() {
                output::opoo("The following files could not be trashed, please do so manually:");
                for path in untrashable {
                    eprintln!("{}", path.display());
                }
            }
            Ok(())
        }
        "rmdir" => {
            let paths = resolve_directive_paths(cfg, "rmdir", value);
            if paths.is_empty() {
                return Ok(());
            }
            output::ohai("Removing directories if empty:");
            for path in paths {
                println!("{}", printable(cfg, &path));
                if !opts.dry_run {
                    recursive_rmdir(&path);
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// `directives[:signal] = Array(directives[:signal]).flatten.each_slice(2)`.
fn signal_pairs(value: &Value) -> Vec<(String, String)> {
    let mut flat: Vec<String> = Vec::new();
    fn flatten(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::Array(items) => items.iter().for_each(|v| flatten(v, out)),
            Value::String(s) => out.push(s.clone()),
            _ => {}
        }
    }
    flatten(value, &mut flat);
    flat.chunks(2)
        .filter(|c| c.len() == 2)
        .map(|c| (c[0].clone(), c[1].clone()))
        .collect()
}

/// `AbstractUninstall#each_resolved_path`: expand `~`, refuse relative paths
/// and paths with `.`/`..` segments, then glob.
pub fn resolve_directive_paths(cfg: &Config, action: &str, value: &Value) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for path in as_list(value).iter().filter_map(Value::as_str) {
        let resolved = if path == "~" {
            cfg.home.clone()
        } else if let Some(rest) = path.strip_prefix("~/") {
            cfg.home.join(rest)
        } else {
            PathBuf::from(path)
        };
        if !resolved.is_absolute() {
            output::opoo(&format!("Skipping {action} for relative path '{path}'."));
            continue;
        }
        if resolved
            .components()
            .any(|c| matches!(c.as_os_str().to_str(), Some(".") | Some("..")))
        {
            output::opoo(&format!(
                "Skipping {action} for path with relative segments '{path}'."
            ));
            continue;
        }
        let text = resolved.to_string_lossy().into_owned();
        if text.contains(['*', '?', '[']) {
            if let Ok(paths) = glob::glob(&text) {
                out.extend(paths.flatten());
            }
        } else if resolved.exists() || resolved.is_symlink() {
            out.push(resolved);
        }
    }
    out
}

fn run_uninstall_script(
    cfg: &Config,
    value: &Value,
    ctx: &CaskContext,
    opts: ArtifactOptions,
) -> Result<()> {
    let (executable, args, sudo) = match value {
        Value::String(s) => (s.clone(), vec![], false),
        Value::Object(map) => (
            map.get("executable")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::user("uninstall :script without :executable.".to_string()))?
                .to_string(),
            map.get("args")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            map.get("sudo") == Some(&Value::Bool(true)),
        ),
        _ => return Ok(()),
    };

    output::ohai(&format!("Running uninstall script {executable}"));
    let path = staged_path_join_executable(ctx, &executable);
    if !path.exists() {
        let message = format!("uninstall script {executable} does not exist");
        if !opts.force {
            return Err(Error::user(format!("{message}.")));
        }
        output::opoo(&format!("{message}; skipping."));
        return Ok(());
    }
    if opts.dry_run {
        println!("{}{}", if sudo { "sudo " } else { "" }, path.display());
        return Ok(());
    }
    let path_env = format!(
        "{}:{}:{}",
        cfg.prefix.join("bin").display(),
        cfg.prefix.join("sbin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut command = if sudo {
        let mut c = Command::new("/usr/bin/sudo");
        c.arg("-E").arg("--").arg(&path);
        c
    } else {
        Command::new(&path)
    };
    command.args(&args).env("PATH", path_env);
    let status = command.status()?;
    if !status.success() {
        return Err(Error::user(format!(
            "Failure while executing; `{}` exited with {}.",
            path.display(),
            status.code().unwrap_or(-1)
        )));
    }
    Ok(())
}

fn quit_application(bundle_id: &str, opts: ArtifactOptions) {
    output::ohai(&format!("Quitting application '{bundle_id}'..."));
    if opts.dry_run {
        return;
    }
    let script = format!("tell application id \"{bundle_id}\" to quit");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let ok = Command::new("osascript")
            .args(["-e", &script])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok && !application_running(bundle_id) {
            println!("Application '{bundle_id}' quit successfully.");
            return;
        }
        if std::time::Instant::now() >= deadline {
            output::opoo(&format!(
                "Application '{bundle_id}' did not quit. Enable Automation access for \"Terminal → System Events\" in:\n  System Settings → Privacy & Security → Automation\nif you haven't already."
            ));
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

fn application_running(bundle_id: &str) -> bool {
    let script = "'use strict';\nObjC.import('stdlib')\nfunction run(argv) {\n  try {\n    var app = Application(argv[0])\n    if (app.running()) { $.exit(0) }\n  } catch (err) { }\n  $.exit(1)\n}\n";
    Command::new("osascript")
        .args(["-l", "JavaScript", "-e", script, bundle_id])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn send_signal(signal: &str, bundle_id: &str, opts: ArtifactOptions) {
    output::ohai(&format!(
        "Signalling '{signal}' to application ID '{bundle_id}'"
    ));
    if opts.dry_run {
        return;
    }
    for pid in running_pids(bundle_id) {
        let _ = Command::new("/bin/kill")
            .arg(format!("-{signal}"))
            .arg(pid.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// `AbstractUninstall#running_processes`: `launchctl list` entries whose label
/// matches the bundle id.
fn running_pids(bundle_id: &str) -> Vec<i32> {
    let Ok(out) = Command::new("/bin/launchctl").arg("list").output() else {
        return vec![];
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let pid: i32 = fields.next()?.trim().parse().ok()?;
            let _state = fields.next()?;
            let label = fields.next()?.trim();
            let base = label.strip_prefix("application.").unwrap_or(label);
            let matches = base == bundle_id
                || base.strip_prefix(bundle_id).is_some_and(|rest| {
                    rest.split('.')
                        .all(|p| p.is_empty() || p.chars().all(|c| c.is_ascii_digit()))
                });
            (pid != 0 && matches).then_some(pid)
        })
        .collect()
}

fn pkgutil_packages(pattern: &str) -> Vec<String> {
    Command::new("/usr/sbin/pkgutil")
        .arg(format!("--pkgs={pattern}"))
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// `Cask::Pkg#uninstall`: delete the package's files, then forget the receipt.
fn forget_package(package_id: &str) -> Result<()> {
    let info = Command::new("/usr/sbin/pkgutil")
        .args(["--pkg-info-plist", package_id])
        .output()?;
    let root = plist::Value::from_reader_xml(std::io::Cursor::new(&info.stdout))
        .ok()
        .and_then(|v| {
            let dict = v.as_dictionary()?.clone();
            let volume = dict.get("volume")?.as_string()?.to_string();
            let location = dict.get("install-location")?.as_string()?.to_string();
            Some(PathBuf::from(volume).join(location.trim_start_matches('/')))
        })
        .unwrap_or_else(|| PathBuf::from("/"));

    let files = Command::new("/usr/sbin/pkgutil")
        .args(["--files", package_id])
        .output()?;
    let mut paths: Vec<PathBuf> = String::from_utf8_lossy(&files.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| root.join(l))
        .collect();
    // Deepest first so directories empty out before they are removed.
    paths.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    for path in paths {
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let args = if metadata.is_dir() && !metadata.file_type().is_symlink() {
            vec!["-d".to_string()]
        } else {
            vec!["-f".to_string()]
        };
        let _ = steps::run_tool("/bin/rm", &args, std::slice::from_ref(&path), true);
    }
    steps::run_tool(
        "/usr/sbin/pkgutil",
        &["--forget".to_string(), package_id.to_string()],
        &[],
        true,
    )
}

/// `Cask::Utils::Trash.trash`: move into the user's Trash, uniquifying the name.
/// Returns the paths that could not be trashed.
pub fn trash_paths(cfg: &Config, paths: &[PathBuf]) -> Vec<PathBuf> {
    let trash = cfg.home.join(".Trash");
    let mut failed = Vec::new();
    if std::fs::create_dir_all(&trash).is_err() {
        return paths.to_vec();
    }
    for path in paths {
        let basename = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let mut destination = trash.join(&basename);
        let mut suffix = 1;
        while destination.exists() || destination.is_symlink() {
            destination = trash.join(format!("{basename} {suffix}"));
            suffix += 1;
            if suffix > 1000 {
                break;
            }
        }
        if super::unpack::move_path(path, &destination).is_err() {
            failed.push(path.clone());
        }
    }
    failed
}

/// `AbstractUninstall#recursive_rmdir`: remove a directory tree that holds
/// nothing but directories and `.DS_Store` files.
fn recursive_rmdir(path: &Path) -> bool {
    if path.is_symlink() || !path.is_dir() {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return false;
    };
    let children: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    let ds_store = path.join(".DS_Store");
    for child in &children {
        if *child == ds_store {
            continue;
        }
        if !child.is_dir() || child.is_symlink() {
            return false;
        }
    }
    for child in &children {
        if *child == ds_store {
            let _ = std::fs::remove_file(child);
            continue;
        }
        if !recursive_rmdir(child) {
            return false;
        }
    }
    std::fs::remove_dir(path).is_ok()
}

// --------------------------------------------------------------- receipt

pub fn uninstall_artifacts_json(
    cfg: &Config,
    dirs: &CaskDirs,
    cask: &CaskEntry,
    _staged: &Path,
) -> Vec<Value> {
    specs_to_json(&artifact_specs(cfg, dirs, cask))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Config, CaskDirs) {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = crate::cask::tests_support::config(tmp.path());
        cfg.cask_opts = vec![format!(
            "--appdir={}/home/Applications",
            tmp.path().display()
        )];
        let dirs = CaskDirs::resolve(&cfg, &[]);
        (tmp, cfg, dirs)
    }

    fn ghostty() -> CaskEntry {
        let mut cask: CaskEntry = serde_json::from_value(serde_json::json!({
            "version": "1.3.1",
            "tap_string": "homebrew/cask",
            "url_args": ["https://release.files.ghostty.org/1.3.1/Ghostty.dmg"],
            "sha256": "abc",
            "raw_artifacts": [
                [":app", ["Ghostty.app"]],
                [":manpage", ["$APPDIR/Ghostty.app/Contents/Resources/man/man1/ghostty.1"]],
                [":bash_completion", ["$APPDIR/Ghostty.app/Contents/Resources/bash-completion/completions/ghostty.bash"]],
                [":zsh_completion", ["$APPDIR/Ghostty.app/Contents/Resources/zsh/site-functions/_ghostty"]],
                [":binary", ["$APPDIR/Ghostty.app/Contents/MacOS/ghostty"]],
                [":zap", {":trash": ["~/.config/ghostty"]}]
            ]
        }))
        .unwrap();
        cask.token = "ghostty".into();
        cask
    }

    #[test]
    fn sorts_artifacts_like_homebrew() {
        let (_tmp, cfg, dirs) = fixture();
        let specs = artifact_specs(&cfg, &dirs, &ghostty());
        let kinds: Vec<&str> = specs.iter().map(|s| s.kind.as_str()).collect();
        assert_eq!(
            kinds,
            [
                "app",
                "binary",
                "manpage",
                "bash_completion",
                "zsh_completion",
                "zap"
            ]
        );
    }

    #[test]
    fn expands_appdir_and_home_placeholders() {
        let (_tmp, cfg, dirs) = fixture();
        let specs = artifact_specs(&cfg, &dirs, &ghostty());
        let binary = specs.iter().find(|s| s.kind == "binary").unwrap();
        assert_eq!(
            binary.args[0].as_str().unwrap(),
            format!(
                "{}/Ghostty.app/Contents/MacOS/ghostty",
                dirs.appdir.display()
            )
        );
    }

    #[test]
    fn resolves_targets() {
        let (_tmp, cfg, dirs) = fixture();
        let cask = ghostty();
        let ctx = CaskContext::from_entry(&cfg, &cask);
        let specs = artifact_specs(&cfg, &dirs, &cask);
        let target = |kind: &str| {
            let spec = specs.iter().find(|s| s.kind == kind).unwrap();
            artifact_target(&cfg, &dirs, &ctx, spec).unwrap()
        };
        assert_eq!(target("app"), dirs.appdir.join("Ghostty.app"));
        assert_eq!(target("binary"), cfg.prefix.join("bin/ghostty"));
        assert_eq!(
            target("manpage"),
            cfg.prefix.join("share/man/man1/ghostty.1")
        );
        assert_eq!(
            target("bash_completion"),
            cfg.prefix.join("etc/bash_completion.d/ghostty")
        );
        assert_eq!(
            target("zsh_completion"),
            cfg.prefix.join("share/zsh/site-functions/_ghostty")
        );

        // `$APPDIR`-relative sources resolve outside the staged directory.
        let spec = specs.iter().find(|s| s.kind == "binary").unwrap();
        assert_eq!(
            artifact_source(&ctx, spec).unwrap(),
            dirs.appdir.join("Ghostty.app/Contents/MacOS/ghostty")
        );
    }

    #[test]
    fn target_option_and_tilde_targets() {
        let (_tmp, cfg, dirs) = fixture();
        let mut cask: CaskEntry = serde_json::from_value(serde_json::json!({
            "version": "1.0",
            "raw_artifacts": [
                [":app", ["Src.app", {":target": "Renamed.app"}]],
                [":artifact", ["thing", {":target": "~/Library/Thing"}]],
                [":artifact", ["abs", {":target": "/Library/Abs"}]]
            ]
        }))
        .unwrap();
        cask.token = "demo".into();
        let ctx = CaskContext::from_entry(&cfg, &cask);
        let specs = artifact_specs(&cfg, &dirs, &cask);
        assert_eq!(
            artifact_target(&cfg, &dirs, &ctx, &specs[0]).unwrap(),
            dirs.appdir.join("Renamed.app")
        );
        assert_eq!(
            artifact_target(&cfg, &dirs, &ctx, &specs[1]).unwrap(),
            cfg.home.join("Library/Thing")
        );
        assert_eq!(
            artifact_target(&cfg, &dirs, &ctx, &specs[2]).unwrap(),
            PathBuf::from("/Library/Abs")
        );
    }

    #[test]
    fn receipt_artifacts_round_trip() {
        let (_tmp, cfg, dirs) = fixture();
        let cask = ghostty();
        let json = uninstall_artifacts_json(&cfg, &dirs, &cask, Path::new("/tmp"));
        assert_eq!(json[0], serde_json::json!({"app": ["Ghostty.app"]}));
        assert_eq!(
            json[1],
            serde_json::json!({"binary": [format!("{}/Ghostty.app/Contents/MacOS/ghostty", dirs.appdir.display())]})
        );
        assert_eq!(
            json.last().unwrap(),
            &serde_json::json!({"zap": [{"trash": ["~/.config/ghostty"]}]})
        );

        // Reconstructing from the receipt gives the same specs back.
        let from_receipt = artifact_specs_from_receipt(&cfg, &dirs, &json);
        let direct = artifact_specs(&cfg, &dirs, &cask);
        assert_eq!(from_receipt.len(), direct.len());
        for (a, b) in from_receipt.iter().zip(direct.iter()) {
            assert_eq!(a.kind, b.kind);
        }
    }

    #[test]
    fn uninstall_directive_ordering() {
        let (_tmp, cfg, dirs) = fixture();
        let mut cask: CaskEntry = serde_json::from_value(serde_json::json!({
            "version": "1.0",
            "raw_artifacts": [[":uninstall", {
                ":rmdir": ["/tmp/x"],
                ":delete": ["/tmp/y"],
                ":quit": "com.example.app",
                ":launchctl": ["com.example.agent"],
                ":pkgutil": "com.example.pkg",
                ":script": {":executable": "u.sh"},
                ":signal": [["TERM", "com.example.app"]],
                ":early_script": {":executable": "e.sh"},
                ":trash": ["/tmp/z"],
                ":login_item": "Example",
                ":kext": "com.example.kext"
            }]]
        }))
        .unwrap();
        cask.token = "demo".into();
        let specs = artifact_specs(&cfg, &dirs, &cask);
        let names: Vec<String> = ordered_directives(&specs[0])
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(names, ORDERED_DIRECTIVES);
    }

    #[test]
    fn signal_pairs_flatten() {
        assert_eq!(
            signal_pairs(&serde_json::json!([["TERM", "com.a"], ["KILL", "com.b"]])),
            vec![
                ("TERM".to_string(), "com.a".to_string()),
                ("KILL".to_string(), "com.b".to_string())
            ]
        );
        assert_eq!(
            signal_pairs(&serde_json::json!(["TERM", "com.a"])),
            vec![("TERM".to_string(), "com.a".to_string())]
        );
    }

    #[test]
    fn pkg_command_construction() {
        let (program, args) =
            pkg_command(Path::new("/Caskroom/demo/1.0/Demo.pkg"), None, false, false);
        assert_eq!(program, "/usr/sbin/installer");
        assert_eq!(
            args,
            ["-pkg", "/Caskroom/demo/1.0/Demo.pkg", "-target", "/"]
        );

        let (_, args) = pkg_command(
            Path::new("/Caskroom/demo/1.0/Demo.pkg"),
            Some(Path::new("/tmp/choices.xml")),
            true,
            true,
        );
        assert_eq!(
            args,
            [
                "-pkg",
                "/Caskroom/demo/1.0/Demo.pkg",
                "-target",
                "/",
                "-verboseR",
                "-allowUntrusted",
                "-applyChoiceChangesXML",
                "/tmp/choices.xml"
            ]
        );
    }

    #[test]
    fn choices_xml_is_a_plist_array() {
        let xml = choices_xml(&serde_json::json!([
            {":choiceIdentifier": "org.example.x", ":choiceAttribute": "selected", ":attributeSetting": 1}
        ]))
        .unwrap();
        assert!(xml.contains("<array>"));
        assert!(xml.contains("<key>choiceIdentifier</key>"));
        assert!(xml.contains("<string>org.example.x</string>"));
        assert!(xml.contains("<integer>1</integer>"));
    }

    #[test]
    fn manpage_sections() {
        assert_eq!(manpage_section("ghostty.1"), Some('1'));
        assert_eq!(manpage_section("a/b/ghostty.5.gz"), Some('5'));
        assert_eq!(manpage_section("foo.n"), Some('n'));
        assert_eq!(manpage_section("foo.txt"), None);
    }

    #[test]
    fn moves_an_app_and_leaves_a_symlink() {
        let (_tmp, cfg, dirs) = fixture();
        let mut cask: CaskEntry = serde_json::from_value(serde_json::json!({
            "version": "1.0",
            "raw_artifacts": [[":app", ["Demo.app"]]]
        }))
        .unwrap();
        cask.token = "demo".into();
        let ctx = CaskContext::from_entry(&cfg, &cask);
        std::fs::create_dir_all(ctx.staged_path.join("Demo.app/Contents")).unwrap();
        std::fs::write(ctx.staged_path.join("Demo.app/Contents/Info.plist"), b"x").unwrap();
        std::fs::create_dir_all(&dirs.appdir).unwrap();

        let specs = artifact_specs(&cfg, &dirs, &cask);
        install_specs(&cfg, &dirs, &specs, &ctx, ArtifactOptions::default()).unwrap();
        assert!(dirs.appdir.join("Demo.app/Contents/Info.plist").is_file());
        assert!(ctx.staged_path.join("Demo.app").is_symlink());
        assert_eq!(
            std::fs::read_link(ctx.staged_path.join("Demo.app")).unwrap(),
            dirs.appdir.join("Demo.app")
        );

        // Installing again over the moved app is refused without `--force`.
        let err = move_artifact(&cfg, &dirs, &specs[0], &ctx, ArtifactOptions::default())
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            format!(
                "It seems there is already an App at '{}'.",
                dirs.appdir.join("Demo.app").display()
            )
        );

        uninstall_specs(&cfg, &dirs, &specs, &ctx, false, ArtifactOptions::default()).unwrap();
        assert!(!dirs.appdir.join("Demo.app").exists());
        assert!(
            ctx.staged_path
                .join("Demo.app/Contents/Info.plist")
                .is_file()
        );
    }

    #[test]
    fn already_present_app_errors_without_force() {
        let (_tmp, cfg, dirs) = fixture();
        let mut cask: CaskEntry = serde_json::from_value(serde_json::json!({
            "version": "1.0",
            "raw_artifacts": [[":app", ["Demo.app"]]]
        }))
        .unwrap();
        cask.token = "demo".into();
        let ctx = CaskContext::from_entry(&cfg, &cask);
        std::fs::create_dir_all(ctx.staged_path.join("Demo.app")).unwrap();
        std::fs::create_dir_all(dirs.appdir.join("Demo.app")).unwrap();

        let specs = artifact_specs(&cfg, &dirs, &cask);
        let err = install_specs(&cfg, &dirs, &specs, &ctx, ArtifactOptions::default())
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            format!(
                "It seems there is already an App at '{}'.",
                dirs.appdir.join("Demo.app").display()
            )
        );

        // `--force` overwrites.
        let opts = ArtifactOptions {
            force: true,
            ..ArtifactOptions::default()
        };
        install_specs(&cfg, &dirs, &specs, &ctx, opts).unwrap();
        assert!(ctx.staged_path.join("Demo.app").is_symlink());
    }
}
