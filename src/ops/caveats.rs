//! Formula caveats: a port of `Library/Homebrew/caveats.rb`.
//!
//! `Caveats#caveats` joins the formula's own caveat text, the keg-only block
//! and the service block with a blank line; `completions_and_elisp` is built
//! separately by inspecting the installed keg, and Homebrew prints it above the
//! recorded per-formula caveats (`messages.rb#display_caveats`).

use std::path::Path;

use crate::config::Config;
use crate::keg::Keg;
use crate::model::{FormulaEntry, KegOnly};
use crate::services::plist::ServiceDef;

/// Everything `==> Caveats` shows for one freshly installed formula.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Caveats {
    /// Formula caveats, keg-only text and service text, joined with `\n`.
    pub text: Option<String>,
    /// Completion and Emacs Lisp notices, which Homebrew prints once per run.
    pub completions_and_elisp: Vec<String>,
}

impl Caveats {
    pub fn is_empty(&self) -> bool {
        self.text.is_none() && self.completions_and_elisp.is_empty()
    }
}

/// `Caveats.new(formula)` for a keg that has just been installed.
pub fn caveats(cfg: &Config, formula: &FormulaEntry, keg: &Keg) -> Caveats {
    let linked = keg.is_linked(cfg);
    let mut blocks: Vec<String> = Vec::new();
    if let Some(text) = formula_caveats(cfg, formula, keg) {
        blocks.push(format!("{}\n", text.trim_end_matches('\n')));
    }
    if let Some(text) = keg_only_text(cfg, formula, keg, linked) {
        blocks.push(text);
    }
    if let Some(text) = service_caveats(cfg, formula, keg) {
        blocks.push(text);
    }
    let text = (!blocks.is_empty()).then(|| blocks.join("\n"));
    Caveats {
        text,
        completions_and_elisp: completions_and_elisp(cfg, formula, keg),
    }
}

/// The formula's own `caveats` string with Homebrew's placeholders expanded.
///
/// The internal API serialises the block's result with `#{HOMEBREW_PREFIX}`
/// and friends already interpolated as `$HOMEBREW_PREFIX`-style markers, plus
/// `#{prefix}`/`#{opt_prefix}` for the keg itself.
pub fn formula_caveats(cfg: &Config, formula: &FormulaEntry, keg: &Keg) -> Option<String> {
    let raw = formula.caveats.as_deref()?;
    if raw.trim().is_empty() {
        return None;
    }
    let opt = cfg.opt_record(&formula.name);
    let expanded = cfg
        .expand_placeholders(raw)
        .replace("$HOMEBREW_REPOSITORY", &cfg.repository.to_string_lossy())
        .replace("#{prefix}", &keg.path.to_string_lossy())
        .replace("#{opt_prefix}", &opt.to_string_lossy())
        .replace("#{version}", &keg.version.to_string())
        .replace("#{name}", &formula.name)
        .replace("#{full_name}", &formula.full_name())
        .replace("#{HOMEBREW_PREFIX}", &cfg.prefix.to_string_lossy())
        .replace("#{HOMEBREW_CELLAR}", &cfg.cellar.to_string_lossy())
        .replace("#{etc}", &cfg.prefix.join("etc").to_string_lossy())
        .replace("#{var}", &cfg.prefix.join("var").to_string_lossy());
    Some(expanded)
}

/// `Caveats#keg_only_text`.
pub fn keg_only_text(
    cfg: &Config,
    formula: &FormulaEntry,
    keg: &Keg,
    linked: bool,
) -> Option<String> {
    let (reason, extra) = formula.keg_only()?;
    if linked {
        return None;
    }
    let explanation = explanation_for(&reason, extra.as_deref(), cfg);
    let opt = cfg.opt_record(&formula.name);
    let mut s = format!(
        "{} is keg-only, which means it was not symlinked into {},\nbecause {}.\n",
        formula.name,
        cfg.prefix.display(),
        explanation.trim_end_matches('\n')
    );

    let has = |sub: &str| keg.path.join(sub).is_dir();
    if has("bin") || has("sbin") {
        s.push_str(&format!(
            "\nIf you need to have {} first in your PATH, run:\n",
            formula.name
        ));
        if has("bin") {
            s.push_str(&format!(
                "  {}\n",
                prepend_path_in_profile(cfg, &opt.join("bin"))
            ));
        }
        if has("sbin") {
            s.push_str(&format!(
                "  {}\n",
                prepend_path_in_profile(cfg, &opt.join("sbin"))
            ));
        }
    }

    if has("lib") || has("include") {
        s.push_str(&format!(
            "\nFor compilers to find {} you may need to set:\n",
            formula.name
        ));
        if has("lib") {
            s.push_str(&format!(
                "  {}\n",
                export_value("LDFLAGS", &format!("-L{}", opt.join("lib").display()))
            ));
        }
        if has("include") {
            s.push_str(&format!(
                "  {}\n",
                export_value("CPPFLAGS", &format!("-I{}", opt.join("include").display()))
            ));
        }
        let pkgconfig_lib = keg.path.join("lib/pkgconfig").is_dir();
        let pkgconfig_share = keg.path.join("share/pkgconfig").is_dir();
        if which("pkgconf").is_some() && (pkgconfig_lib || pkgconfig_share) {
            s.push_str(&format!(
                "\nFor pkgconf to find {} you may need to set:\n",
                formula.name
            ));
            if pkgconfig_lib {
                s.push_str(&format!(
                    "  {}\n",
                    export_value(
                        "PKG_CONFIG_PATH",
                        &format!("{}/pkgconfig", opt.join("lib").display())
                    )
                ));
            }
            if pkgconfig_share {
                s.push_str(&format!(
                    "  {}\n",
                    export_value(
                        "PKG_CONFIG_PATH",
                        &format!("{}/pkgconfig", opt.join("share").display())
                    )
                ));
            }
        }
        let cmake_lib = keg.path.join("lib/cmake").is_dir();
        let cmake_share = keg.path.join("share/cmake").is_dir();
        if which("cmake").is_some() && (cmake_lib || cmake_share) {
            s.push_str(&format!(
                "\nFor cmake to find {} you may need to set:\n  {}\n",
                formula.name,
                export_value("CMAKE_PREFIX_PATH", &opt.to_string_lossy())
            ));
        }
    }
    if !s.ends_with('\n') {
        s.push('\n');
    }
    Some(s)
}

fn explanation_for(reason: &KegOnly, extra: Option<&str>, cfg: &Config) -> String {
    cfg.expand_placeholders(&reason.explanation(extra))
}

/// `Caveats#function_completion_caveats` and `#elisp_caveats`.
pub fn completions_and_elisp(cfg: &Config, formula: &FormulaEntry, keg: &Keg) -> Vec<String> {
    completions_and_elisp_for_shells(cfg, formula, keg, &preferred_shells())
}

/// [`completions_and_elisp`] for an explicit list of shells (the caller's
/// preferred shells in production; fixed in tests so `$SHELL` cannot change
/// the outcome).
pub fn completions_and_elisp_for_shells(
    cfg: &Config,
    formula: &FormulaEntry,
    keg: &Keg,
    shells: &[&'static str],
) -> Vec<String> {
    let keg_only = formula.is_keg_only();
    let root = if keg_only {
        cfg.opt_record(&formula.name)
    } else {
        cfg.prefix.clone()
    };
    let root = root.display().to_string();
    let mut out = Vec::new();

    // Homebrew narrows this to the caller's own shell when it recognises it;
    // an unknown shell (or none, as when fastbrew's output is piped) checks all
    // four, which is also what a non-interactive `brew install` does.
    for &shell in shells {
        let completion = completion_installed(keg, shell);
        let functions = functions_installed(keg, shell);
        if !completion && !functions {
            continue;
        }
        if which(shell).is_none() {
            continue;
        }
        let mut installed: Vec<&str> = Vec::new();
        if completion {
            installed.push("completions");
        }
        if functions {
            installed.push("functions");
        }
        let installed = installed.join(" and ");
        out.push(match shell {
            "bash" => {
                format!("Bash completion has been installed to:\n  {root}/etc/bash_completion.d\n")
            }
            "fish" => {
                let mut s = format!("fish {installed} have been installed to:");
                if completion {
                    s.push_str(&format!("\n  {root}/share/fish/vendor_completions.d"));
                }
                if functions {
                    s.push_str(&format!("\n  {root}/share/fish/vendor_functions.d"));
                }
                s
            }
            "zsh" => format!(
                "zsh {installed} have been installed to:\n  {root}/share/zsh/site-functions\n"
            ),
            _ => format!(
                "PowerShell completion has been installed to:\n  {root}/share/pwsh/completions\n"
            ),
        });
    }

    if !keg_only && elisp_installed(keg) {
        out.push(format!(
            "Emacs Lisp files have been installed to:\n  {}/share/emacs/site-lisp/{}\n",
            cfg.prefix.display(),
            formula.name
        ));
    }
    out
}

/// `Utils::Shell.preferred || Utils::Shell.parent`, falling back to all of them.
fn preferred_shells() -> Vec<&'static str> {
    const VALID: [&str; 4] = ["bash", "zsh", "fish", "pwsh"];
    let shell = std::env::var("SHELL").unwrap_or_default();
    let basename = shell.rsplit('/').next().unwrap_or("");
    match VALID.iter().find(|s| **s == basename) {
        Some(s) => vec![*s],
        None => VALID.to_vec(),
    }
}

/// `Keg#completion_installed?`.
fn completion_installed(keg: &Keg, shell: &str) -> bool {
    let dir = match shell {
        "bash" => keg.path.join("etc/bash_completion.d"),
        "fish" => keg.path.join("share/fish/vendor_completions.d"),
        "pwsh" => keg.path.join("share/pwsh/completions"),
        "zsh" => {
            let dir = keg.path.join("share/zsh/site-functions");
            if !children(&dir).iter().any(|n| n.starts_with('_')) {
                return false;
            }
            dir
        }
        _ => return false,
    };
    !children(&dir).is_empty()
}

/// `Keg#functions_installed?`.
fn functions_installed(keg: &Keg, shell: &str) -> bool {
    match shell {
        "fish" => !children(&keg.path.join("share/fish/vendor_functions.d")).is_empty(),
        "zsh" => children(&keg.path.join("share/zsh/site-functions"))
            .iter()
            .any(|n| !n.starts_with('_')),
        _ => false,
    }
}

/// `Keg#elisp_installed?` (`Keg::ELISP_EXTENSIONS`).
fn elisp_installed(keg: &Keg) -> bool {
    let dir = keg.path.join("share/emacs/site-lisp").join(&keg.name);
    children(&dir)
        .iter()
        .any(|n| n.ends_with(".el") || n.ends_with(".elc"))
}

fn children(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// `Caveats#service_caveats`, for the bottled (launchd/systemd) case.
pub fn service_caveats(cfg: &Config, formula: &FormulaEntry, keg: &Keg) -> Option<String> {
    // `return if !formula.service? && ... && !keg&.plist_installed?`: without a
    // service definition there is nothing to say, even when the keg ships a
    // plist of its own.
    let _ = keg;
    let def = ServiceDef::from_formula(cfg, formula)?;
    let full_name = formula.full_name();
    let mut lines = Vec::new();
    // A freshly installed formula is never already running under launchd, so
    // only the "start now" branches apply. `requires_root?` picks the daemon
    // wording.
    if def.require_root {
        lines.push(format!("To start {full_name} now and restart at startup:"));
        lines.push(format!("  sudo brew services start {full_name}"));
    } else {
        lines.push(format!("To start {full_name} now and restart at login:"));
        lines.push(format!("  brew services start {full_name}"));
    }
    if !def.command(cfg).is_empty() {
        lines.push("Or, if you don't want/need a background service you can just run:".to_string());
        lines.push(format!("  {}", def.manual_command(cfg)));
    }
    Some(format!("{}\n", lines.join("\n")))
}

/// `Utils::Shell.prepend_path_in_profile` for the shell we are running under.
fn prepend_path_in_profile(_cfg: &Config, path: &Path) -> String {
    let path = path.display();
    match preferred_shells().first().copied().unwrap_or("bash") {
        "fish" => format!("fish_add_path {path}"),
        "pwsh" => format!("$env:PATH = \"{path}:\" + $env:PATH"),
        _ => format!("export PATH=\"{path}:$PATH\""),
    }
}

/// `Utils::Shell.export_value`.
fn export_value(key: &str, value: &str) -> String {
    match preferred_shells().first().copied().unwrap_or("bash") {
        "fish" => format!("set -gx {key} \"{value}\""),
        "pwsh" => format!("$env:{key} = \"{value}\""),
        _ => format!("export {key}=\"{value}\""),
    }
}

/// `which(name, ORIGINAL_PATHS)`: look the tool up on `PATH`.
fn which(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keg::Keg;

    fn keg_with(root: &Path, dirs: &[&str]) -> (Config, Keg) {
        let cfg = Config::for_test(root);
        let keg = Keg::new(&cfg, "demo", "1.0");
        for d in dirs {
            std::fs::create_dir_all(keg.path.join(d)).unwrap();
        }
        std::fs::create_dir_all(&keg.path).unwrap();
        (cfg, keg)
    }

    #[test]
    fn service_block_offers_brew_services_and_the_manual_command() {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg, keg) = keg_with(tmp.path(), &["bin"]);
        let mut formula = FormulaEntry {
            name: "demo".into(),
            service_run_args: vec![serde_json::json!([
                "$HOMEBREW_PREFIX/opt/demo/bin/demod",
                "--config",
                "$HOMEBREW_PREFIX/etc/demo.conf"
            ])],
            ..Default::default()
        };
        let text = service_caveats(&cfg, &formula, &keg).unwrap();
        assert_eq!(
            text,
            format!(
                "To start demo now and restart at login:\n  \
                 brew services start demo\n\
                 Or, if you don't want/need a background service you can just run:\n  \
                 {prefix}/opt/demo/bin/demod --config {prefix}/etc/demo.conf\n",
                prefix = cfg.prefix.display()
            )
        );

        // `require_root` switches to the daemon wording.
        formula.service_args = vec![serde_json::json!([":require_root", true])];
        let text = service_caveats(&cfg, &formula, &keg).unwrap();
        assert!(
            text.starts_with(
                "To start demo now and restart at startup:\n  sudo brew services start demo\n"
            ),
            "{text}"
        );

        // A formula with no runnable service says nothing.
        let plain = FormulaEntry {
            name: "demo".into(),
            ..Default::default()
        };
        assert_eq!(service_caveats(&cfg, &plain, &keg), None);
    }

    #[test]
    fn keg_only_block_lists_the_paths_that_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg, keg) = keg_with(tmp.path(), &["bin", "lib", "include"]);
        let formula = FormulaEntry {
            name: "demo".into(),
            keg_only_args: vec![serde_json::json!(":versioned_formula")],
            ..Default::default()
        };
        let text = keg_only_text(&cfg, &formula, &keg, false).unwrap();
        assert!(
            text.starts_with(&format!(
                "demo is keg-only, which means it was not symlinked into {},\nbecause this is an alternate version of another formula.\n",
                cfg.prefix.display()
            )),
            "{text}"
        );
        assert!(text.contains("If you need to have demo first in your PATH, run:"));
        assert!(text.contains("For compilers to find demo you may need to set:"));
        assert!(text.contains("LDFLAGS"), "{text}");
        assert!(text.contains("CPPFLAGS"), "{text}");
        // A linked keg gets no keg-only caveat at all.
        std::fs::create_dir_all(cfg.linked_kegs()).unwrap();
        assert_eq!(keg_only_text(&cfg, &formula, &keg, true), None);
    }

    #[test]
    fn completions_are_reported_from_the_keg() {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg, keg) = keg_with(tmp.path(), &["share/zsh/site-functions"]);
        std::fs::write(keg.path.join("share/zsh/site-functions/_demo"), "#compdef").unwrap();
        let formula = FormulaEntry {
            name: "demo".into(),
            ..Default::default()
        };
        // Only assert when a zsh is on PATH, which is the macOS default. The
        // shell list is explicit so the caller's `$SHELL` cannot narrow it.
        if which("zsh").is_some() {
            let notes = completions_and_elisp_for_shells(&cfg, &formula, &keg, &["zsh"]);
            assert!(
                notes.iter().any(
                    |n| n.starts_with("zsh completions have been installed to:\n  ")
                        && n.contains("share/zsh/site-functions")
                ),
                "{notes:?}"
            );
        }
    }

    #[test]
    fn formula_caveats_expand_placeholders() {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg, keg) = keg_with(tmp.path(), &[]);
        let formula = FormulaEntry {
            name: "demo".into(),
            caveats: Some("Config goes in $HOMEBREW_PREFIX/etc and #{opt_prefix}/share.".into()),
            ..Default::default()
        };
        let text = formula_caveats(&cfg, &formula, &keg).unwrap();
        assert_eq!(
            text,
            format!(
                "Config goes in {}/etc and {}/share.",
                cfg.prefix.display(),
                cfg.opt_record("demo").display()
            )
        );
    }
}
