//! Homebrew version semantics (`docs/COMPAT.md` 8). Port of
//! `Library/Homebrew/version.rb` and `pkg_version.rb`.
//!
//! Required API:
//! - `Version::new(&str)`, `Version::head()`, `is_head()`, `Display`.
//! - `impl Ord for Version` with Homebrew's token rules.
//! - `Version::detect_from_url(url)` (used by rubylite when a formula has no
//!   explicit version; port `Version.parse`/`detect`).
//! - `PkgVersion { version, revision }`, `PkgVersion::parse("1.2_3")`,
//!   `Display` (`1.2_3`), `Ord`.
//!
//! Tests must port the comparison cases from `Library/Homebrew/test/version_spec.rb`.

use std::cmp::Ordering;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Version {
    raw: String,
}

impl Version {
    pub fn new(s: &str) -> Self {
        Version { raw: s.to_string() }
    }

    pub fn head() -> Self {
        Version {
            raw: "HEAD".to_string(),
        }
    }

    pub fn is_head(&self) -> bool {
        self.raw == "HEAD" || self.raw.starts_with("HEAD-")
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// Port of `Version.detect(url, **specs)` / `Version.parse`.
    pub fn detect_from_url(_url: &str) -> Option<Version> {
        todo!("version::Version::detect_from_url")
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, _other: &Self) -> Ordering {
        todo!("version::Version::cmp (port of Version#<=>)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PkgVersion {
    pub version: Version,
    pub revision: u32,
}

impl PkgVersion {
    pub fn new(version: Version, revision: u32) -> Self {
        PkgVersion { version, revision }
    }

    /// Parse `1.2.3` or `1.2.3_4` (the last `_<digits>` is the revision).
    pub fn parse(s: &str) -> Self {
        if let Some((v, r)) = s.rsplit_once('_')
            && let Ok(rev) = r.parse::<u32>()
        {
            return PkgVersion {
                version: Version::new(v),
                revision: rev,
            };
        }
        PkgVersion {
            version: Version::new(s),
            revision: 0,
        }
    }
}

impl fmt::Display for PkgVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.revision > 0 {
            write!(f, "{}_{}", self.version, self.revision)
        } else {
            write!(f, "{}", self.version)
        }
    }
}

impl PartialOrd for PkgVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PkgVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        self.version
            .cmp(&other.version)
            .then(self.revision.cmp(&other.revision))
    }
}
