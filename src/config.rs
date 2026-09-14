//! Resolution of Homebrew's environment and paths.
//!
//! Mirrors `bin/brew`, `Library/Homebrew/brew.sh`, `utils/os.sh` and
//! `startup/config.rb`. Every path fastbrew touches comes from here.

use std::env;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Default prefix per platform (`HOMEBREW_MACOS_ARM_DEFAULT_PREFIX` etc.).
pub const DEFAULT_PREFIX_MACOS_ARM64: &str = "/opt/homebrew";
pub const DEFAULT_PREFIX_MACOS_X86_64: &str = "/usr/local";
pub const DEFAULT_PREFIX_LINUX: &str = "/home/linuxbrew/.linuxbrew";

pub const DEFAULT_API_DOMAIN: &str = "https://formulae.brew.sh/api";
pub const DEFAULT_BOTTLE_DOMAIN: &str = "https://ghcr.io/v2/homebrew/core";
pub const DEFAULT_BREW_GIT_REMOTE: &str = "https://github.com/Homebrew/brew";

/// Fully resolved configuration for one invocation.
#[derive(Debug, Clone)]
pub struct Config {
    pub prefix: PathBuf,
    pub cellar: PathBuf,
    pub repository: PathBuf,
    pub library: PathBuf,
    pub cache: PathBuf,
    pub logs: PathBuf,
    pub temp: PathBuf,
    pub home: PathBuf,
    pub api_domain: String,
    pub bottle_domain: String,
    /// `HOMEBREW_ARTIFACT_DOMAIN`, an OCI proxy replacing `https://ghcr.io`.
    pub artifact_domain: Option<String>,
    pub github_packages_token: Option<String>,
    pub github_packages_user: Option<String>,
    pub no_auto_update: bool,
    pub auto_update_secs: u64,
    pub api_auto_update_secs: u64,
    pub no_install_cleanup: bool,
    pub no_install_upgrade: bool,
    pub no_installed_dependents_check: bool,
    pub no_emoji: bool,
    pub install_badge: String,
    pub no_env_hints: bool,
    pub verbose: bool,
    pub debug: bool,
    pub download_concurrency: usize,
    pub cleanup_max_age_days: u64,
    pub curl_retries: u32,
    pub cask_opts: Vec<String>,
    /// `FASTBREW_BREW`: explicit path to the Ruby `brew` for delegation.
    pub brew_path: Option<PathBuf>,
    pub no_delegate: bool,
    pub require_sandbox: bool,
}

impl Config {
    /// Build the configuration from the process environment.
    ///
    /// Prefix resolution order: `HOMEBREW_PREFIX`, else the platform default.
    /// `HOMEBREW_REPOSITORY` defaults to the prefix on Apple Silicon and Linux
    /// and to `<prefix>/Homebrew` on Intel macOS when that directory exists.
    /// `HOMEBREW_CELLAR` defaults to `<repository>/Cellar` if it exists, else
    /// `<prefix>/Cellar`. Cache/logs/temp defaults per `utils/os.sh`.
    pub fn from_env() -> Result<Self> {
        let is_linux = cfg!(target_os = "linux");
        let default_prefix = if is_linux {
            DEFAULT_PREFIX_LINUX
        } else if cfg!(target_arch = "aarch64") {
            DEFAULT_PREFIX_MACOS_ARM64
        } else {
            DEFAULT_PREFIX_MACOS_X86_64
        };
        let prefix = env_path("HOMEBREW_PREFIX").unwrap_or_else(|| PathBuf::from(default_prefix));
        let repository = env_path("HOMEBREW_REPOSITORY").unwrap_or_else(|| {
            let nested = prefix.join("Homebrew");
            if !cfg!(target_arch = "aarch64") && !is_linux && nested.join("bin/brew").exists() {
                nested
            } else {
                prefix.clone()
            }
        });
        let library = env_path("HOMEBREW_LIBRARY").unwrap_or_else(|| repository.join("Library"));
        let cellar = env_path("HOMEBREW_CELLAR").unwrap_or_else(|| {
            let in_repo = repository.join("Cellar");
            if in_repo.is_dir() {
                in_repo
            } else {
                prefix.join("Cellar")
            }
        });
        let home = env_path("HOME")
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")));
        let (default_cache, default_logs, default_temp) = if is_linux {
            let cache_home = env_path("XDG_CACHE_HOME").unwrap_or_else(|| home.join(".cache"));
            (
                cache_home.join("Homebrew"),
                cache_home.join("Homebrew/Logs"),
                PathBuf::from("/tmp"),
            )
        } else {
            (
                home.join("Library/Caches/Homebrew"),
                home.join("Library/Logs/Homebrew"),
                PathBuf::from("/private/tmp"),
            )
        };
        let cache = env_path("HOMEBREW_CACHE").unwrap_or(default_cache);
        let logs = env_path("HOMEBREW_LOGS").unwrap_or(default_logs);
        let temp = env_path("HOMEBREW_TEMP").unwrap_or(default_temp);

        let cfg = Config {
            prefix,
            cellar,
            repository,
            library,
            cache,
            logs,
            temp,
            home,
            api_domain: env_string("HOMEBREW_API_DOMAIN")
                .unwrap_or_else(|| DEFAULT_API_DOMAIN.to_string()),
            bottle_domain: env_string("HOMEBREW_BOTTLE_DOMAIN")
                .unwrap_or_else(|| DEFAULT_BOTTLE_DOMAIN.to_string()),
            artifact_domain: env_string("HOMEBREW_ARTIFACT_DOMAIN"),
            github_packages_token: env_string("HOMEBREW_GITHUB_PACKAGES_TOKEN"),
            github_packages_user: env_string("HOMEBREW_GITHUB_PACKAGES_USER"),
            no_auto_update: env_flag("HOMEBREW_NO_AUTO_UPDATE"),
            auto_update_secs: env_u64("HOMEBREW_AUTO_UPDATE_SECS").unwrap_or(86_400),
            api_auto_update_secs: env_u64("HOMEBREW_API_AUTO_UPDATE_SECS").unwrap_or(450),
            no_install_cleanup: env_flag("HOMEBREW_NO_INSTALL_CLEANUP"),
            no_install_upgrade: env_flag("HOMEBREW_NO_INSTALL_UPGRADE"),
            no_installed_dependents_check: env_flag("HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK"),
            no_emoji: env_flag("HOMEBREW_NO_EMOJI"),
            install_badge: env_string("HOMEBREW_INSTALL_BADGE").unwrap_or_else(|| "🍺".to_string()),
            no_env_hints: env_flag("HOMEBREW_NO_ENV_HINTS"),
            verbose: env_flag("HOMEBREW_VERBOSE"),
            debug: env_flag("HOMEBREW_DEBUG"),
            download_concurrency: env_u64("HOMEBREW_DOWNLOAD_CONCURRENCY")
                .map(|n| n.max(1) as usize)
                .unwrap_or(8),
            cleanup_max_age_days: env_u64("HOMEBREW_CLEANUP_MAX_AGE_DAYS").unwrap_or(120),
            curl_retries: env_u64("HOMEBREW_CURL_RETRIES").unwrap_or(3) as u32,
            cask_opts: env_string("HOMEBREW_CASK_OPTS")
                .map(|s| s.split_whitespace().map(str::to_string).collect())
                .unwrap_or_default(),
            brew_path: env_path("FASTBREW_BREW"),
            no_delegate: env_flag("FASTBREW_NO_DELEGATE"),
            require_sandbox: env_flag("FASTBREW_REQUIRE_SANDBOX"),
        };
        cfg.check_sandbox()?;
        Ok(cfg)
    }

    /// Refuse to operate on a real Homebrew prefix when tests demand a sandbox.
    fn check_sandbox(&self) -> Result<()> {
        if !self.require_sandbox {
            return Ok(());
        }
        let standard = [
            DEFAULT_PREFIX_MACOS_ARM64,
            DEFAULT_PREFIX_MACOS_X86_64,
            DEFAULT_PREFIX_LINUX,
        ];
        let bad = |p: &Path| standard.iter().any(|s| p.starts_with(s));
        let host_cache = dirs::home_dir().map(|h| h.join("Library/Caches/Homebrew"));
        if bad(&self.prefix)
            || bad(&self.cellar)
            || host_cache
                .as_deref()
                .is_some_and(|c| self.cache.starts_with(c))
        {
            return Err(Error::user(format!(
                "FASTBREW_REQUIRE_SANDBOX is set but the prefix ({}) or cache ({}) points at a real Homebrew location",
                self.prefix.display(),
                self.cache.display()
            )));
        }
        Ok(())
    }

    // Derived paths (names follow startup/config.rb).
    pub fn opt_dir(&self) -> PathBuf {
        self.prefix.join("opt")
    }
    pub fn caskroom(&self) -> PathBuf {
        self.prefix.join("Caskroom")
    }
    pub fn linked_kegs(&self) -> PathBuf {
        self.prefix.join("var/homebrew/linked")
    }
    pub fn pinned_kegs(&self) -> PathBuf {
        self.prefix.join("var/homebrew/pinned")
    }
    pub fn pinned_casks(&self) -> PathBuf {
        self.prefix.join("var/homebrew/pinned_casks")
    }
    pub fn locks_dir(&self) -> PathBuf {
        self.prefix.join("var/homebrew/locks")
    }
    pub fn taps_dir(&self) -> PathBuf {
        self.library.join("Taps")
    }
    pub fn cache_api(&self) -> PathBuf {
        self.cache.join("api")
    }
    pub fn cache_downloads(&self) -> PathBuf {
        self.cache.join("downloads")
    }
    /// fastbrew's private cache (fast index, tap metadata cache).
    pub fn cache_fastbrew(&self) -> PathBuf {
        self.cache.join("fastbrew")
    }
    pub fn rack(&self, name: &str) -> PathBuf {
        self.cellar.join(name)
    }
    pub fn opt_record(&self, name: &str) -> PathBuf {
        self.opt_dir().join(name)
    }
    pub fn linked_record(&self, name: &str) -> PathBuf {
        self.linked_kegs().join(name)
    }
    pub fn pinned_record(&self, name: &str) -> PathBuf {
        self.pinned_kegs().join(name)
    }

    /// True when the prefix is the platform default (bottles need no build-prefix relocation).
    pub fn is_default_prefix(&self) -> bool {
        let default = if cfg!(target_os = "linux") {
            DEFAULT_PREFIX_LINUX
        } else if cfg!(target_arch = "aarch64") {
            DEFAULT_PREFIX_MACOS_ARM64
        } else {
            DEFAULT_PREFIX_MACOS_X86_64
        };
        self.prefix == Path::new(default)
    }

    /// A configuration rooted at `root` for tests: `<root>/prefix`,
    /// `<root>/cache`, `<root>/home` and so on, with every environment-derived
    /// option at its default. Never reads the process environment, so unit
    /// tests stay independent of the host.
    #[doc(hidden)]
    pub fn for_test(root: &Path) -> Config {
        let prefix = root.join("prefix");
        Config {
            cellar: prefix.join("Cellar"),
            repository: prefix.clone(),
            library: prefix.join("Library"),
            cache: root.join("cache"),
            logs: root.join("logs"),
            temp: root.join("tmp"),
            home: root.join("home"),
            prefix,
            api_domain: DEFAULT_API_DOMAIN.to_string(),
            bottle_domain: DEFAULT_BOTTLE_DOMAIN.to_string(),
            artifact_domain: None,
            github_packages_token: None,
            github_packages_user: None,
            no_auto_update: true,
            auto_update_secs: 86_400,
            api_auto_update_secs: 450,
            no_install_cleanup: true,
            no_install_upgrade: false,
            no_installed_dependents_check: false,
            no_emoji: false,
            install_badge: "🍺".to_string(),
            no_env_hints: true,
            verbose: false,
            debug: false,
            download_concurrency: 8,
            cleanup_max_age_days: 120,
            curl_retries: 3,
            cask_opts: vec![],
            brew_path: None,
            no_delegate: true,
            require_sandbox: false,
        }
    }

    /// Replace Homebrew's API placeholders in a string.
    pub fn expand_placeholders(&self, s: &str) -> String {
        s.replace("$HOMEBREW_PREFIX", &self.prefix.to_string_lossy())
            .replace("$HOMEBREW_CELLAR", &self.cellar.to_string_lossy())
            .replace("/$HOME", &self.home.to_string_lossy())
            .replace("$HOME", &self.home.to_string_lossy())
    }
}

fn env_string(key: &str) -> Option<String> {
    env::var(key).ok().filter(|v| !v.is_empty())
}

fn env_path(key: &str) -> Option<PathBuf> {
    env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn env_flag(key: &str) -> bool {
    env::var_os(key).is_some_and(|v| !v.is_empty())
}

fn env_u64(key: &str) -> Option<u64> {
    env_string(key).and_then(|v| v.parse().ok())
}
