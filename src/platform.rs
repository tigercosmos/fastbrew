//! Host platform detection: macOS version, CPU, bottle tag.
//!
//! Mirrors `utils/os.sh`, `macos_version.rb` and `utils/bottles.rb`.

use std::fmt;
use std::process::Command;
use std::sync::OnceLock;

/// macOS release names by major version (`MacOSVersion::RELEASES`).
pub const MACOS_RELEASES: &[(u32, &str)] = &[
    (27, "golden_gate"),
    (26, "tahoe"),
    (15, "sequoia"),
    (14, "sonoma"),
    (13, "ventura"),
    (12, "monterey"),
    (11, "big_sur"),
];

/// Padded build prefix used by newer bottles (`Homebrew::MACOS_ARM64_BOTTLE_PREFIX`).
pub fn padded_prefix_macos_arm64() -> String {
    format!("{:_<64}", "/opt/homebrew/.brew-padded-arm64")
}

pub fn padded_prefix_linux_arm64() -> String {
    format!("{:_<64}", "/home/linuxbrew/.linuxbrew/.brew-padded-arm64")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Arch {
    Arm64,
    X86_64,
}

impl Arch {
    pub fn as_str(self) -> &'static str {
        match self {
            Arch::Arm64 => "arm64",
            Arch::X86_64 => "x86_64",
        }
    }
}

/// A parsed macOS product version, e.g. 26.4.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct MacOsVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl MacOsVersion {
    pub fn parse(s: &str) -> Option<Self> {
        let mut it = s.trim().split('.').map(|p| p.parse::<u32>().ok());
        let major = it.next()??;
        let minor = it.next().flatten().unwrap_or(0);
        let patch = it.next().flatten().unwrap_or(0);
        // Big Sur may report 10.16 under SYSTEM_VERSION_COMPAT.
        if major == 10 && minor == 16 {
            return Some(Self {
                major: 11,
                minor: 0,
                patch: 0,
            });
        }
        Some(Self {
            major,
            minor,
            patch,
        })
    }

    /// Release name (`tahoe`), or `None` for unsupported versions.
    pub fn release_name(&self) -> Option<&'static str> {
        MACOS_RELEASES
            .iter()
            .find(|(m, _)| *m == self.major)
            .map(|(_, n)| *n)
    }

    /// Major version for a release symbol such as `sequoia`.
    pub fn major_for_symbol(sym: &str) -> Option<u32> {
        MACOS_RELEASES
            .iter()
            .find(|(_, n)| *n == sym)
            .map(|(m, _)| *m)
    }

    /// `HOMEBREW_MACOS_VERSION_NUMERIC`-style number, e.g. 260401.
    pub fn numeric(&self) -> u32 {
        self.major * 10_000 + self.minor * 100 + self.patch
    }
}

impl fmt::Display for MacOsVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)?;
        if self.patch > 0 {
            write!(f, ".{}", self.patch)?;
        }
        Ok(())
    }
}

/// A bottle tag such as `arm64_tahoe`, `sonoma`, `x86_64_linux` or `all`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BottleTag {
    pub arch: Option<Arch>,
    /// `tahoe`, `linux`, or `all`.
    pub system: String,
}

impl BottleTag {
    pub fn all() -> Self {
        BottleTag {
            arch: None,
            system: "all".into(),
        }
    }

    /// Parse `arm64_tahoe`, `tahoe`, `x86_64_linux`, `all` (with or without a leading colon).
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.strip_prefix(':').unwrap_or(s);
        if s == "all" {
            return Some(Self::all());
        }
        if let Some(rest) = s.strip_prefix("arm64_") {
            return Some(BottleTag {
                arch: Some(Arch::Arm64),
                system: rest.to_string(),
            });
        }
        if let Some(rest) = s.strip_prefix("x86_64_") {
            return Some(BottleTag {
                arch: Some(Arch::X86_64),
                system: rest.to_string(),
            });
        }
        if s.is_empty() || s.contains('/') {
            return None;
        }
        Some(BottleTag {
            arch: Some(Arch::X86_64),
            system: s.to_string(),
        })
    }

    pub fn is_linux(&self) -> bool {
        self.system == "linux"
    }

    pub fn is_all(&self) -> bool {
        self.system == "all"
    }

    /// Default Cellar for bottles built for this tag.
    pub fn default_cellar(&self) -> &'static str {
        match (self.is_linux(), self.arch) {
            (true, _) => "/home/linuxbrew/.linuxbrew/Cellar",
            (false, Some(Arch::Arm64)) => "/opt/homebrew/Cellar",
            _ => "/usr/local/Cellar",
        }
    }

    pub fn default_prefix(&self) -> &'static str {
        match (self.is_linux(), self.arch) {
            (true, _) => "/home/linuxbrew/.linuxbrew",
            (false, Some(Arch::Arm64)) => "/opt/homebrew",
            _ => "/usr/local",
        }
    }

    /// Padded build prefix for this tag, when bottles use one.
    pub fn padded_prefix(&self) -> Option<String> {
        match (self.is_linux(), self.arch) {
            (true, Some(Arch::Arm64)) => Some(padded_prefix_linux_arm64()),
            (false, Some(Arch::Arm64)) => Some(padded_prefix_macos_arm64()),
            _ => None,
        }
    }
}

impl fmt::Display for BottleTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.arch {
            None => write!(f, "{}", self.system),
            Some(Arch::Arm64) => write!(f, "arm64_{}", self.system),
            Some(Arch::X86_64) if self.is_linux() => write!(f, "x86_64_{}", self.system),
            Some(Arch::X86_64) => write!(f, "{}", self.system),
        }
    }
}

/// What this process is running on.
#[derive(Debug, Clone)]
pub struct Host {
    pub arch: Arch,
    pub macos: Option<MacOsVersion>,
    pub linux: bool,
}

impl Host {
    pub fn detect() -> &'static Host {
        static HOST: OnceLock<Host> = OnceLock::new();
        HOST.get_or_init(|| {
            let arch = if cfg!(target_arch = "aarch64") {
                Arch::Arm64
            } else {
                Arch::X86_64
            };
            let linux = cfg!(target_os = "linux");
            let macos = if linux { None } else { detect_macos_version() };
            Host { arch, macos, linux }
        })
    }

    /// Bottle tag for this host (`arm64_tahoe`); `HOMEBREW_BOTTLE_TAG` may override for tests.
    pub fn bottle_tag(&self) -> BottleTag {
        if let Some(t) = std::env::var("FASTBREW_BOTTLE_TAG")
            .ok()
            .and_then(|s| BottleTag::parse(&s))
        {
            return t;
        }
        if self.linux {
            return BottleTag {
                arch: Some(self.arch),
                system: "linux".into(),
            };
        }
        let name = self
            .macos
            .and_then(|v| v.release_name())
            .unwrap_or("unknown");
        BottleTag {
            arch: Some(self.arch),
            system: name.to_string(),
        }
    }

    /// `HOMEBREW_SYSTEM` as written into receipts (`Macintosh`/`Linux`).
    pub fn system_name(&self) -> &'static str {
        if self.linux { "Linux" } else { "Macintosh" }
    }

    /// `os_version` as written into receipts (`macOS 26`).
    pub fn os_version_string(&self) -> String {
        match self.macos {
            Some(v) => format!("macOS {}", v.major),
            None => "Linux".to_string(),
        }
    }
}

fn detect_macos_version() -> Option<MacOsVersion> {
    if let Ok(v) = std::env::var("FASTBREW_MACOS_VERSION") {
        return MacOsVersion::parse(&v);
    }
    // Fast path: parse SystemVersion.plist without spawning sw_vers.
    if let Ok(text) = std::fs::read_to_string("/System/Library/CoreServices/SystemVersion.plist")
        && let Some(idx) = text.find("<key>ProductVersion</key>")
    {
        let rest = &text[idx..];
        if let Some(start) = rest.find("<string>") {
            let rest = &rest[start + 8..];
            if let Some(end) = rest.find("</string>")
                && let Some(v) = MacOsVersion::parse(&rest[..end])
            {
                return Some(v);
            }
        }
    }
    let out = Command::new("/usr/bin/sw_vers")
        .arg("-productVersion")
        .output()
        .ok()?;
    MacOsVersion::parse(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tags() {
        assert_eq!(
            BottleTag::parse("arm64_tahoe").unwrap().to_string(),
            "arm64_tahoe"
        );
        assert_eq!(
            BottleTag::parse(":arm64_sequoia").unwrap().to_string(),
            "arm64_sequoia"
        );
        assert_eq!(BottleTag::parse("sonoma").unwrap().to_string(), "sonoma");
        assert_eq!(
            BottleTag::parse("x86_64_linux").unwrap().to_string(),
            "x86_64_linux"
        );
        assert!(BottleTag::parse("all").unwrap().is_all());
    }

    #[test]
    fn macos_versions() {
        let v = MacOsVersion::parse("26.4.1").unwrap();
        assert_eq!(v.release_name(), Some("tahoe"));
        assert_eq!(v.numeric(), 260401);
        assert_eq!(MacOsVersion::parse("10.16").unwrap().major, 11);
    }

    #[test]
    fn padded_prefix_is_64_bytes() {
        assert_eq!(padded_prefix_macos_arm64().len(), 64);
    }
}
