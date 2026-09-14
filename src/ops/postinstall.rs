//! Declarative `post_install_steps` executor (`docs/DESIGN.md` 7) and the
//! `etc`/`var` seeding step.
//!
//! The step executor itself is [`crate::ops::steps`]; this module is the entry
//! point `brew postinstall` and the installer use. `install_etc_var` ports
//! `Formula#install_etc_var` with `InstallRenamed`'s `.default` semantics
//! (`install_renamed.rb`, `utils/path.rb#cp_path_sub`).

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::error::Result;
use crate::keg::Keg;
use crate::model::FormulaEntry;
use crate::ops::steps::{self, StepContext};

/// Run the formula's post-install work against an installed keg.
///
/// Core formulae carry declarative `post_install_steps`; a third-party tap
/// formula may instead define `post_install` in Ruby, which only the Ruby
/// `brew` can run. Skipping it silently would leave the keg half-configured,
/// so it is handed over (or reported) instead.
pub fn run_post_install(cfg: &Config, formula: &FormulaEntry, keg: &Keg) -> Result<()> {
    let steps = steps::steps_from_value(&formula.post_install_steps);
    if steps.is_empty() {
        if needs_ruby_post_install(formula) {
            return delegate_post_install(cfg, formula);
        }
        return Ok(());
    }
    let ctx = StepContext::new(cfg, formula, keg);
    steps::run(&ctx, &steps)
}

/// Whether the formula has post-install work of either kind.
pub fn has_post_install(formula: &FormulaEntry) -> bool {
    !formula.post_install_steps.is_empty() || needs_ruby_post_install(formula)
}

/// A Ruby `post_install` with nothing declarative to run in its place.
pub fn needs_ruby_post_install(formula: &FormulaEntry) -> bool {
    formula.post_install_defined && formula.post_install_steps.is_empty()
}

/// Run `brew postinstall <formula>` for a Ruby `post_install`.
///
/// Without a `brew` to hand it to there is nothing fastbrew can do, so it says
/// what is missing and makes the command exit 1. The keg stays: it is
/// installed and linked, only its post-install step is outstanding.
fn delegate_post_install(cfg: &Config, formula: &FormulaEntry) -> Result<()> {
    let full = formula.full_name();
    let args = [
        std::ffi::OsString::from("postinstall"),
        std::ffi::OsString::from(&full),
    ];
    let reason = format!("post-install of {full}");
    match crate::delegate::run_brew(cfg, &args, &reason, false) {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(crate::error::Error::user(format!(
            "`brew postinstall {full}` exited with {}.",
            status.code().unwrap_or(-1)
        ))),
        Err(_) => {
            crate::output::opoo(&format!(
                "{full}'s post_install requires the Ruby formula DSL; run: brew postinstall {full}"
            ));
            crate::output::set_failed();
            Ok(())
        }
    }
}

/// `Formula#install_etc_var`: copy `<keg>/.bottle/etc` and `<keg>/.bottle/var`
/// into the prefix, renaming a file to `<name>.default` when a different file
/// is already there.
pub fn install_etc_var(cfg: &Config, keg: &Keg) -> Result<()> {
    let bottle_prefix = keg.path.join(".bottle");
    for sub in ["etc", "var"] {
        let source_root = bottle_prefix.join(sub);
        if !source_root.is_dir() {
            continue;
        }
        copy_tree(cfg, &bottle_prefix, &source_root)?;
    }
    Ok(())
}

/// `Find.find(...)` + `InstallRenamed.cp_path_sub(path, bottle_prefix, HOMEBREW_PREFIX)`.
fn copy_tree(cfg: &Config, bottle_prefix: &Path, root: &Path) -> Result<()> {
    for entry in walkdir::WalkDir::new(root).sort_by_file_name() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let src = entry.path();
        let Ok(relative) = src.strip_prefix(bottle_prefix) else {
            continue;
        };
        let dst = cfg.prefix.join(relative);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&dst)?;
            continue;
        }
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let dst = destination_for(cfg, src, &dst);
        copy_file(src, &dst)?;
    }
    Ok(())
}

/// `InstallRenamed.destination_for`.
///
/// A destination that does not exist (or is not a regular file) is used as is.
/// An identical destination is overwritten in place. Otherwise the new default
/// is written beside it as `<dst>.default` — unless the live file still matches
/// the default shipped by another keg of the same formula, in which case the
/// untouched config is advanced to the new default.
pub fn destination_for(cfg: &Config, src: &Path, dst: &Path) -> PathBuf {
    if !dst.is_file() {
        return dst.to_path_buf();
    }
    if identical(src, dst) {
        return dst.to_path_buf();
    }
    if matches_a_sibling_kegs_default(cfg, src, dst) {
        return dst.to_path_buf();
    }
    PathBuf::from(format!("{}.default", dst.display()))
}

/// The `src.ascend` walk of `InstallRenamed.destination_for`: find the `.bottle`
/// directory of the keg being installed, then compare `dst` with the same file
/// under every *other* keg of that rack.
fn matches_a_sibling_kegs_default(cfg: &Config, src: &Path, dst: &Path) -> bool {
    let Ok(cellar) = std::fs::canonicalize(&cfg.cellar) else {
        return false;
    };
    // Resolve symlinks so the walk goes through the Cellar, not through `opt`.
    let src = match std::fs::canonicalize(src) {
        Ok(p) => p,
        Err(_) => src.to_path_buf(),
    };
    let mut current: &Path = &src;
    while let Some(parent) = current.parent() {
        current = parent;
        if current.file_name() != Some(std::ffi::OsStr::new(".bottle")) {
            continue;
        }
        // `<cellar>/<name>/<version>/.bottle`: the rack must be the Cellar's child.
        let Some(keg) = current.parent() else { break };
        let Some(rack) = keg.parent() else { break };
        if rack.parent() != Some(cellar.as_path()) {
            break;
        }
        let Ok(relative) = src.strip_prefix(current) else {
            break;
        };
        let Ok(siblings) = std::fs::read_dir(rack) else {
            break;
        };
        for sibling in siblings.flatten() {
            if sibling.path() == keg {
                continue;
            }
            let default_file = sibling.path().join(".bottle").join(relative);
            if default_file.is_file() && identical(&default_file, dst) {
                return true;
            }
        }
        break;
    }
    false
}

fn identical(a: &Path, b: &Path) -> bool {
    match (std::fs::read(a), std::fs::read(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// `FileUtils.cp`: follows a symlinked source, overwrites the destination.
fn copy_file(src: &Path, dst: &Path) -> Result<()> {
    if dst.is_symlink() || dst.exists() {
        let _ = std::fs::remove_file(dst);
    }
    if src.is_symlink() && !src.exists() {
        // A broken symlink is copied as a symlink; `FileUtils.cp` would fail.
        let target = std::fs::read_link(src)?;
        std::os::unix::fs::symlink(target, dst)?;
        return Ok(());
    }
    std::fs::copy(src, dst)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keg_with_bottle_etc(root: &Path, version: &str, body: &str) -> (Config, Keg) {
        let cfg = Config::for_test(root);
        let keg = Keg::new(&cfg, "demo", version);
        let etc = keg.path.join(".bottle/etc/demo");
        std::fs::create_dir_all(&etc).unwrap();
        std::fs::write(etc.join("demo.conf"), body).unwrap();
        std::fs::create_dir_all(keg.path.join(".bottle/var/demo")).unwrap();
        std::fs::write(keg.path.join(".bottle/var/demo/seed"), "seed").unwrap();
        std::fs::create_dir_all(cfg.prefix.join("etc")).unwrap();
        (cfg, keg)
    }

    #[test]
    fn seeds_etc_and_var_then_uses_default_for_modified_files() {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg, keg) = keg_with_bottle_etc(tmp.path(), "1.0", "a = 1\n");
        install_etc_var(&cfg, &keg).unwrap();
        let conf = cfg.prefix.join("etc/demo/demo.conf");
        assert_eq!(std::fs::read_to_string(&conf).unwrap(), "a = 1\n");
        assert_eq!(
            std::fs::read_to_string(cfg.prefix.join("var/demo/seed")).unwrap(),
            "seed"
        );

        // An unchanged config identical to the shipped default is left alone.
        install_etc_var(&cfg, &keg).unwrap();
        assert!(!conf.with_extension("conf.default").exists());

        // A config the user edited keeps its content; the new default lands
        // beside it as `<name>.default`.
        std::fs::write(&conf, "a = 99\n").unwrap();
        let (_, keg2) = keg_with_bottle_etc(tmp.path(), "2.0", "a = 2\n");
        install_etc_var(&cfg, &keg2).unwrap();
        assert_eq!(std::fs::read_to_string(&conf).unwrap(), "a = 99\n");
        assert_eq!(
            std::fs::read_to_string(cfg.prefix.join("etc/demo/demo.conf.default")).unwrap(),
            "a = 2\n"
        );
    }

    #[test]
    fn an_untouched_older_default_is_advanced_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        // 1.0 ships `a = 1` and is installed, so `etc` holds exactly that.
        let (cfg, keg1) = keg_with_bottle_etc(tmp.path(), "1.0", "a = 1\n");
        install_etc_var(&cfg, &keg1).unwrap();
        let conf = cfg.prefix.join("etc/demo/demo.conf");

        // 2.0 ships a different default. Because the live file still matches
        // 1.0's default, Homebrew replaces it rather than writing `.default`.
        let (_, keg2) = keg_with_bottle_etc(tmp.path(), "2.0", "a = 2\n");
        install_etc_var(&cfg, &keg2).unwrap();
        assert_eq!(std::fs::read_to_string(&conf).unwrap(), "a = 2\n");
        assert!(!cfg.prefix.join("etc/demo/demo.conf.default").exists());
    }

    #[test]
    fn a_keg_without_a_bottle_directory_is_a_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path());
        let keg = Keg::new(&cfg, "plain", "1.0");
        std::fs::create_dir_all(&keg.path).unwrap();
        install_etc_var(&cfg, &keg).unwrap();
        assert!(!cfg.prefix.join("etc").exists());
    }
}
