//! Read-only commands: `info`, `search`, `desc`, `home`, `list`, `deps`,
//! `uses`, `leaves`, `outdated`, `missing`, `options`, `which-formula`.
//!
//! Output follows `docs/COMPAT.md` 9 and the corresponding `cmd/*.rb`.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::api::index::{DescScope, Index, SearchQuery};
use crate::config::Config;
use crate::deps::{self, DepOptions};
use crate::error::{Error, Result};
use crate::keg::{self, Keg};
use crate::model::{CaskEntry, FormulaEntry};
use crate::ops::outdated as outdated_ops;
use crate::output;
use crate::resolve::{self, Kind};

use super::commands::*;
use super::fmt;
use super::{Ctx, misc};

fn kind_of(formula: bool, cask: bool) -> Kind {
    match (formula, cask) {
        (true, false) => Kind::Formula,
        (false, true) => Kind::Cask,
        _ => Kind::Any,
    }
}

fn dep_options(
    include_build: bool,
    include_test: bool,
    include_optional: bool,
    include_implicit: bool,
    skip_recommended: bool,
) -> DepOptions {
    DepOptions {
        include_build,
        include_test,
        include_optional,
        include_implicit,
        skip_recommended,
    }
}

// ---------------------------------------------------------------------------
// info
// ---------------------------------------------------------------------------

pub fn info(ctx: &Ctx, args: &InfoArgs) -> Result<()> {
    let index = ctx.index()?;
    if let Some(version) = &args.json {
        return info_json(ctx, args, version);
    }
    if args.github {
        for name in &args.names {
            let formula = resolve::resolve_formula(&ctx.cfg, index, name)?;
            misc::open_url(&github_url(&ctx.cfg, &formula))?;
        }
        return Ok(());
    }

    if args.installed {
        let mut first = true;
        for name in keg::installed_formula_names(&ctx.cfg) {
            if !first {
                println!();
            }
            first = false;
            match index.formula(&name) {
                Some(f) => print_formula_info(ctx, &f)?,
                None => {
                    let f = resolve::resolve_formula(&ctx.cfg, index, &name)?;
                    print_formula_info(ctx, &f)?;
                }
            }
        }
        for token in deps::installed_cask_tokens(&ctx.cfg) {
            if !first {
                println!();
            }
            first = false;
            let cask = resolve::resolve_cask(&ctx.cfg, index, &token)?;
            print_cask_info(ctx, &cask);
        }
        return Ok(());
    }

    if args.names.is_empty() {
        return print_statistics(ctx);
    }

    let kind = kind_of(args.formula, args.cask);
    for (i, name) in args.names.iter().enumerate() {
        if i > 0 {
            println!();
        }
        match resolve::resolve(&ctx.cfg, index, name, kind)? {
            resolve::Resolved::Formula(f) => print_formula_info(ctx, &f)?,
            resolve::Resolved::Cask(c) => print_cask_info(ctx, &c),
        }
    }
    Ok(())
}

/// `brew info` with no arguments: keg count and Cellar size.
fn print_statistics(ctx: &Ctx) -> Result<()> {
    let names = keg::installed_formula_names(&ctx.cfg);
    let count: usize = names
        .iter()
        .map(|n| keg::installed_kegs(&ctx.cfg, n).len())
        .sum();
    let (files, bytes) = keg::disk_usage(&ctx.cfg.cellar);
    println!(
        "{}, {}",
        output::plural(count as u64, "keg"),
        fmt::abv(files, bytes)
    );
    Ok(())
}

/// `Info#github_remote_path`: a GitHub remote becomes a `blob/HEAD` URL,
/// anything else is joined as-is.
fn github_remote_path(remote: &str, path: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"^(?:https?://|git(?:@|://))github\.com[:/](.+)/(.+?)(?:\.git)?$")
            .expect("static regex")
    });
    match re.captures(remote) {
        Some(c) => format!("https://github.com/{}/{}/blob/HEAD/{path}", &c[1], &c[2]),
        None => format!("{remote}/{path}"),
    }
}

/// `Info#github_info`: the formula file on its tap's remote.
fn github_url(cfg: &Config, formula: &FormulaEntry) -> String {
    let tap_name = if formula.tap.is_empty() {
        "homebrew/core"
    } else {
        formula.tap.as_str()
    };
    let remote = match crate::tap::Tap::parse(tap_name) {
        Some(t) => t.remote(cfg).unwrap_or_else(|| t.default_remote()),
        None => "https://github.com/Homebrew/homebrew-core".to_string(),
    };
    github_remote_path(&remote, &formula.ruby_path())
}

/// Title spec list: `stable <version> (bottled)`, plus `HEAD` when a head spec
/// exists; an outdated install rewrites the first spec as `<installed> → <spec>`.
fn title_specs(formula: &FormulaEntry, installed_version: Option<&str>) -> String {
    let mut specs: Vec<String> = Vec::new();
    if formula.stable_version.is_some() {
        let mut s = format!("stable {}", formula.pkg_version());
        if formula.has_bottle() {
            s.push_str(" (bottled)");
        }
        specs.push(s);
    }
    if !formula.head_url_args.is_empty() {
        specs.push("HEAD".to_string());
    }
    if let Some(installed) = installed_version
        && let Some(first) = specs.first_mut()
    {
        *first = format!("{installed} → {first}");
    }
    specs.join(", ")
}

fn deprecation_message(formula: &FormulaEntry) -> Option<String> {
    let (kind, info) = if let Some(d) = formula.disablement() {
        ("Disabled", d)
    } else {
        ("Deprecated", formula.deprecation()?)
    };
    let mut msg = match info.because.as_deref() {
        Some(reason) => format!("{kind} because it {}!", humanize_reason(reason)),
        None => format!("{kind}!"),
    };
    if let Some(date) = info.date.as_deref() {
        if kind == "Disabled" {
            msg.push_str(&format!(" It was disabled on {date}."));
        } else {
            msg.push_str(&format!(" It will be disabled on {date}."));
        }
    }
    if let Some(replacement) = info.replacement_formula.or(info.replacement_cask) {
        msg.push_str(&format!("\nReplacement:\n  brew install {replacement}"));
    }
    Some(msg)
}

/// `DeprecateDisable::FORMULA_DEPRECATE_DISABLE_REASONS`: symbols become
/// sentences, free text is used verbatim.
fn humanize_reason(reason: &str) -> String {
    match reason {
        "does_not_build" => "does not build".into(),
        "no_license" => "has no license".into(),
        "repo_archived" => "has an archived upstream repository".into(),
        "repo_removed" => "has a removed upstream repository".into(),
        "unmaintained" => "is not maintained upstream".into(),
        "unsupported" => "is not supported upstream".into(),
        "deprecated_upstream" => "is deprecated upstream".into(),
        "versioned_formula" => "is a versioned formula".into(),
        "checksum_mismatch" => {
            "was built with an initially released source file that had a different checksum than the current one. \
             Upstream's repository might have been compromised. \
             We can create a new version or bottle for this formula if the upstream checksum change is intentional"
                .into()
        }
        other => other.to_string(),
    }
}

pub fn print_formula_info(ctx: &Ctx, formula: &FormulaEntry) -> Result<()> {
    let cfg = &ctx.cfg;
    let index = ctx.index()?;
    let kegs = keg::installed_kegs(cfg, &formula.name);
    let installed = !kegs.is_empty();
    let outdated = !outdated_ops::outdated_kegs(cfg, index, &formula.name).is_empty();
    let installed_version = if installed && outdated {
        keg::linked_keg(cfg, &formula.name)
            .map(|k| k.version.to_string())
            .or_else(|| kegs.last().map(|k| k.version.to_string()))
    } else {
        None
    };

    let mut attrs: Vec<&str> = Vec::new();
    if formula.is_keg_only() {
        attrs.push("keg-only");
    }
    let name = fmt::install_status(&formula.full_name(), installed, false);
    let attr_suffix = if attrs.is_empty() {
        String::new()
    } else {
        format!(" [{}]", attrs.join(", "))
    };
    println!(
        "{}: {}{attr_suffix}",
        output::format_ohai(&name),
        title_specs(formula, installed_version.as_deref())
    );

    if let Some(desc) = &formula.desc {
        println!("{desc}");
    }
    if let Some(homepage) = &formula.homepage {
        println!("{}", output::format_url(homepage));
    }
    if let Some(msg) = deprecation_message(formula) {
        println!("{msg}");
    }

    if installed {
        println!("Installed");
        // `--verbose` adds the section header above the keg list
        // (`docs/COMPAT.md` 9).
        if ctx.verbose {
            output::ohai("Installed Versions");
        }
        for k in &kegs {
            let (files, bytes) = k.disk_usage();
            let star = if k.is_linked(cfg) || k.is_optlinked(cfg) {
                " *"
            } else {
                ""
            };
            println!("{} ({}){star}", k.path.display(), fmt::abv(files, bytes));
            if let Ok(receipt) = k.receipt() {
                println!("  {}", fmt::tab_line(&receipt));
            }
        }
    } else {
        println!("Not installed");
    }

    println!("From: {}", output::format_url(&github_url(cfg, formula)));
    if !formula.tap.is_empty() && formula.tap != "homebrew/core" {
        println!("Tap: {}", formula.tap);
    }
    if let Some(license) = formula.license_string() {
        println!("License: {license}");
    }

    print_dependencies(ctx, formula, installed);

    if let Some(caveats) = caveats_text(cfg, formula) {
        output::ohai("Caveats");
        println!("{caveats}");
    }
    Ok(())
}

fn print_dependencies(ctx: &Ctx, formula: &FormulaEntry, installed: bool) {
    let cfg = &ctx.cfg;
    let all = formula.dependencies();
    let ufm: Vec<crate::model::Dependency> = formula
        .uses_from_macos()
        .into_iter()
        .filter(|u| {
            let since = u
                .since
                .as_deref()
                .and_then(crate::platform::MacOsVersion::major_for_symbol)
                .unwrap_or(0);
            deps::uses_from_macos_is_dependency(since)
        })
        .map(|u| u.dep)
        .collect();

    let mut lines: Vec<String> = Vec::new();
    for (label, pick) in [
        ("Build", 0u8),
        ("Required", 1),
        ("Recommended", 2),
        ("Optional", 3),
    ] {
        let selected: Vec<&crate::model::Dependency> = all
            .iter()
            .chain(ufm.iter())
            .filter(|d| match pick {
                0 => d.is_build(),
                1 => !d.is_build() && !d.is_test() && !d.is_optional() && !d.is_recommended(),
                2 => d.is_recommended(),
                _ => d.is_optional(),
            })
            .collect();
        if selected.is_empty() {
            continue;
        }
        let rendered: Vec<String> = selected
            .iter()
            .map(|d| {
                let has = !keg::installed_kegs(cfg, &d.name).is_empty();
                fmt::install_status(&d.name, has, installed || output::stdout_is_tty())
            })
            .collect();
        lines.push(format!("{label}: {}", rendered.join(", ")));
    }
    if lines.is_empty() {
        return;
    }
    output::ohai("Dependencies");
    for line in lines {
        println!("{line}");
    }
}

/// `Caveats#to_s`: the formula's own caveats plus the keg-only explanation.
fn caveats_text(cfg: &Config, formula: &FormulaEntry) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(c) = &formula.caveats {
        parts.push(cfg.expand_placeholders(c).trim_end().to_string());
    }
    if let Some((reason, extra)) = formula.keg_only() {
        let linked = keg::linked_keg(cfg, &formula.name).is_some();
        if !linked {
            parts.push(format!(
                "{} is keg-only, which means it was not symlinked into {},\nbecause {}.",
                formula.name,
                cfg.prefix.display(),
                reason.explanation(extra.as_deref())
            ));
        }
    }
    (!parts.is_empty()).then(|| parts.join("\n"))
}

pub fn print_cask_info(ctx: &Ctx, cask: &CaskEntry) {
    let cfg = &ctx.cfg;
    let installed_version = outdated_ops::installed_cask_version(cfg, &cask.token);
    let installed = installed_version.is_some();
    let mut title = fmt::install_status(&cask.full_token(), installed, false);
    if !cask.names.is_empty() {
        title.push_str(&format!(" ({})", cask.names.join(", ")));
    }
    let version = cask.version.clone().unwrap_or_default();
    let shown = match &installed_version {
        Some(v) if *v != version => format!("{v} → {version}"),
        _ => version,
    };
    let auto = if cask.auto_updates {
        " (auto_updates)"
    } else {
        ""
    };
    println!("{}: {shown}{auto}", output::format_ohai(&title));

    if let Some(desc) = &cask.desc {
        println!("{desc}");
    }
    if let Some(homepage) = &cask.homepage {
        println!("{}", output::format_url(homepage));
    }
    match &installed_version {
        Some(v) => {
            let dir = cfg.caskroom().join(&cask.token).join(v);
            let (_, bytes) = keg::disk_usage(&dir);
            println!("Installed");
            println!("{} ({})", dir.display(), keg::disk_usage_readable(bytes));
        }
        None => println!("Not installed"),
    }
    if let Some(path) = &cask.ruby_source_path {
        println!(
            "From: {}",
            output::format_url(&format!(
                "https://github.com/Homebrew/homebrew-cask/blob/HEAD/{path}"
            ))
        );
    }
    let artifacts = cask.artifacts();
    let shown: Vec<&crate::model::cask::Artifact> = artifacts
        .iter()
        .filter(|a| !matches!(a.kind.as_str(), "uninstall" | "zap"))
        .collect();
    if !shown.is_empty() {
        let appdir = cask_appdir(cfg);
        output::ohai("Artifacts");
        for a in shown {
            let summary = a
                .args
                .first()
                .and_then(|v| v.as_array())
                .and_then(|arr| arr.first())
                .and_then(Value::as_str)
                .map(|s| s.replace("$APPDIR", &appdir))
                .unwrap_or_else(|| a.kind.clone());
            println!("{summary} ({})", english_name(&a.kind));
        }
    }
    if let Some(caveats) = cask.caveats_text() {
        output::ohai("Caveats");
        println!("{}", cfg.expand_placeholders(&caveats));
    }
}

/// The resolved cask app directory: `--appdir=` from `HOMEBREW_CASK_OPTS`,
/// else Homebrew's default `/Applications`.
fn cask_appdir(cfg: &Config) -> String {
    cfg.cask_opts
        .iter()
        .find_map(|opt| opt.strip_prefix("--appdir=").map(str::to_string))
        .unwrap_or_else(|| "/Applications".to_string())
}

/// `AbstractArtifact.english_name`: `command_wrapper` -> `Command Wrapper`.
fn english_name(kind: &str) -> String {
    kind.split('_')
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn info_json(ctx: &Ctx, args: &InfoArgs, version: &str) -> Result<()> {
    let index = ctx.index()?;
    let v2 = version == "v2";
    if !v2 && args.cask {
        return Err(Error::user(
            "Cannot specify `--cask` when using `--json=v1`!",
        ));
    }
    let kind = kind_of(args.formula, args.cask);

    let names: Vec<String> = if args.installed {
        keg::installed_formula_names(&ctx.cfg)
    } else {
        args.names.clone()
    };
    let mut formulae: Vec<Value> = Vec::new();
    let mut casks: Vec<Value> = Vec::new();
    for name in &names {
        match resolve::resolve(&ctx.cfg, index, name, kind)? {
            resolve::Resolved::Formula(f) => {
                // The v2 document comes from `formula/<name>.json`, which only
                // exists for the core tap; a tap formula's hash is built by
                // the Ruby from the loaded `Formula` object.
                if !f.tap.is_empty() && f.tap != "homebrew/core" {
                    return ctx.delegate(&format!(
                        "`info --json` for the tap formula {} needs the Ruby formula DSL",
                        f.full_name()
                    ));
                }
                formulae.push(formula_json(ctx, &f)?);
            }
            resolve::Resolved::Cask(c) => {
                if c.tap() != "homebrew/cask" {
                    return ctx.delegate(&format!(
                        "`info --json` for the tap cask {} needs the Ruby cask DSL",
                        c.full_token()
                    ));
                }
                casks.push(cask_json(ctx, &c)?);
            }
        }
    }
    if args.installed && !args.formula {
        for token in deps::installed_cask_tokens(&ctx.cfg) {
            let c = resolve::resolve_cask(&ctx.cfg, index, &token)?;
            casks.push(cask_json(ctx, &c)?);
        }
    }

    let doc = if v2 {
        json!({ "formulae": formulae, "casks": casks })
    } else {
        Value::Array(formulae)
    };
    println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
    Ok(())
}

/// The v2 formula document from `formula/<name>.json`, with the locally
/// computed `installed`, `linked_keg`, `pinned` and `outdated` keys merged in
/// (what `Formula#to_hash_with_variations` does for API formulae).
fn formula_json(ctx: &Ctx, formula: &FormulaEntry) -> Result<Value> {
    let mut doc = crate::api::fetch::fetch_json_endpoint(
        &ctx.cfg,
        &format!("formula/{}.json", formula.name),
        Some(crate::api::fetch::JSON_ENDPOINT_STALE_SECONDS),
    )?;
    let Some(map) = doc.as_object_mut() else {
        return Ok(doc);
    };
    let kegs = keg::installed_kegs(&ctx.cfg, &formula.name);
    let installed: Vec<Value> = kegs
        .iter()
        .map(|k| {
            let receipt = k.receipt().unwrap_or_default();
            json!({
                "version": k.version.to_string(),
                "used_options": receipt.used_options,
                "built_as_bottle": receipt.built_as_bottle,
                "poured_from_bottle": receipt.poured_from_bottle,
                "time": receipt.time,
                "runtime_dependencies": receipt.runtime_dependencies.clone().unwrap_or_default(),
                "installed_on_request": receipt.installed_on_request,
            })
        })
        .collect();
    map.insert("installed".into(), Value::Array(installed));
    map.insert(
        "linked_keg".into(),
        keg::linked_keg(&ctx.cfg, &formula.name)
            .map(|k| Value::String(k.version.to_string()))
            .unwrap_or(Value::Null),
    );
    map.insert(
        "pinned".into(),
        Value::Bool(keg::is_pinned(&ctx.cfg, &formula.name)),
    );
    map.insert(
        "outdated".into(),
        Value::Bool(!outdated_ops::outdated_kegs(&ctx.cfg, ctx.index()?, &formula.name).is_empty()),
    );
    Ok(doc)
}

fn cask_json(ctx: &Ctx, cask: &CaskEntry) -> Result<Value> {
    let mut doc = crate::api::fetch::fetch_json_endpoint(
        &ctx.cfg,
        &format!("cask/{}.json", cask.token),
        Some(crate::api::fetch::JSON_ENDPOINT_STALE_SECONDS),
    )?;
    let Some(map) = doc.as_object_mut() else {
        return Ok(doc);
    };
    let installed = outdated_ops::installed_cask_version(&ctx.cfg, &cask.token);
    map.insert(
        "installed".into(),
        installed.clone().map(Value::String).unwrap_or(Value::Null),
    );
    let pinned = ctx.cfg.pinned_casks().join(&cask.token).is_symlink();
    map.insert("pinned".into(), Value::Bool(pinned));
    let index = ctx.index()?;
    let outdated = !outdated_ops::outdated_casks(
        &ctx.cfg,
        index,
        Some(std::slice::from_ref(&cask.token)),
        false,
    )?
    .is_empty();
    map.insert("outdated".into(), Value::Bool(outdated));
    Ok(doc)
}

// ---------------------------------------------------------------------------
// search / desc / home
// ---------------------------------------------------------------------------

/// `Search.search`: match a tap's full names against the query, keeping the
/// `user/repo/name` form Homebrew prints for tap packages.
fn search_taps<I>(names: I, query: &SearchQuery) -> Vec<String>
where
    I: IntoIterator<Item = String>,
{
    let mut hits: Vec<String> = names
        .into_iter()
        .filter(|full| {
            let short = full.rsplit('/').next().unwrap_or(full);
            match query {
                SearchQuery::Regex(re) => re.is_match(short) || re.is_match(full),
                SearchQuery::Text(t) => {
                    let needle = crate::api::index::simplify(t);
                    crate::api::index::simplify(short).contains(&needle)
                        || crate::api::index::simplify(full).contains(&needle)
                }
            }
        })
        .collect();
    hits.sort();
    hits.dedup();
    hits
}

pub fn search(ctx: &Ctx, args: &SearchArgs) -> Result<()> {
    if args.query.is_empty() {
        return Err(Error::user(
            "Invalid usage: this command requires at least 1 named argument.",
        ));
    }
    let query = args.query.join(" ");
    let index = ctx.index()?;
    let want_formulae = !args.cask || args.formula;
    let want_casks = !args.formula || args.cask;

    if args.desc {
        let parsed = SearchQuery::parse(&query)?;
        // `brew search --desc` searches descriptions only.
        if want_formulae {
            output::ohai("Formulae");
            print_descriptions(
                ctx,
                index.search_formula_descriptions(&parsed, DescScope::Desc),
                false,
            );
        }
        if want_formulae && want_casks {
            println!();
        }
        if want_casks {
            output::ohai("Casks");
            print_descriptions(
                ctx,
                index.search_cask_descriptions(&parsed, DescScope::Desc),
                true,
            );
        }
        return Ok(());
    }

    let parsed = SearchQuery::parse(&query)?;
    let mut formulae = if want_formulae {
        match &parsed {
            SearchQuery::Regex(re) => index.search_formula_names_regex(re),
            SearchQuery::Text(t) => index.search_formula_names(t),
        }
    } else {
        vec![]
    };
    let mut casks = if want_casks {
        match &parsed {
            SearchQuery::Regex(re) => index.search_cask_tokens_regex(re),
            SearchQuery::Text(t) => index.search_cask_tokens(t),
        }
    } else {
        vec![]
    };

    // `Search.search_formulae`/`search_casks` add spell-checker hits for plain
    // text queries: appended (not re-sorted) for formulae, sorted in for casks.
    if let SearchQuery::Text(text) = &parsed {
        if want_formulae {
            for hit in resolve::spell_check(text, &index.formula_names()) {
                if !formulae.contains(&hit) {
                    formulae.push(hit);
                }
            }
        }
        if want_casks {
            casks.extend(resolve::spell_check(text, &index.cask_tokens()));
            casks.sort();
            casks.dedup();
        }
    }

    // An alias is dropped when the formula it points at is also a result.
    let found: std::collections::HashSet<String> = formulae.iter().cloned().collect();
    formulae.retain(|name| match index.formula_alias(name) {
        Some(target) => !found.contains(&target),
        None => true,
    });

    // `Search.search_taps`: tap packages are listed by their full name.
    if want_formulae {
        formulae.extend(search_taps(
            ctx.taps().all_formulae().iter().map(|f| f.full_name()),
            &parsed,
        ));
    }
    if want_casks {
        casks.extend(search_taps(
            ctx.taps().all_casks().iter().map(|c| c.full_token()),
            &parsed,
        ));
    }

    let tty = output::stdout_is_tty();
    if !formulae.is_empty() {
        if tty {
            output::ohai("Formulae");
        }
        let decorated: Vec<String> = formulae
            .iter()
            .map(|n| {
                let installed = !keg::installed_kegs(&ctx.cfg, n).is_empty();
                fmt::install_status(n, installed, false)
            })
            .collect();
        fmt::print_names(&decorated);
    }
    if !formulae.is_empty() && !casks.is_empty() {
        println!();
    }
    if !casks.is_empty() {
        if tty {
            output::ohai("Casks");
        }
        let decorated: Vec<String> = casks
            .iter()
            .map(|t| {
                let installed = outdated_ops::installed_cask_version(&ctx.cfg, t).is_some();
                fmt::install_status(t, installed, false)
            })
            .collect();
        fmt::print_names(&decorated);
    }
    if formulae.is_empty() && casks.is_empty() {
        return Err(Error::user(format!(
            "No formulae or casks found for {}.",
            json!(query)
        )));
    }
    Ok(())
}

fn print_descriptions(ctx: &Ctx, mut hits: Vec<(String, String)>, cask: bool) {
    hits.sort_by(|a, b| a.0.cmp(&b.0));
    let index = ctx.index().ok();
    for (name, desc) in hits {
        let installed = if cask {
            outdated_ops::installed_cask_version(&ctx.cfg, &name).is_some()
        } else {
            !keg::installed_kegs(&ctx.cfg, &name).is_empty()
        };
        let display = fmt::install_status(&name, installed, false);
        let names = if cask {
            index
                .and_then(|i| i.cask_index(&name))
                .map(|i| index.unwrap().cask_names_at(i))
                .unwrap_or_default()
        } else {
            vec![]
        };
        if cask && !names.is_empty() {
            println!("{display}: ({}) {desc}", names.join(", "));
        } else {
            println!("{display}: {desc}");
        }
    }
}

pub fn desc(ctx: &Ctx, args: &DescArgs) -> Result<()> {
    if args.names.is_empty() {
        return Err(Error::user(
            "Invalid usage: this command requires at least 1 named argument.",
        ));
    }
    let index = ctx.index()?;
    if args.search || args.name || args.description {
        let query = SearchQuery::parse(&args.names.join(" "))?;
        let want_formulae = !args.cask;
        let want_casks = !args.formula;
        if want_formulae {
            output::ohai("Formulae");
            print_descriptions(ctx, filter_desc(index, &query, args, false), false);
        }
        if want_formulae && want_casks {
            println!();
        }
        if want_casks {
            output::ohai("Casks");
            print_descriptions(ctx, filter_desc(index, &query, args, true), true);
        }
        return Ok(());
    }

    let kind = kind_of(args.formula, args.cask);
    let mut hits: BTreeMap<String, (String, bool)> = BTreeMap::new();
    for name in &args.names {
        match resolve::resolve(&ctx.cfg, index, name, kind)? {
            resolve::Resolved::Formula(f) => {
                if let Some(d) = f.desc {
                    hits.insert(f.name.clone(), (d, false));
                }
            }
            resolve::Resolved::Cask(c) => {
                if let Some(d) = c.desc.clone() {
                    hits.insert(c.token.clone(), (d, true));
                }
            }
        }
    }
    for (name, (d, is_cask)) in hits {
        print_descriptions(ctx, vec![(name, d)], is_cask);
    }
    Ok(())
}

/// `--name` matches names only, `--description` descriptions only, `--search` both.
fn filter_desc(
    index: &Index,
    query: &SearchQuery,
    args: &DescArgs,
    cask: bool,
) -> Vec<(String, String)> {
    let scope = if args.search || (args.name && args.description) {
        DescScope::Either
    } else if args.name {
        DescScope::Name
    } else {
        DescScope::Desc
    };
    if cask {
        index.search_cask_descriptions(query, scope)
    } else {
        index.search_formula_descriptions(query, scope)
    }
}

pub fn home(ctx: &Ctx, args: &HomeArgs) -> Result<()> {
    if args.names.is_empty() {
        return misc::open_url("https://brew.sh");
    }
    let index = ctx.index()?;
    let mut urls: Vec<String> = Vec::new();
    for name in &args.names {
        match resolve::resolve(&ctx.cfg, index, name, Kind::Any)? {
            resolve::Resolved::Formula(f) => {
                println!("Opening homepage for Formula {}", f.name);
                if let Some(h) = f.homepage {
                    urls.push(h);
                }
            }
            resolve::Resolved::Cask(c) => {
                println!("Opening homepage for Cask {}", c.token);
                if let Some(h) = c.homepage {
                    urls.push(h);
                }
            }
        }
    }
    for url in urls {
        misc::open_url(&url)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

pub fn list(ctx: &Ctx, args: &ListArgs) -> Result<()> {
    let cfg = &ctx.cfg;
    if args.json {
        if !args.versions {
            return Err(Error::user("`fastbrew list --json` requires `--versions`."));
        }
        return list_json(ctx, args);
    }
    if args.pinned {
        let mut entries: Vec<String> = keg::installed_formula_names(cfg)
            .into_iter()
            .filter(|n| keg::is_pinned(cfg, n))
            .map(|n| match outdated_ops::pinned_version(cfg, &n) {
                Some(v) if args.versions => format!("{n} {v}"),
                _ => n,
            })
            .collect();
        entries.sort();
        for e in entries {
            println!("{e}");
        }
        return Ok(());
    }
    if !args.names.is_empty() {
        return list_named(ctx, args);
    }
    if args.installed_on_request || args.installed_as_dependency {
        return list_by_receipt(ctx, args);
    }
    if args.versions || args.multiple {
        return list_versions(ctx, args);
    }

    let tty = output::stdout_is_tty();
    let formulae = if args.cask {
        vec![]
    } else {
        list_formula_names(ctx, args)
    };
    let casks = if args.formula {
        vec![]
    } else {
        deps::installed_cask_tokens(cfg)
    };

    if !formulae.is_empty() {
        if tty && !args.formula {
            output::ohai("Formulae");
        }
        print_list(&formulae, args);
        if tty && !args.formula && !casks.is_empty() {
            println!();
        }
    }
    if !casks.is_empty() {
        if tty && !args.cask {
            output::ohai("Casks");
        }
        print_list(&casks, args);
    }
    Ok(())
}

/// `Formula#full_name` for a name that may come from a tap: the index first,
/// then the tap metadata, then the keg receipt (the only source for a formula
/// the API never had).
fn installed_full_name(ctx: &Ctx, name: &str) -> String {
    if let Ok(index) = ctx.index()
        && let Some(f) = index.formula(name)
    {
        return f.full_name();
    }
    if let Some(meta) = ctx.taps().taps_with_formula(name).first() {
        return meta.tap.full_package_name(name);
    }
    let tap = keg::latest_keg(&ctx.cfg, name)
        .and_then(|k| k.receipt().ok())
        .and_then(|r| r.tap().map(str::to_string));
    match tap {
        Some(t) if t != "homebrew/core" => format!("{t}/{name}"),
        _ => name.to_string(),
    }
}

fn list_formula_names(ctx: &Ctx, args: &ListArgs) -> Vec<String> {
    let mut names = keg::installed_formula_names(&ctx.cfg);
    if args.full_name {
        names = names
            .into_iter()
            .map(|n| installed_full_name(ctx, &n))
            .collect();
        names.sort_by(tap_and_name_comparison);
    }
    if args.by_time {
        names.sort_by_key(|n| {
            std::fs::metadata(ctx.cfg.rack(n))
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH)
        });
        names.reverse();
    }
    if args.reverse {
        names.reverse();
    }
    names
}

/// `Cask::List::TAP_AND_NAME_COMPARISON`: unqualified names sort first.
fn tap_and_name_comparison(a: &String, b: &String) -> std::cmp::Ordering {
    match (a.contains('/'), b.contains('/')) {
        (false, true) => std::cmp::Ordering::Less,
        (true, false) => std::cmp::Ordering::Greater,
        _ => a.cmp(b),
    }
}

fn print_list(items: &[String], args: &ListArgs) {
    if args.one || args.long {
        for i in items {
            println!("{i}");
        }
    } else {
        fmt::print_names(items);
    }
}

fn list_versions(ctx: &Ctx, args: &ListArgs) -> Result<()> {
    let cfg = &ctx.cfg;
    let wanted: Vec<String> = if args.names.is_empty() {
        keg::installed_formula_names(cfg)
    } else {
        args.names.clone()
    };
    if !args.cask {
        for name in &wanted {
            let versions: Vec<String> = keg::installed_kegs(cfg, name)
                .iter()
                .map(|k| k.version.to_string())
                .collect();
            if versions.is_empty() || (args.multiple && versions.len() < 2) {
                continue;
            }
            println!("{name} {}", versions.join(" "));
        }
    }
    if !args.formula && !args.multiple {
        for token in deps::installed_cask_tokens(cfg) {
            if let Some(v) = outdated_ops::installed_cask_version(cfg, &token) {
                println!("{token} {v}");
            }
        }
    }
    Ok(())
}

fn list_by_receipt(ctx: &Ctx, args: &ListArgs) -> Result<()> {
    if !args.names.is_empty() {
        let mut flags: Vec<&str> = Vec::new();
        if args.installed_on_request {
            flags.push("`--installed-on-request`");
        }
        if args.installed_as_dependency {
            flags.push("`--installed-as-dependency`");
        }
        return Err(Error::user(format!(
            "Cannot use {} with formula arguments.",
            flags.join(", ")
        )));
    }
    let both = args.installed_on_request && args.installed_as_dependency;
    for name in list_formula_names(ctx, args) {
        let on_request = deps::installed_on_request(&ctx.cfg, &name);
        let mut statuses: Vec<&str> = Vec::new();
        if args.installed_on_request && on_request {
            statuses.push("installed on request");
        }
        if args.installed_as_dependency && !on_request {
            statuses.push("installed as dependency");
        }
        if statuses.is_empty() {
            continue;
        }
        if both {
            println!("{name}: {}", statuses.join(", "));
        } else {
            println!("{name}");
        }
    }
    Ok(())
}

fn list_json(ctx: &Ctx, args: &ListArgs) -> Result<()> {
    let cfg = &ctx.cfg;
    let formulae: Vec<Value> = if args.cask {
        vec![]
    } else {
        keg::installed_formula_names(cfg)
            .into_iter()
            .map(|name| {
                let mut versions: Vec<String> = keg::installed_kegs(cfg, &name)
                    .iter()
                    .map(|k| k.version.to_string())
                    .collect();
                versions.dedup();
                json!({
                    "name": name,
                    "versions": versions,
                    "linked_version": linked_version(cfg, &name),
                    "optlinked_version": optlinked_version(cfg, &name),
                    "pinned_version": outdated_ops::pinned_version(cfg, &name),
                })
            })
            .collect()
    };
    let casks: Vec<Value> = if args.formula {
        vec![]
    } else {
        deps::installed_cask_tokens(cfg)
            .into_iter()
            .map(|token| {
                let versions: Vec<String> = outdated_ops::installed_cask_version(cfg, &token)
                    .into_iter()
                    .collect();
                let pinned = cfg.pinned_casks().join(&token);
                json!({
                    "token": token,
                    "versions": versions,
                    "pinned_version": pinned.is_symlink().then(|| {
                        std::fs::read_link(&pinned).ok()
                            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                    }).flatten(),
                })
            })
            .collect()
    };
    println!(
        "{}",
        serde_json::to_string(&json!({"formulae": formulae, "casks": casks})).unwrap_or_default()
    );
    Ok(())
}

fn linked_version(cfg: &Config, name: &str) -> Option<String> {
    let record = cfg.linked_record(name);
    record.is_symlink().then(|| {
        std::fs::read_link(&record)
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
    })?
}

fn optlinked_version(cfg: &Config, name: &str) -> Option<String> {
    let record = cfg.opt_record(name);
    record.is_symlink().then(|| {
        std::fs::read_link(&record)
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
    })?
}

fn list_named(ctx: &Ctx, args: &ListArgs) -> Result<()> {
    if args.versions || args.multiple {
        return list_versions(ctx, args);
    }
    let cfg = &ctx.cfg;
    for name in &args.names {
        let kegs = keg::installed_kegs(cfg, name);
        if kegs.is_empty() {
            let caskroom = cfg.caskroom().join(name);
            if caskroom.is_dir() {
                print_find(&caskroom);
                continue;
            }
            return Err(Error::user(format!(
                "No such keg: {}",
                cfg.rack(name).display()
            )));
        }
        for k in kegs {
            if output::stdout_is_tty() && !ctx.verbose {
                pretty_listing(&k);
            } else {
                print_find(&k.path);
            }
        }
    }
    Ok(())
}

/// `find <keg> -not -type d -not -name .DS_Store -print`.
fn print_find(root: &std::path::Path) {
    for entry in walkdir::WalkDir::new(root)
        .sort_by_file_name()
        .into_iter()
        .flatten()
    {
        if entry.file_type().is_dir() {
            continue;
        }
        if entry.file_name() == ".DS_Store" {
            continue;
        }
        println!("{}", entry.path().display());
    }
}

/// Port of `Homebrew::Cmd::List::PrettyListing`.
fn pretty_listing(k: &Keg) {
    let Ok(rd) = std::fs::read_dir(&k.path) else {
        return;
    };
    let mut children: Vec<std::path::PathBuf> = rd.flatten().map(|e| e.path()).collect();
    children.sort_by_key(|p| p.to_string_lossy().to_lowercase());
    for pn in children {
        let base = pn
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        match base.as_str() {
            "bin" | "sbin" => {
                for entry in walkdir::WalkDir::new(&pn)
                    .sort_by_file_name()
                    .into_iter()
                    .flatten()
                {
                    if !entry.file_type().is_dir() {
                        println!("{}", entry.path().display());
                    }
                }
            }
            "lib" => print_dir(&pn, true),
            ".brew" => {}
            _ => {
                if pn.is_dir() {
                    if pn.is_symlink() {
                        if let Ok(target) = std::fs::read_link(&pn) {
                            println!("{} -> {}", pn.display(), target.display());
                        }
                    } else {
                        print_dir(&pn, false);
                    }
                } else if metafiles_list(&base) {
                    println!("{}", pn.display());
                }
            }
        }
    }
}

fn print_dir(root: &std::path::Path, lib: bool) {
    let valid_lib_extensions = [".cps", ".dylib", ".pc"];
    let Ok(rd) = std::fs::read_dir(root) else {
        return;
    };
    let mut children: Vec<std::path::PathBuf> = rd.flatten().map(|e| e.path()).collect();
    children.sort();
    let mut dirs = Vec::new();
    let mut remaining = Vec::new();
    let mut other = "";
    for pn in children {
        if pn.is_dir() {
            dirs.push(pn);
            continue;
        }
        let name = pn
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let is_valid_lib = lib
            && valid_lib_extensions
                .iter()
                .any(|e| name.ends_with(e) && !pn.is_symlink());
        if is_valid_lib {
            println!("{}", pn.display());
            other = "other ";
        } else if name != ".DS_Store" {
            remaining.push(pn);
        }
    }
    for d in dirs {
        let files: Vec<std::path::PathBuf> = walkdir::WalkDir::new(&d)
            .sort_by_file_name()
            .into_iter()
            .flatten()
            .filter(|e| !e.file_type().is_dir())
            .map(|e| e.path().to_path_buf())
            .collect();
        print_remaining(&files, &d, "");
    }
    print_remaining(&remaining, root, other);
}

fn print_remaining(files: &[std::path::PathBuf], root: &std::path::Path, other: &str) {
    match files.len() {
        0 => {}
        1 => println!("{}", files[0].display()),
        n => println!("{}/ ({n} {other}files)", root.display()),
    }
}

/// `Metafiles.list?`.
fn metafiles_list(file: &str) -> bool {
    if file == ".DS_Store" || file == "INSTALL_RECEIPT.json" {
        return false;
    }
    !metafiles_copy(file)
}

fn metafiles_copy(file: &str) -> bool {
    const LICENSES: [&str; 4] = ["copying", "copyright", "license", "licence"];
    const EXTENSIONS: [&str; 18] = [
        ".adoc",
        ".asc",
        ".asciidoc",
        ".creole",
        ".html",
        ".markdown",
        ".md",
        ".mdown",
        ".mediawiki",
        ".mkdn",
        ".org",
        ".pod",
        ".rdoc",
        ".rst",
        ".rtf",
        ".textile",
        ".txt",
        ".wiki",
    ];
    const BASENAMES: [&str; 10] = [
        "about",
        "authors",
        "changelog",
        "changes",
        "history",
        "news",
        "notes",
        "notice",
        "readme",
        "todo",
    ];
    let file = file.to_lowercase();
    let license = file.split(['.', '-']).next().unwrap_or("");
    if LICENSES.contains(&license) {
        return true;
    }
    let stem = match file.rfind('.') {
        Some(i) if EXTENSIONS.contains(&&file[i..]) => &file[..i],
        _ => file.as_str(),
    };
    BASENAMES.contains(&stem)
}

// ---------------------------------------------------------------------------
// deps / uses / leaves
// ---------------------------------------------------------------------------

pub fn deps(ctx: &Ctx, args: &DepsArgs) -> Result<()> {
    let index = ctx.index()?;
    let opts = dep_options(
        args.include_build,
        args.include_test,
        args.include_optional,
        args.include_implicit,
        args.skip_recommended,
    );
    let recursive = !args.direct;

    let roots: Vec<String> = if !args.names.is_empty() {
        let mut r: Vec<String> = args
            .names
            .iter()
            .map(|n| {
                resolve::resolve_formula(&ctx.cfg, index, n)
                    .map(|f| f.name)
                    .unwrap_or_else(|_| n.clone())
            })
            .collect();
        r.sort();
        r
    } else if args.installed {
        keg::installed_formula_names(&ctx.cfg)
    } else {
        return Err(Error::user(
            "This command requires a formula argument (or `--installed`).",
        ));
    };

    let runtime = runtime_dependencies_mode(ctx, args, &roots);

    if args.tree {
        for root in &roots {
            // `puts_deps_tree` labels each root with its full name.
            println!("{}", installed_full_name(ctx, root));
            let mut seen: Vec<String> = Vec::new();
            print_tree(ctx, index, root, "", opts, recursive, &mut seen, args);
            println!();
        }
        return Ok(());
    }

    if args.for_each || (args.installed && args.names.is_empty()) {
        for root in &roots {
            let mut d = collect_deps(ctx, index, root, opts, recursive, runtime);
            d.sort();
            println!("{root}: {}", render_deps(ctx, index, &d, args).join(" "));
        }
        return Ok(());
    }

    let mut all: Option<Vec<String>> = None;
    for root in &roots {
        let d = collect_deps(ctx, index, root, opts, recursive, runtime);
        all = Some(match all {
            None => d,
            Some(prev) if args.union => {
                let mut merged = prev;
                for x in d {
                    if !merged.contains(&x) {
                        merged.push(x);
                    }
                }
                merged
            }
            Some(prev) => prev.into_iter().filter(|x| d.contains(x)).collect(),
        });
    }
    let mut result = all.unwrap_or_default();
    if args.missing {
        result.retain(|n| keg::installed_kegs(&ctx.cfg, n).is_empty());
    }
    if args.installed && !args.names.is_empty() {
        result.retain(|n| !keg::installed_kegs(&ctx.cfg, n).is_empty());
    }
    let mut rendered = render_deps(ctx, index, &result, args);
    rendered.dedup();
    if !args.topological {
        rendered.sort();
    }
    for d in rendered {
        println!("{d}");
    }
    Ok(())
}

fn collect_deps(
    ctx: &Ctx,
    index: &Index,
    root: &str,
    opts: DepOptions,
    recursive: bool,
    runtime: bool,
) -> Vec<String> {
    if runtime && let Some(names) = recorded_runtime_dependencies(ctx, root) {
        return names;
    }
    // A tap formula is not in the index, so its own `depends_on` list comes
    // from the parsed entry; the dependencies themselves are core formulae
    // the index knows about.
    if !index.has_formula(root)
        && let Some(direct) = tap_direct_dependencies(ctx, root, opts)
    {
        if !recursive {
            return direct;
        }
        let mut all: Vec<String> = Vec::new();
        for d in direct {
            for name in deps::recursive_dependency_names(index, &d, opts)
                .into_iter()
                .chain(std::iter::once(d))
            {
                if !all.contains(&name) {
                    all.push(name);
                }
            }
        }
        return all;
    }
    if recursive {
        deps::recursive_dependency_names(index, root, opts)
    } else {
        deps::direct_dependency_names(index, root, opts, true)
    }
}

/// `depends_on` of a tap formula, filtered like `deps::direct_dependency_names`.
fn tap_direct_dependencies(ctx: &Ctx, name: &str, opts: DepOptions) -> Option<Vec<String>> {
    let index = ctx.index().ok()?;
    let entry = resolve::resolve_formula(&ctx.cfg, index, name).ok()?;
    if entry.tap.is_empty() || entry.tap == "homebrew/core" {
        return None;
    }
    Some(
        entry
            .dependencies()
            .into_iter()
            .filter(|d| {
                (d.is_runtime()
                    || (opts.include_build && d.is_build())
                    || (opts.include_test && d.is_test()))
                    && (!d.is_optional() || opts.include_optional)
                    && (!d.is_recommended() || !opts.skip_recommended)
            })
            .map(|d| d.name)
            .collect(),
    )
}

/// The runtime closure the installed keg recorded, in receipt order.
fn recorded_runtime_dependencies(ctx: &Ctx, name: &str) -> Option<Vec<String>> {
    let receipt = keg::latest_keg(&ctx.cfg, name)?.receipt().ok()?;
    Some(
        receipt
            .runtime_dependency_names()
            .into_iter()
            .map(|full| deps::short_name(full).to_string())
            .collect(),
    )
}

/// Port of `Homebrew::Cmd::Deps#use_runtime_dependencies?`: an installed
/// formula reports what its keg actually links against unless a flag asks for
/// the declared graph instead. The mismatch prints Homebrew's env hint.
fn runtime_dependencies_mode(ctx: &Ctx, args: &DepsArgs, roots: &[String]) -> bool {
    let all_installed = !roots.is_empty()
        && roots
            .iter()
            .all(|r| !keg::installed_kegs(&ctx.cfg, r).is_empty());
    let reason: Option<&str> = if !all_installed {
        Some(if args.installed {
            "not all the named formulae were installed"
        } else {
            "`--installed` was not passed"
        })
    } else if args.direct {
        Some("--direct was passed")
    } else if args.tree {
        Some("--tree was passed")
    } else if args.skip_recommended {
        Some("--skip-recommended was passed")
    } else if args.missing {
        Some("--missing was passed")
    } else if args.include_implicit {
        Some("--include-implicit was passed")
    } else if args.include_build {
        Some("--include-build was passed")
    } else if args.include_test {
        Some("--include-test was passed")
    } else if args.include_optional {
        Some("--include-optional was passed")
    } else {
        None
    };
    match reason {
        None => true,
        Some(reason) => {
            if !ctx.cfg.no_env_hints {
                output::opoo(&format!(
                    "`fastbrew deps` is not the actual runtime dependencies because {reason}!\nThis means dependencies may differ from a formula's declared dependencies.\nHide these hints with `HOMEBREW_NO_ENV_HINTS=1` (see `man brew`)."
                ));
            }
            false
        }
    }
}

fn render_deps(ctx: &Ctx, index: &Index, names: &[String], args: &DepsArgs) -> Vec<String> {
    names
        .iter()
        .map(|n| {
            let mut s = if args.full_name {
                index
                    .formula(n)
                    .map(|f| f.full_name())
                    .unwrap_or_else(|| n.clone())
            } else {
                n.clone()
            };
            if args.annotate {
                s.push_str(&annotations(index, n, args));
            }
            let _ = ctx;
            s
        })
        .collect()
}

/// Annotations for the flat `deps` listing.
///
/// KNOWN GAP: a flat listing loses which dependent declared the dependency
/// (the list can be an intersection or union of several roots), so tags are
/// only rendered by `--tree`, which keeps that context.
fn annotations(_index: &Index, _name: &str, _args: &DepsArgs) -> String {
    String::new()
}

#[allow(clippy::too_many_arguments)]
fn print_tree(
    ctx: &Ctx,
    index: &Index,
    name: &str,
    prefix: &str,
    opts: DepOptions,
    recursive: bool,
    seen: &mut Vec<String>,
    args: &DepsArgs,
) {
    let dependables = if index.has_formula(name) {
        deps::direct_dependency_names(index, name, opts, seen.is_empty())
    } else {
        tap_direct_dependencies(ctx, name, opts).unwrap_or_default()
    };
    let max = dependables.len().saturating_sub(1);
    seen.push(name.to_string());
    for (i, dep) in dependables.iter().enumerate() {
        let branch = if i == max { "└──" } else { "├──" };
        let mut display = dep.clone();
        if args.annotate {
            display.push(' ');
            display.push_str(&annotate_tags(index, name, dep));
        }
        let circular = seen.iter().any(|s| s == dep);
        if circular {
            display.push_str(" (CIRCULAR DEPENDENCY)");
        }
        println!("{prefix}{branch} {display}");
        if !recursive || circular {
            continue;
        }
        let addition = if i == max { "    " } else { "│   " };
        print_tree(
            ctx,
            index,
            dep,
            &format!("{prefix}{addition}"),
            opts,
            true,
            seen,
            args,
        );
    }
    seen.pop();
}

/// ` [build] [test] [optional] [recommended] [implicit]` in Homebrew's order.
fn annotate_tags(index: &Index, dependent: &str, dep: &str) -> String {
    use crate::api::index::depflag;
    let Some((_, flags, _)) = index
        .formula_deps(dependent)
        .into_iter()
        .find(|(n, _, _)| *n == dep)
    else {
        return String::new();
    };
    let mut out = String::new();
    for (bit, label) in [
        (depflag::BUILD, "build"),
        (depflag::TEST, "test"),
        (depflag::OPTIONAL, "optional"),
        (depflag::RECOMMENDED, "recommended"),
        (depflag::IMPLICIT, "implicit"),
    ] {
        if flags & bit != 0 {
            out.push_str(&format!(" [{label}]"));
        }
    }
    out
}

pub fn uses(ctx: &Ctx, args: &UsesArgs) -> Result<()> {
    let index = ctx.index()?;
    let opts = dep_options(
        args.include_build,
        args.include_test,
        args.include_optional,
        false,
        args.skip_recommended,
    );
    let mut result: Option<Vec<String>> = None;
    for name in &args.names {
        let formula = resolve::resolve_formula(&ctx.cfg, index, name)?;
        let users = deps::uses(
            &ctx.cfg,
            index,
            &formula.name,
            args.recursive,
            args.installed,
            opts,
        )?;
        result = Some(match result {
            None => users,
            Some(prev) => prev.into_iter().filter(|x| users.contains(x)).collect(),
        });
    }
    let mut users = result.unwrap_or_default();
    // Tap formulae are not in the index, so scan their parsed entries too.
    for name in &args.names {
        let short = name.rsplit('/').next().unwrap_or(name);
        for entry in ctx.taps().all_formulae() {
            if args.installed && keg::installed_kegs(&ctx.cfg, &entry.name).is_empty() {
                continue;
            }
            let declares = entry.dependencies().into_iter().any(|d| {
                d.name == short
                    && (d.is_runtime()
                        || (args.include_build && d.is_build())
                        || (args.include_test && d.is_test()))
            });
            if declares && !users.contains(&entry.full_name()) {
                users.push(entry.full_name());
            }
        }
    }
    users.sort();
    users.dedup();
    if users.is_empty() {
        return Ok(());
    }
    fmt::print_names(&users);
    Ok(())
}

pub fn leaves(ctx: &Ctx, args: &LeavesArgs) -> Result<()> {
    let index = ctx.index()?;
    let mut names = deps::leaves_with_index(&ctx.cfg, index)?;
    if args.installed_on_request {
        names.retain(|n| deps::installed_on_request(&ctx.cfg, n));
    }
    if args.installed_as_dependency {
        names.retain(|n| !deps::installed_on_request(&ctx.cfg, n));
    }
    for n in names {
        println!("{n}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// outdated / missing / options / which-formula
// ---------------------------------------------------------------------------

/// Installed formulae that came from a third-party tap.
///
/// `ops::outdated` compares against the packages API, which knows nothing
/// about them, so their kegs are compared against the tap metadata instead
/// (`Formula#outdated_kegs` with the tap's version).
fn outdated_tap_formulae(
    ctx: &Ctx,
    names: Option<&[String]>,
) -> Vec<outdated_ops::OutdatedFormula> {
    use crate::version::PkgVersion;
    let taps = ctx.taps();
    if taps.is_empty() {
        return vec![];
    }
    let mut out = Vec::new();
    for entry in taps.all_formulae() {
        if let Some(wanted) = names
            && !wanted
                .iter()
                .any(|n| *n == entry.name || *n == entry.full_name())
        {
            continue;
        }
        let kegs = keg::installed_kegs(&ctx.cfg, &entry.name);
        if kegs.is_empty() {
            continue;
        }
        // The keg has to have come from this tap, not from the core API.
        let from_tap = kegs.iter().any(|k| {
            k.receipt()
                .ok()
                .and_then(|r| r.tap().map(str::to_string))
                .is_some_and(|t| t.eq_ignore_ascii_case(&entry.tap))
        });
        if !from_tap {
            continue;
        }
        let current = PkgVersion::parse(&entry.pkg_version());
        if current.version.as_str().is_empty() {
            continue;
        }
        if kegs.iter().any(|k| k.version >= current) {
            continue;
        }
        out.push(outdated_ops::OutdatedFormula {
            name: entry.name.clone(),
            installed_versions: kegs.iter().map(|k| k.version.to_string()).collect(),
            current_version: current.to_string(),
            pinned: keg::is_pinned(&ctx.cfg, &entry.name),
            pinned_version: outdated_ops::pinned_version(&ctx.cfg, &entry.name),
        });
    }
    out
}

pub fn outdated(ctx: &Ctx, args: &OutdatedArgs) -> Result<()> {
    crate::update::auto_update_if_needed(&ctx.cfg, "outdated", &args.names);
    let index = ctx.index()?;
    let names = (!args.names.is_empty()).then(|| args.names.clone());
    let greedy = args.greedy || args.greedy_latest || args.greedy_auto_updates;

    let formulae = if args.cask && !args.formula {
        vec![]
    } else {
        let mut all = outdated_ops::outdated_formulae(&ctx.cfg, index, names.as_deref())?;
        all.extend(outdated_tap_formulae(ctx, names.as_deref()));
        all.sort_by(|a, b| a.name.cmp(&b.name));
        all.dedup_by(|a, b| a.name == b.name);
        all
    };
    let casks = if args.formula && !args.cask {
        vec![]
    } else {
        outdated_ops::outdated_casks(&ctx.cfg, index, names.as_deref(), greedy)?
    };

    if let Some(version) = &args.json {
        if version == "v1" {
            return Err(Error::user(
                "`fastbrew outdated --json=v1` is no longer supported. Use fastbrew outdated --json=v2 instead.",
            ));
        }
        let doc = json!({
            "formulae": formulae.iter().map(|o| json!({
                "name": o.name,
                "installed_versions": o.installed_versions,
                "current_version": o.current_version,
                "pinned": o.pinned,
                "pinned_version": o.pinned_version,
            })).collect::<Vec<_>>(),
            "casks": casks.iter().map(|o| json!({
                "name": o.token,
                "installed_versions": [o.installed_version],
                "current_version": o.current_version,
                "pinned": false,
                "pinned_version": Value::Null,
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
        return Ok(());
    }

    // `Outdated#verbose?`: version info on a TTY or with `-v`, never with `-q`.
    let verbose = (output::stdout_is_tty() || ctx.verbose) && !ctx.quiet;
    for o in &formulae {
        if verbose {
            let pinned = match &o.pinned_version {
                Some(v) if o.pinned => format!(" [pinned at {v}]"),
                _ => String::new(),
            };
            println!(
                "{} ({}) < {}{pinned}",
                o.name,
                o.installed_versions.join(", "),
                o.current_version
            );
        } else {
            println!("{}", o.name);
        }
    }
    for o in &casks {
        if verbose {
            println!(
                "{} ({}) != {}",
                o.token, o.installed_version, o.current_version
            );
        } else {
            println!("{}", o.token);
        }
    }
    Ok(())
}

pub fn missing(ctx: &Ctx, args: &MissingArgs) -> Result<()> {
    let index = ctx.index()?;
    let results = deps::missing(&ctx.cfg, index, &args.names);
    let count = if args.names.is_empty() {
        keg::installed_formula_names(&ctx.cfg).len()
    } else {
        args.names.len()
    };
    for (name, mut gone) in results {
        gone.retain(|d| !args.hide.contains(d));
        if gone.is_empty() {
            continue;
        }
        if count > 1 {
            print!("{name}: ");
        }
        println!("{}", gone.join(" "));
    }
    Ok(())
}

pub fn options(ctx: &Ctx, args: &OptionsArgs) -> Result<()> {
    // Formulae loaded from the API carry no build options, so Homebrew's
    // `next if f.options.empty?` skips every one of them.
    let _ = (ctx, args);
    Ok(())
}

pub fn which_formula(ctx: &Ctx, args: &WhichFormulaArgs) -> Result<()> {
    let index = ctx.index()?;
    let mut failed = true;
    for command in &args.commands {
        let mut providers = index.formulae_providing_executable(command);
        if providers.is_empty() {
            continue;
        }
        failed = false;
        if args.explain {
            providers.retain(|n| keg::installed_kegs(&ctx.cfg, n).is_empty());
            if providers.is_empty() {
                continue;
            }
            if providers.len() == 1 {
                println!(
                    "The program '{command}' is currently not installed. You can install it by typing:\n  brew install {}",
                    providers[0]
                );
            } else {
                println!("The program '{command}' can be found in the following formulae:");
                for p in &providers {
                    println!("  * {p}");
                }
                println!("Try: brew install <selected formula>");
            }
        } else {
            for p in providers {
                let installed = !keg::installed_kegs(&ctx.cfg, &p).is_empty();
                println!("{}", fmt::install_status(&p, installed, true));
            }
        }
    }
    if failed {
        return Err(Error::user(format!(
            "No formula provides {}.",
            args.commands.join(", ")
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metafiles_rules() {
        assert!(!metafiles_list(".DS_Store"));
        assert!(!metafiles_list("INSTALL_RECEIPT.json"));
        assert!(!metafiles_list("LICENSE"));
        assert!(!metafiles_list("COPYING"));
        assert!(!metafiles_list("README.md"));
        assert!(!metafiles_list("ChangeLog"));
        assert!(metafiles_list("sbom.spdx.json"));
        assert!(metafiles_list("wget"));
    }

    #[test]
    fn english_artifact_names() {
        assert_eq!(english_name("app"), "App");
        assert_eq!(english_name("command_wrapper"), "Command Wrapper");
        assert_eq!(english_name("bash_completion"), "Bash Completion");
    }

    #[test]
    fn tap_name_ordering() {
        let mut v: Vec<String> = ["b/c/d", "a", "z"].iter().map(|s| s.to_string()).collect();
        v.sort_by(tap_and_name_comparison);
        assert_eq!(v, vec!["a", "z", "b/c/d"]);
    }

    #[test]
    fn title_spec_strings() {
        let mut f = FormulaEntry {
            name: "jq".into(),
            stable_version: Some("1.8.2".into()),
            bottle_checksum: Some("x".into()),
            ..Default::default()
        };
        assert_eq!(title_specs(&f, None), "stable 1.8.2 (bottled)");
        assert_eq!(
            title_specs(&f, Some("1.8.1")),
            "1.8.1 → stable 1.8.2 (bottled)"
        );
        f.head_url_args = vec![json!("https://example.com/x.git")];
        assert_eq!(title_specs(&f, None), "stable 1.8.2 (bottled), HEAD");
        f.bottle_checksum = None;
        assert_eq!(title_specs(&f, None), "stable 1.8.2, HEAD");
    }

    #[test]
    fn github_urls() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(dir.path());
        let f = FormulaEntry {
            name: "hello".into(),
            tap: "homebrew/core".into(),
            ..Default::default()
        };
        assert_eq!(
            github_url(&cfg, &f),
            "https://github.com/Homebrew/homebrew-core/blob/HEAD/Formula/h/hello.rb"
        );
        let lib = FormulaEntry {
            name: "libpng".into(),
            tap: "homebrew/core".into(),
            ..Default::default()
        };
        assert_eq!(
            github_url(&cfg, &lib),
            "https://github.com/Homebrew/homebrew-core/blob/HEAD/Formula/lib/libpng.rb"
        );
        // A tap formula links to its own file in its own repository.
        let tapped = FormulaEntry {
            name: "bun".into(),
            tap: "oven-sh/bun".into(),
            ruby_source_path: Some("Formula/bun.rb".into()),
            ..Default::default()
        };
        assert_eq!(
            github_url(&cfg, &tapped),
            "https://github.com/oven-sh/homebrew-bun/blob/HEAD/Formula/bun.rb"
        );
    }

    #[test]
    fn non_github_remotes_are_joined() {
        assert_eq!(
            github_remote_path("https://example.com/x/homebrew-y", "Formula/foo.rb"),
            "https://example.com/x/homebrew-y/Formula/foo.rb"
        );
        assert_eq!(
            github_remote_path("git@github.com:user/homebrew-tap.git", "Formula/foo.rb"),
            "https://github.com/user/homebrew-tap/blob/HEAD/Formula/foo.rb"
        );
    }
}
