//! `pin`/`unpin`: `var/homebrew/pinned/<name>` symlink to the latest keg.
//!
//! Port of `FormulaPin` (`formula_pin.rb`) with the messages of `cmd/pin.rb`
//! and `cmd/unpin.rb`.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::keg;
use crate::output;

pub fn pin(cfg: &Config, name: &str) -> Result<()> {
    let record = cfg.pinned_record(name);
    if record.is_symlink() {
        output::opoo(&format!("{name} already pinned"));
        return Ok(());
    }
    let Some(latest) = keg::latest_keg(cfg, name) else {
        return Err(Error::user(format!("{name} not installed")));
    };
    std::fs::create_dir_all(cfg.pinned_kegs())?;
    // `FormulaPin#pin_at` uses a relative symlink, like every other record.
    keg::make_relative_symlink(&record, &latest.path)?;
    Ok(())
}

pub fn unpin(cfg: &Config, name: &str) -> Result<()> {
    let record = cfg.pinned_record(name);
    if record.is_symlink() {
        std::fs::remove_file(&record)?;
        // `Utils::Path.rmdir_if_possible(HOMEBREW_PINNED_KEGS)`.
        let _ = std::fs::remove_dir(cfg.pinned_kegs());
        return Ok(());
    }
    if keg::installed_kegs(cfg, name).is_empty() {
        output::onoe(&format!("{name} not installed"));
    } else {
        output::opoo(&format!("{name} not pinned"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox() -> (tempfile::TempDir, Config) {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path());
        std::fs::create_dir_all(cfg.cellar.join("jq/1.8.2")).unwrap();
        (tmp, cfg)
    }

    #[test]
    fn pins_the_latest_keg_with_a_relative_symlink() {
        let (_tmp, cfg) = sandbox();
        pin(&cfg, "jq").unwrap();
        let record = cfg.pinned_record("jq");
        assert!(record.is_symlink());
        assert_eq!(
            std::fs::read_link(&record).unwrap(),
            std::path::PathBuf::from("../../../Cellar/jq/1.8.2")
        );
        assert!(keg::is_pinned(&cfg, "jq"));

        // Pinning twice is a warning, not an error.
        pin(&cfg, "jq").unwrap();

        unpin(&cfg, "jq").unwrap();
        assert!(!record.is_symlink());
        assert!(!cfg.pinned_kegs().exists(), "the empty dir is pruned");
        // Unpinning an unpinned formula is not an error either.
        unpin(&cfg, "jq").unwrap();
    }

    #[test]
    fn pinning_an_uninstalled_formula_fails() {
        let (_tmp, cfg) = sandbox();
        let err = pin(&cfg, "wget").unwrap_err();
        assert_eq!(err.to_string(), "wget not installed");
    }
}
