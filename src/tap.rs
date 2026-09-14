//! Third-party taps under `$LIBRARY/Taps/<user>/homebrew-<repo>`.
//!
//! - `Tap::parse("user/repo")`, `installed_taps(cfg)`, `path()`, `remote()`.
//! - `tap(cfg, name, url)`: `git clone --origin=origin --template= --config core.fsmonitor=false <url> <path>`;
//!   default URL `https://github.com/<user>/homebrew-<repo>`. Refuse
//!   `homebrew/core` and `homebrew/cask` without `--force` with Homebrew's
//!   message ("Tapping homebrew/core is no longer typically necessary...").
//! - `untap`: refuse when formulae from the tap are installed unless `--force`.
//! - `update_all`: `git fetch` + fast-forward each tap concurrently, returning
//!   which changed.
//! - `formula_files`, `cask_files`: `Formula/**/*.rb`, `Casks/**/*.rb`
//!   (also `HomebrewFormula/`), and `aliases` from the `Aliases/` symlinks.
//!
//! Port of `Library/Homebrew/tap.rb`, `tap_constants.rb`, `cmd/tap.rb` and
//! `cmd/untap.rb`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use rayon::prelude::*;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::keg;
use crate::model::FormulaReceipt;
use crate::output;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Tap {
    pub user: String,
    pub repo: String,
}

/// What `tap()` did, so callers can mirror `brew tap`'s exit status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TapOutcome {
    /// The tap was cloned.
    Tapped,
    /// It was already present; `brew tap` swallows `TapAlreadyTappedError`.
    AlreadyTapped,
}

impl Tap {
    /// `Tap.fetch`: accepts `user/repo`, `user/homebrew-repo` and remote URLs.
    pub fn parse(name: &str) -> Option<Tap> {
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        let reference = if looks_like_url(name) {
            remote_repository(name)?
        } else {
            name.to_string()
        };

        let (user, repo) = reference.split_once('/')?;
        if user.is_empty() || repo.is_empty() || repo.contains('/') {
            return None;
        }
        if [user, repo].iter().any(|p| *p == "." || *p == "..") {
            return None;
        }

        // `HOMEBREW_OFFICIAL_REPO_PREFIXES_REGEX`.
        let repo = repo
            .strip_prefix("homebrew-")
            .or_else(|| repo.strip_prefix("linuxbrew-"))
            .unwrap_or(repo);
        if repo.is_empty() {
            return None;
        }

        // Homebrew capitalises the official users so `brew tap homebrew/foo`
        // clones from `https://github.com/Homebrew/homebrew-foo`.
        let user = match user.to_lowercase().as_str() {
            "homebrew" => "Homebrew".to_string(),
            "linuxbrew" => "Linuxbrew".to_string(),
            _ => user.to_string(),
        };
        let repo = if user == "Homebrew" && repo.eq_ignore_ascii_case("homebrew") {
            "core".to_string()
        } else {
            repo.to_string()
        };

        Some(Tap { user, repo })
    }

    pub fn name(&self) -> String {
        format!("{}/{}", self.user.to_lowercase(), self.repo.to_lowercase())
    }

    /// `user/homebrew-repo`, as used in the remote URL.
    pub fn full_name(&self) -> String {
        format!("{}/homebrew-{}", self.user, self.repo)
    }

    pub fn path(&self, cfg: &Config) -> PathBuf {
        cfg.taps_dir()
            .join(self.user.to_lowercase())
            .join(format!("homebrew-{}", self.repo.to_lowercase()))
    }

    pub fn default_remote(&self) -> String {
        format!("https://github.com/{}/homebrew-{}", self.user, self.repo)
    }

    pub fn is_core(&self) -> bool {
        self.name() == "homebrew/core"
    }

    pub fn is_cask(&self) -> bool {
        self.name() == "homebrew/cask"
    }

    /// Whether Homebrew's own taps (which the API replaces) are meant.
    pub fn is_official(&self) -> bool {
        self.user == "Homebrew"
    }

    pub fn is_installed(&self, cfg: &Config) -> bool {
        self.path(cfg).is_dir()
    }

    /// Configured `origin` URL, falling back to the default remote.
    pub fn remote(&self, cfg: &Config) -> Option<String> {
        if !self.is_installed(cfg) {
            return Some(self.default_remote());
        }
        let out = git(&self.path(cfg), &["config", "--get", "remote.origin.url"]).ok()?;
        let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if url.is_empty() { None } else { Some(url) }
    }

    /// `<tap>/<name>` used in messages and receipts.
    pub fn full_package_name(&self, name: &str) -> String {
        format!("{}/{name}", self.name())
    }

    /// Directory holding the tap's formulae (`Formula`, `HomebrewFormula`, or the root).
    pub fn formula_dir(&self, cfg: &Config) -> PathBuf {
        let root = self.path(cfg);
        if self.is_official() {
            return root.join("Formula");
        }
        for candidate in ["Formula", "HomebrewFormula"] {
            let p = root.join(candidate);
            if p.is_dir() {
                return p;
            }
        }
        root
    }

    pub fn cask_dir(&self, cfg: &Config) -> PathBuf {
        self.path(cfg).join("Casks")
    }

    pub fn alias_dir(&self, cfg: &Config) -> PathBuf {
        self.path(cfg).join("Aliases")
    }
}

impl std::fmt::Display for Tap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name())
    }
}

fn looks_like_url(s: &str) -> bool {
    s.contains("://") || (s.contains(':') && !s.starts_with('/')) || s.starts_with('/')
}

/// `HOMEBREW_TAP_REPOSITORY_REGEX`: the `user/repo` part of a clone URL.
fn remote_repository(url: &str) -> Option<String> {
    let trimmed = url.trim_end_matches('/');
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    let trimmed = trimmed.trim_end_matches('/');
    let cut = trimmed.rfind(['/', ':'])?;
    let repo = &trimmed[cut + 1..];
    let head = &trimmed[..cut];
    let user_cut = head.rfind(['/', ':']).map(|i| i + 1).unwrap_or(0);
    let user = &head[user_cut..];
    if user.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("{user}/{repo}"))
}

/// Every tap cloned under `$LIBRARY/Taps`, sorted by name.
pub fn installed_taps(cfg: &Config) -> Vec<Tap> {
    let Ok(users) = std::fs::read_dir(cfg.taps_dir()) else {
        return vec![];
    };
    let mut taps: Vec<Tap> = vec![];
    for user in users.flatten() {
        if !user.path().is_dir() {
            continue;
        }
        let Ok(repos) = std::fs::read_dir(user.path()) else {
            continue;
        };
        for repo in repos.flatten() {
            if !repo.path().is_dir() {
                continue;
            }
            let user_name = user.file_name().to_string_lossy().into_owned();
            let repo_name = repo.file_name().to_string_lossy().into_owned();
            if user_name.starts_with('.') || repo_name.starts_with('.') {
                continue;
            }
            if let Some(t) = Tap::parse(&format!("{user_name}/{repo_name}")) {
                taps.push(t);
            }
        }
    }
    taps.sort_by_key(Tap::name);
    taps.dedup();
    taps
}

// ---------------------------------------------------------------------------
// git
// ---------------------------------------------------------------------------

/// Run `git` inside `dir` with hooks and credential prompts disabled.
fn git(dir: &Path, args: &[&str]) -> Result<Output> {
    git_in(Some(dir), args)
}

fn git_in(dir: Option<&Path>, args: &[&str]) -> Result<Output> {
    let mut cmd = Command::new("git");
    cmd.arg("-c").arg("core.hooksPath=/dev/null");
    if let Some(dir) = dir {
        cmd.arg("-C").arg(dir);
    }
    cmd.args(args);
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.env("GIT_ASKPASS", "");
    cmd.env("GIT_CONFIG_NOSYSTEM", "1");
    cmd.output()
        .map_err(|e| Error::user(format!("Failed to run git: {e}")))
}

fn git_checked(dir: Option<&Path>, args: &[&str]) -> Result<Output> {
    let out = git_in(dir, args)?;
    if out.status.success() {
        return Ok(out);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    Err(Error::user(format!(
        "Failure while executing; `git {}` exited with {}.\n{}",
        args.join(" "),
        out.status.code().unwrap_or(-1),
        stderr.trim()
    )))
}

fn head_sha(path: &Path) -> Option<String> {
    let out = git(path, &["rev-parse", "HEAD"]).ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if sha.is_empty() { None } else { Some(sha) }
}

/// `<sha>` of the tap's `HEAD`, written into receipts as `tap_git_head`.
pub fn tap_git_head(cfg: &Config, tap: &Tap) -> Option<String> {
    head_sha(&tap.path(cfg))
}

fn git_line(path: &Path, args: &[&str]) -> Option<String> {
    let out = git(path, args).ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!line.is_empty()).then_some(line)
}

/// `GitRepository#last_committed`: the relative date of `HEAD` (`9 days ago`).
pub fn git_last_commit(cfg: &Config, tap: &Tap) -> Option<String> {
    git_line(&tap.path(cfg), &["show", "-s", "--format=%cr", "HEAD"])
}

/// `GitRepository#branch_name`: the checked-out branch, `HEAD` when detached.
pub fn git_branch(cfg: &Config, tap: &Tap) -> Option<String> {
    let reference = git_line(
        &tap.path(cfg),
        &["rev-parse", "--symbolic-full-name", "HEAD"],
    )?;
    if reference == "HEAD" {
        return Some(reference);
    }
    reference
        .strip_prefix("refs/heads/")
        .map(str::to_string)
        .or(Some(reference))
}

// ---------------------------------------------------------------------------
// tap / untap
// ---------------------------------------------------------------------------

/// `Utils.pluralize("formula", n)` and friends, for the `Tapped ...` line.
fn pluralize(stem: &str, count: usize) -> String {
    let plural = match stem {
        "formula" => "e",
        _ => "s",
    };
    let suffix = if count == 1 { "" } else { plural };
    format!("{count} {stem}{suffix}")
}

/// `Utils::Text.to_sentence`.
fn to_sentence(values: &[String]) -> String {
    match values.len() {
        0 => String::new(),
        1 => values[0].clone(),
        2 => format!("{} and {}", values[0], values[1]),
        n => format!("{} and {}", values[..n - 1].join(", "), values[n - 1]),
    }
}

/// `Pathname#abv`: `N files, SIZE` (the count is omitted for a single file).
fn abv(path: &Path) -> String {
    let (files, bytes) = keg::disk_usage(path);
    let size = keg::disk_usage_readable(bytes);
    if files > 1 {
        format!("{}, {size}", number_readable(files))
    } else {
        size
    }
}

/// `Formatter.number_readable`: thousands separated by commas.
fn number_readable(n: u64) -> String {
    let digits: Vec<char> = n.to_string().chars().collect();
    let mut out = String::new();
    for (i, c) in digits.iter().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*c);
    }
    format!("{out} files")
}

/// `Tap#contents`: `1 command`, `2 casks`, `3 formulae` for what the tap has.
pub fn contents(cfg: &Config, tap: &Tap) -> Vec<String> {
    let mut out = vec![];
    let commands = command_files(cfg, tap).len();
    if commands > 0 {
        out.push(pluralize("command", commands));
    }
    let casks = cask_files(cfg, tap).len();
    if casks > 0 {
        out.push(pluralize("cask", casks));
    }
    let formulae = formula_files(cfg, tap).len();
    if formulae > 0 {
        out.push(pluralize("formula", formulae));
    }
    out
}

pub fn tap(cfg: &Config, name: &str, url: Option<&str>, force: bool, quiet: bool) -> Result<()> {
    match tap_with_outcome(cfg, name, url, force, quiet)? {
        // `cmd/tap.rb` rescues `TapAlreadyTappedError` and exits 0.
        TapOutcome::AlreadyTapped | TapOutcome::Tapped => Ok(()),
    }
}

/// The full `Tap#install` behaviour, including the already-tapped signal.
pub fn tap_with_outcome(
    cfg: &Config,
    name: &str,
    url: Option<&str>,
    force: bool,
    quiet: bool,
) -> Result<TapOutcome> {
    let tap = Tap::parse(name).ok_or_else(|| Error::user(format!("Invalid tap name: '{name}'")))?;
    let path = tap.path(cfg);

    if path.is_dir() {
        return Err(Error::user(format!("Tap {} already tapped.\n", tap.name())));
    }

    if (tap.is_core() || tap.is_cask()) && !force {
        return Err(Error::user(format!(
            "Tapping {} is no longer typically necessary.\n\
             Add {} if you are sure you need it for contributing to Homebrew.",
            tap.name(),
            output::underline("--force")
        )));
    }

    let remote = url
        .map(str::to_string)
        .unwrap_or_else(|| tap.default_remote());

    if !quiet {
        eprintln!(
            "{}",
            output::format_ohai(&format!("Tapping {}", tap.name()))
        );
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut args: Vec<String> = vec!["clone".into(), "--origin=origin".into()];
    if quiet {
        args.push("-q".into());
    }
    args.push("--template=".into());
    args.push("--config".into());
    args.push("core.fsmonitor=false".into());
    args.push("--end-of-options".into());
    args.push(remote);
    args.push(path.to_string_lossy().into_owned());
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();

    if let Err(e) = git_checked(None, &argv) {
        let _ = std::fs::remove_dir_all(&path);
        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir(parent);
        }
        return Err(e);
    }

    if !quiet {
        let listed = to_sentence(&contents(cfg, &tap));
        let prefix = if listed.is_empty() {
            String::new()
        } else {
            format!(" {listed}")
        };
        eprintln!("Tapped{prefix} ({}).", abv(&path));
    }
    Ok(TapOutcome::Tapped)
}

/// Packages from `tap` that are currently installed.
fn installed_from_tap(cfg: &Config, tap: &Tap) -> (Vec<String>, Vec<String>) {
    let tap_name = tap.name();
    let mut formulae: Vec<String> = vec![];
    for name in keg::installed_formula_names(cfg) {
        let from_tap = keg::installed_kegs(cfg, &name).iter().any(|k| {
            FormulaReceipt::read(&k.receipt_path())
                .ok()
                .and_then(|r| r.source.tap)
                .is_some_and(|t| t.eq_ignore_ascii_case(&tap_name))
        });
        if from_tap {
            formulae.push(format!("{tap_name}/{name}"));
        }
    }

    let mut casks: Vec<String> = cask_files(cfg, tap)
        .into_iter()
        .map(|(token, _)| token)
        .filter(|token| cfg.caskroom().join(token).is_dir())
        .map(|token| format!("{tap_name}/{token}"))
        .collect();
    casks.sort();
    (formulae, casks)
}

pub fn untap(cfg: &Config, name: &str, force: bool) -> Result<()> {
    let tap = Tap::parse(name).ok_or_else(|| Error::user(format!("Invalid tap name: '{name}'")))?;
    let path = tap.path(cfg);
    if !path.is_dir() {
        return Err(Error::user(format!("No available tap {}.\n", tap.name())));
    }

    if !force {
        let (formulae, casks) = installed_from_tap(cfg, &tap);
        if !formulae.is_empty() || !casks.is_empty() {
            let kind = if formulae.is_empty() {
                "casks"
            } else if casks.is_empty() {
                "formulae"
            } else {
                "formulae and casks"
            };
            let names = [formulae, casks].concat().join("\n");
            return Err(Error::user(format!(
                "Refusing to untap {} because it contains the following installed {kind}:\n{names}\n",
                tap.name()
            )));
        }
    }

    eprintln!("Untapping {}...", tap.name());
    let listed = to_sentence(&contents(cfg, &tap));
    let size = abv(&path);
    std::fs::remove_dir_all(&path)?;
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir(parent);
    }
    let prefix = if listed.is_empty() {
        String::new()
    } else {
        format!(" {listed}")
    };
    eprintln!("Untapped{prefix} ({size}).");
    Ok(())
}

/// Returns the taps whose HEAD moved.
pub fn update_all(cfg: &Config, quiet: bool) -> Result<Vec<Tap>> {
    let taps = installed_taps(cfg);
    let mut changed: Vec<Tap> = taps
        .par_iter()
        .filter_map(|tap| update_one(cfg, tap, quiet).ok().flatten())
        .collect();
    changed.sort_by_key(Tap::name);
    Ok(changed)
}

/// Fetch and fast-forward one tap; `Some(tap)` when its `HEAD` moved.
pub fn update_one(cfg: &Config, tap: &Tap, quiet: bool) -> Result<Option<Tap>> {
    let path = tap.path(cfg);
    if !path.join(".git").exists() {
        return Ok(None);
    }
    let before = head_sha(&path);

    let mut fetch: Vec<&str> = vec!["fetch"];
    if quiet {
        fetch.push("-q");
    }
    let fetched = git(&path, &fetch)?;
    if !fetched.status.success() {
        if !quiet {
            output::opoo(&format!(
                "Failed to update tap {}: {}",
                tap.name(),
                String::from_utf8_lossy(&fetched.stderr).trim()
            ));
        }
        return Ok(None);
    }

    // Fast-forward only: never rewrite a tap the user is working in.
    let merged = git(&path, &["merge", "--ff-only", "@{upstream}"])?;
    if !merged.status.success() {
        let pulled = git(&path, &["pull", "--ff-only"])?;
        if !pulled.status.success() && !quiet {
            output::opoo(&format!("Tap {} could not be fast-forwarded.", tap.name()));
        }
    }

    let after = head_sha(&path);
    Ok((before != after).then(|| tap.clone()))
}

// ---------------------------------------------------------------------------
// file enumeration
// ---------------------------------------------------------------------------

fn ruby_files(dir: &Path, recursive: bool) -> Vec<PathBuf> {
    if !dir.is_dir() {
        return vec![];
    }
    let walker = if recursive {
        walkdir::WalkDir::new(dir)
    } else {
        walkdir::WalkDir::new(dir).max_depth(1)
    };
    let mut out: Vec<PathBuf> = walker
        .into_iter()
        .filter_entry(|e| {
            !e.file_name()
                .to_str()
                .is_some_and(|n| n.starts_with('.') && e.depth() > 0)
        })
        .flatten()
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| p.extension().is_some_and(|e| e == "rb"))
        .collect();
    out.sort();
    out
}

/// Keep the longest path when two files share a basename (`formula_files_by_name`).
fn by_name(files: Vec<PathBuf>) -> Vec<(String, PathBuf)> {
    let mut map: BTreeMap<String, PathBuf> = BTreeMap::new();
    for file in files {
        let Some(stem) = file.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        match map.get(stem) {
            Some(existing) if existing.to_string_lossy().len() >= file.to_string_lossy().len() => {}
            _ => {
                map.insert(stem.to_string(), file);
            }
        }
    }
    map.into_iter().collect()
}

/// `(name, path)` for every formula in the tap, sorted by name.
///
/// `name` is the bare formula name; prefix it with `Tap::name()` for a full name.
pub fn formula_files(cfg: &Config, tap: &Tap) -> Vec<(String, PathBuf)> {
    let dir = tap.formula_dir(cfg);
    // Sharding is only supported under `Formula/` and `HomebrewFormula/`; at the
    // tap root only the top level counts, so commands and casks stay out.
    let recursive = dir != tap.path(cfg);
    by_name(ruby_files(&dir, recursive))
}

/// `(token, path)` for every cask in the tap, sorted by token.
pub fn cask_files(cfg: &Config, tap: &Tap) -> Vec<(String, PathBuf)> {
    by_name(ruby_files(&tap.cask_dir(cfg), true))
}

/// External commands shipped by the tap: every file in `cmd/`
/// (`Commands.find_commands`, which does not filter by name).
pub fn command_files(cfg: &Config, tap: &Tap) -> Vec<PathBuf> {
    let dir = tap.path(cfg).join("cmd");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return vec![];
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    out.sort();
    out
}

/// `alias -> formula name` from the `Aliases/` symlinks.
pub fn aliases(cfg: &Config, tap: &Tap) -> BTreeMap<String, String> {
    let dir = tap.alias_dir(cfg);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return BTreeMap::new();
    };
    let mut out = BTreeMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(alias) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if alias.starts_with('.') {
            continue;
        }
        // `Pathname#realpath` on the symlink, then the target's basename.
        let target = std::fs::canonicalize(&path).unwrap_or(path.clone());
        if !target.is_file() {
            continue;
        }
        if let Some(name) = target.file_stem().and_then(|s| s.to_str()) {
            out.insert(alias.to_string(), name.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tap_names() {
        let t = Tap::parse("user/repo").unwrap();
        assert_eq!((t.user.as_str(), t.repo.as_str()), ("user", "repo"));
        assert_eq!(t.name(), "user/repo");
        assert_eq!(t.full_name(), "user/homebrew-repo");
        assert_eq!(t.default_remote(), "https://github.com/user/homebrew-repo");

        // The `homebrew-` prefix is optional.
        assert_eq!(Tap::parse("user/homebrew-repo").unwrap(), t);
        assert_eq!(Tap::parse("user/linuxbrew-repo").unwrap(), t);

        // Official users are capitalised so the remote URL is right.
        let core = Tap::parse("homebrew/core").unwrap();
        assert_eq!(core.user, "Homebrew");
        assert_eq!(core.name(), "homebrew/core");
        assert!(core.is_core());
        assert!(core.is_official());
        assert!(Tap::parse("Homebrew/homebrew-core").unwrap().is_core());
        assert!(Tap::parse("homebrew/homebrew").unwrap().is_core());
        assert!(Tap::parse("homebrew/cask").unwrap().is_cask());

        // Mixed case is preserved for non-official users.
        assert_eq!(Tap::parse("OvEn-sh/bun").unwrap().user, "OvEn-sh");
        assert_eq!(Tap::parse("OvEn-sh/bun").unwrap().name(), "oven-sh/bun");
    }

    #[test]
    fn parses_tap_urls() {
        for url in [
            "https://github.com/user/homebrew-repo",
            "https://github.com/user/homebrew-repo.git",
            "https://github.com/user/homebrew-repo/",
            "git@github.com:user/homebrew-repo.git",
            "ssh://git@example.com/user/homebrew-repo.git",
            "file:///tmp/x/user/homebrew-repo",
        ] {
            let t = Tap::parse(url).unwrap_or_else(|| panic!("{url}"));
            assert_eq!(t.name(), "user/repo", "{url}");
        }
    }

    #[test]
    fn rejects_bad_names() {
        for bad in ["", "nope", "a/b/c", "/", "user/", "/repo", "../x"] {
            assert!(Tap::parse(bad).is_none(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn tap_paths() {
        let cfg = crate::services::test_support::config_with_prefix("/sb", "/sb/home");
        let t = Tap::parse("User/Repo").unwrap();
        assert_eq!(
            t.path(&cfg),
            PathBuf::from("/sb/Library/Taps/user/homebrew-repo")
        );
        assert_eq!(t.formula_dir(&cfg).file_name().unwrap(), "homebrew-repo");
        assert_eq!(
            t.cask_dir(&cfg),
            PathBuf::from("/sb/Library/Taps/user/homebrew-repo/Casks")
        );
    }

    #[test]
    fn formats_counts() {
        assert_eq!(pluralize("formula", 1), "1 formula");
        assert_eq!(pluralize("formula", 3), "3 formulae");
        assert_eq!(pluralize("cask", 1), "1 cask");
        assert_eq!(pluralize("cask", 2), "2 casks");
        assert_eq!(
            to_sentence(&["1 cask".into(), "2 formulae".into()]),
            "1 cask and 2 formulae"
        );
        assert_eq!(
            to_sentence(&["a".into(), "b".into(), "c".into()]),
            "a, b and c"
        );
        assert_eq!(number_readable(1234), "1,234 files");
        assert_eq!(number_readable(42), "42 files");
    }
}
