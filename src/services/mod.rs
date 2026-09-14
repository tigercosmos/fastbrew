//! `services` command and launchd plist generation (`docs/COMPAT.md` 7).
//!
//! Port of `Library/Homebrew/services/{cli,formula_wrapper,formulae,system}.rb`
//! and `services/subcommand/*.rb`.
//!
//! Service definitions are read from the keg on disk (`Formula#launchd_service_path`),
//! not from the packages API, so `services list` never touches the index.

pub mod launchd;
pub mod plist;

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::keg::{self, Keg};
use crate::output;

/// `brew services`, used verbatim in Homebrew's messages (`Cli.bin`).
pub const BIN: &str = "brew services";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceStatus {
    None,
    Started,
    Scheduled,
    Stopped,
    Error(i32),
    Unknown,
    Other,
}

impl ServiceStatus {
    /// The symbol Homebrew prints and puts into `--json` output.
    pub fn as_str(&self) -> &'static str {
        match self {
            ServiceStatus::None => "none",
            ServiceStatus::Started => "started",
            ServiceStatus::Scheduled => "scheduled",
            ServiceStatus::Stopped => "stopped",
            ServiceStatus::Error(_) => "error",
            ServiceStatus::Unknown => "unknown",
            ServiceStatus::Other => "other",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ServiceInfo {
    pub name: String,
    pub label: String,
    pub status: ServiceStatus,
    pub user: Option<String>,
    /// Plist path in use (`~/Library/LaunchAgents/...` when loaded, else the keg plist).
    pub file: Option<std::path::PathBuf>,
    pub loaded: bool,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    /// `pid?`: the service has a live process.
    pub running: bool,
    /// `timed?`: the plist schedules the job (`StartInterval`/`StartCalendarInterval`).
    pub schedulable: bool,
    /// A service file exists in `~/Library/LaunchAgents` or `/Library/LaunchDaemons`.
    pub registered: bool,
    /// `path = ...` from `launchctl print`.
    pub loaded_file: Option<String>,
    /// The plist in the keg that fastbrew would install.
    pub source_file: std::path::PathBuf,
    pub command: Option<String>,
    pub working_dir: Option<String>,
    pub root_dir: Option<String>,
    pub log_path: Option<String>,
    pub error_log_path: Option<String>,
    pub interval: Option<i64>,
    pub cron: Option<String>,
}

// ---------------------------------------------------------------------------
// keg discovery
// ---------------------------------------------------------------------------

/// A service file shipped inside an installed keg.
#[derive(Debug, Clone)]
pub struct KegService {
    pub name: String,
    pub keg: Keg,
    pub plist_path: PathBuf,
    pub label: String,
    /// Labels Homebrew probes, newest naming first.
    pub labels: Vec<String>,
    pub timed: bool,
    pub keep_alive: bool,
    pub require_root: bool,
    pub program_arguments: Vec<String>,
    pub working_dir: Option<String>,
    pub root_dir: Option<String>,
    pub log_path: Option<String>,
    pub error_log_path: Option<String>,
    pub input_path: Option<String>,
    pub interval: Option<i64>,
    pub cron: Option<String>,
    pub user_name: Option<String>,
}

fn plist_candidates(name: &str) -> [String; 3] {
    [
        format!("{}{name}.plist", plist::PLIST_PREFIX),
        format!("{}{name}.plist", plist::CANONICAL_PREFIX),
        format!("{}{name}.plist", plist::UNIT_PREFIX),
    ]
}

/// The keg fastbrew reads the service from: the linked one, else the newest.
fn service_keg(cfg: &Config, name: &str) -> Option<Keg> {
    keg::linked_keg(cfg, name)
        .filter(|k| k.name == name)
        .or_else(|| keg::installed_kegs(cfg, name).pop())
}

/// Locate the plist a keg ships, mirroring `Formula#launchd_service_paths` plus
/// the `source_dir.glob("*.plist")` fallback in `FormulaWrapper`.
fn keg_plist_path(keg_path: &Path, name: &str) -> Option<PathBuf> {
    for candidate in plist_candidates(name) {
        let p = keg_path.join(&candidate);
        if p.is_file() {
            return Some(p);
        }
    }
    let mut extras: Vec<PathBuf> = std::fs::read_dir(keg_path)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "plist") && p.is_file())
        .collect();
    extras.sort();
    extras.into_iter().next()
}

/// Read the interesting keys out of an installed plist.
pub fn read_keg_service(cfg: &Config, name: &str) -> Option<KegService> {
    let keg = service_keg(cfg, name)?;
    let plist_path = keg_plist_path(&keg.path, name)?;
    let dict = launchd::read_plist_dictionary(&plist_path);
    let label = dict
        .as_ref()
        .and_then(|d| d.get("Label").and_then(|v| v.as_string()))
        .map(str::to_string)
        .unwrap_or_else(|| {
            plist_path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| format!("{}{name}", plist::PLIST_PREFIX))
        });

    let get_str = |key: &str| -> Option<String> {
        dict.as_ref()?
            .get(key)
            .and_then(|v| v.as_string())
            .map(str::to_string)
    };
    let get_i64 =
        |key: &str| -> Option<i64> { dict.as_ref()?.get(key).and_then(|v| v.as_signed_integer()) };
    let program_arguments = dict
        .as_ref()
        .and_then(|d| d.get("ProgramArguments"))
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_string().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let cron = dict
        .as_ref()
        .and_then(|d| d.get("StartCalendarInterval"))
        .and_then(|v| v.as_dictionary())
        .map(|d| {
            let f = |k: &str| {
                d.get(k)
                    .and_then(|v| v.as_signed_integer())
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "*".to_string())
            };
            format!(
                "{} {} {} {} {}",
                f("Minute"),
                f("Hour"),
                f("Day"),
                f("Month"),
                f("Weekday")
            )
        });
    let interval = get_i64("StartInterval");
    let keep_alive = dict
        .as_ref()
        .and_then(|d| d.get("KeepAlive"))
        .map(|v| match v {
            ::plist::Value::Boolean(b) => *b,
            _ => true,
        })
        .unwrap_or(false);

    let mut labels = vec![label.clone()];
    for candidate in [
        format!("{}{name}", plist::CANONICAL_PREFIX),
        format!("{}{name}", plist::PLIST_PREFIX),
    ] {
        if !labels.contains(&candidate) {
            labels.push(candidate);
        }
    }

    Some(KegService {
        name: name.to_string(),
        keg,
        plist_path,
        label,
        labels,
        timed: interval.is_some() || cron.is_some(),
        keep_alive,
        // A root service is one whose plist lives (or belongs) in LaunchDaemons;
        // the API's `require_root` is not recorded in the plist, so infer nothing.
        require_root: false,
        program_arguments,
        working_dir: get_str("WorkingDirectory"),
        root_dir: get_str("RootDirectory"),
        log_path: get_str("StandardOutPath"),
        error_log_path: get_str("StandardErrorPath"),
        input_path: get_str("StandardInPath"),
        interval,
        cron,
        user_name: get_str("UserName"),
    })
}

/// Every installed formula that ships a service file, sorted by name.
pub fn installed_services(cfg: &Config) -> Vec<KegService> {
    let mut out: Vec<KegService> = keg::installed_formula_names(cfg)
        .iter()
        .filter_map(|name| read_keg_service(cfg, name))
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

impl KegService {
    /// `FormulaWrapper#dest`: where `start` installs the plist.
    pub fn dest(&self, cfg: &Config, sudo: bool) -> PathBuf {
        launchd::dest_dir(cfg, sudo).join(format!("{}.plist", self.label))
    }

    /// The installed service file in use, whichever directory it lives in.
    fn registered_destination(&self, cfg: &Config) -> Option<PathBuf> {
        for dir in [launchd::user_path(cfg), launchd::boot_path()] {
            for label in &self.labels {
                let p = dir.join(format!("{label}.plist"));
                if p.exists() {
                    return Some(p);
                }
            }
        }
        None
    }

    /// `FormulaWrapper#owner`.
    fn owner(&self, cfg: &Config) -> Option<String> {
        let registered = self.registered_destination(cfg)?;
        if let Some(user) = launchd::read_plist_string(&registered, "UserName") {
            return Some(user);
        }
        let dir = registered.parent()?;
        if dir == launchd::boot_path() {
            return Some("root".to_string());
        }
        if dir == launchd::user_path(cfg) {
            return launchd::current_user();
        }
        None
    }

    /// The installed service file lives in `/Library/LaunchDaemons`.
    fn is_root_service(&self, cfg: &Config) -> bool {
        self.registered_destination(cfg)
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .is_some_and(|dir| dir == launchd::boot_path())
    }

    fn path_dirs(&self, cfg: &Config) -> Vec<PathBuf> {
        let def = plist::ServiceDef {
            working_dir: self.working_dir.clone(),
            root_dir: self.root_dir.clone(),
            input_path: self.input_path.clone(),
            log_path: self.log_path.clone(),
            error_log_path: self.error_log_path.clone(),
            ..Default::default()
        };
        def.path_dirs(cfg)
    }

    /// Build the `ServiceInfo` Homebrew's `to_hash` produces, probing launchd
    /// for every candidate label.
    pub fn info(&self, cfg: &Config) -> ServiceInfo {
        self.info_with(cfg, None)
    }

    /// `info`, but skipping the per-label `launchctl print` probes for services
    /// that a single `launchctl list` already showed are not loaded.
    ///
    /// `loaded` is the label set from `launchd::loaded_labels`. Root services
    /// are not in a user's `launchctl list`, so a plist in `/Library/LaunchDaemons`
    /// is still probed the slow way.
    pub fn info_with(
        &self,
        cfg: &Config,
        loaded: Option<&std::collections::HashSet<String>>,
    ) -> ServiceInfo {
        let probe: Vec<&String> = match loaded {
            None => self.labels.iter().collect(),
            Some(known) => {
                let hits: Vec<&String> =
                    self.labels.iter().filter(|l| known.contains(*l)).collect();
                if hits.is_empty() && self.is_root_service(cfg) {
                    self.labels.iter().collect()
                } else {
                    hits
                }
            }
        };
        let status = probe
            .into_iter()
            .find_map(|label| launchd::find_service(label, false).filter(|s| s.success));
        let parsed = status.as_ref().map(|s| s.info());
        let pid = parsed.as_ref().and_then(|p| p.pid).filter(|p| *p > 0);
        let running = pid.is_some();
        let loaded = status.as_ref().is_some_and(|s| s.success);
        let exit_code = parsed.as_ref().and_then(|p| p.last_exit_code);
        let output_blank = status.as_ref().is_none_or(|s| s.output.trim().is_empty());

        let state = if running {
            ServiceStatus::Started
        } else if !loaded {
            ServiceStatus::None
        } else if exit_code == Some(0) {
            if self.timed {
                ServiceStatus::Scheduled
            } else {
                ServiceStatus::Stopped
            }
        } else if let Some(code) = exit_code.filter(|c| *c != 0) {
            ServiceStatus::Error(code)
        } else if output_blank {
            ServiceStatus::Unknown
        } else {
            ServiceStatus::Other
        };

        let registered = self.registered_destination(cfg);
        let file = registered
            .clone()
            .unwrap_or_else(|| self.plist_path.clone());

        ServiceInfo {
            name: self.name.clone(),
            label: status
                .as_ref()
                .map(|s| s.label.clone())
                .unwrap_or_else(|| self.label.clone()),
            status: state,
            user: self.owner(cfg),
            file: Some(file),
            loaded,
            pid,
            exit_code,
            running,
            schedulable: self.timed,
            registered: registered.is_some(),
            loaded_file: parsed.and_then(|p| p.path),
            source_file: self.plist_path.clone(),
            command: (!self.program_arguments.is_empty())
                .then(|| shell_join(&self.program_arguments)),
            working_dir: self.working_dir.clone(),
            root_dir: self.root_dir.clone(),
            log_path: self.log_path.clone(),
            error_log_path: self.error_log_path.clone(),
            interval: self.interval,
            cron: self.cron.clone(),
        }
    }
}

fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if !a.is_empty()
                && a.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-.,:/@+=".contains(&b))
            {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

/// All installed formulae that ship a service, with their current status.
///
/// One `launchctl list` tells us which labels are loaded at all, so the
/// per-label `launchctl print` probes only run for services that are.
pub fn list(cfg: &Config) -> Result<Vec<ServiceInfo>> {
    let services = installed_services(cfg);
    if services.is_empty() {
        return Ok(vec![]);
    }
    let loaded = launchd::loaded_labels();
    Ok(services
        .iter()
        .map(|s| s.info_with(cfg, Some(&loaded)))
        .collect())
}

fn find(cfg: &Config, name: &str) -> Result<KegService> {
    if let Some(svc) = read_keg_service(cfg, name) {
        return Ok(svc);
    }
    if keg::installed_kegs(cfg, name).is_empty() {
        return Err(Error::user(format!("Formula `{name}` is not installed.")));
    }
    Err(Error::user(format!(
        "Formula `{name}` has not implemented #plist, #service or provided a locatable service file."
    )))
}

/// `Cli.report_service_running_or_loaded?`: true when there is nothing to do.
fn report_running_or_loaded(cfg: &Config, svc: &KegService, running_status: &str) -> bool {
    let info = svc.info(cfg);
    let loaded_name = if info.running {
        Some(info.label.clone())
    } else if info.loaded {
        Some(info.label.clone()).filter(|l| *l != svc.label)
    } else {
        None
    };

    if let Some(loaded) = loaded_name.filter(|l| *l != svc.label) {
        let status = if info.running {
            running_status
        } else {
            "loaded"
        };
        println!(
            "Service `{}` already {status} (label: {loaded}), use `{BIN} restart {}` to restart.",
            svc.name, svc.name
        );
        return true;
    }
    if info.running {
        println!(
            "Service `{}` already {running_status}, use `{BIN} restart {}` to restart.",
            svc.name, svc.name
        );
        return true;
    }
    false
}

fn copy_service_file(src: &Path, dest: &Path, sudo: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if sudo && !launchd::is_root() {
        let dir = dest.parent().unwrap_or(Path::new("/"));
        run_sudo(&["/bin/mkdir", "-p", &dir.to_string_lossy()])?;
        run_sudo(&["/bin/cp", &src.to_string_lossy(), &dest.to_string_lossy()])?;
        run_sudo(&["/bin/chmod", "644", &dest.to_string_lossy()])?;
        return Ok(());
    }
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let data = std::fs::read(src)?;
    keg::atomic_write(dest, &data)?;
    std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o644))?;
    Ok(())
}

fn run_sudo(argv: &[&str]) -> Result<()> {
    let status = std::process::Command::new("/usr/bin/sudo")
        .arg("--")
        .args(argv)
        .status()
        .map_err(|e| Error::user(format!("Failed to run sudo: {e}")))?;
    if status.success() {
        return Ok(());
    }
    Err(Error::user(format!(
        "Failure while executing; `sudo {}` exited with {}.",
        argv.join(" "),
        status.code().unwrap_or(-1)
    )))
}

fn remove_service_files(cfg: &Config, svc: &KegService, sudo: bool) {
    for dir in [launchd::user_path(cfg), launchd::boot_path()] {
        for label in &svc.labels {
            let file = dir.join(format!("{label}.plist"));
            if !file.exists() {
                continue;
            }
            if std::fs::remove_file(&file).is_err() && sudo && !launchd::is_root() {
                let _ = run_sudo(&["/bin/rm", "-f", &file.to_string_lossy()]);
            }
        }
    }
}

pub fn start(cfg: &Config, name: &str, sudo: bool) -> Result<()> {
    let svc = find(cfg, name)?;
    if report_running_or_loaded(cfg, &svc, "started") {
        return Ok(());
    }

    for dir in svc.path_dirs(cfg) {
        let _ = std::fs::create_dir_all(dir);
    }

    // `install_service_file`: replace any stale copies, then install ours 0644.
    remove_service_files(cfg, &svc, sudo);
    let dest = svc.dest(cfg, sudo);
    copy_service_file(&svc.plist_path, &dest, sudo)?;

    let domain = launchd::domain_target(sudo || launchd::is_root());
    launchd::enable(&domain, &svc.label, sudo)?;
    launchd::bootstrap(&domain, &dest, sudo)?;
    output::ohai(&format!(
        "Successfully started `{}` (label: {})",
        svc.name, svc.label
    ));
    Ok(())
}

/// `run`: bootstrap without enabling at login.
pub fn run(cfg: &Config, name: &str, sudo: bool) -> Result<()> {
    let svc = find(cfg, name)?;
    if report_running_or_loaded(cfg, &svc, "running") {
        return Ok(());
    }
    if launchd::is_root() {
        println!(
            "Service `{}` cannot be run (but can be started) as root.",
            svc.name
        );
        return Ok(());
    }
    for dir in svc.path_dirs(cfg) {
        let _ = std::fs::create_dir_all(dir);
    }
    let domain = launchd::domain_target(sudo);
    launchd::bootstrap(&domain, &svc.plist_path, sudo)?;
    output::ohai(&format!(
        "Successfully ran `{}` (label: {})",
        svc.name, svc.label
    ));
    Ok(())
}

pub fn stop(cfg: &Config, name: &str, sudo: bool) -> Result<()> {
    let svc = find(cfg, name)?;
    let info = svc.info(cfg);

    if !info.loaded && !info.running {
        remove_service_files(cfg, &svc, sudo);
        if info.registered {
            let owner = info.user.unwrap_or_else(|| "root".to_string());
            return Err(Error::user(format!(
                "Service `{}` is started as `{owner}`. Try:\n  {}{BIN} stop {}\n",
                svc.name,
                if launchd::is_root() { "" } else { "sudo " },
                svc.name
            )));
        }
        let domain = launchd::domain_target(sudo || launchd::is_root());
        if let Some(stopped) = svc
            .labels
            .iter()
            .find(|l| launchd::try_bootout(&domain, l, sudo))
        {
            output::ohai(&format!(
                "Successfully stopped `{}` (label: {stopped})",
                svc.name
            ));
        } else {
            output::opoo(&format!("Service `{}` is not started.", svc.name));
        }
        return Ok(());
    }

    println!("Stopping `{}`... (might take a while)", svc.name);
    let labels: Vec<String> = if info.loaded {
        vec![info.label.clone()]
    } else {
        svc.labels.clone()
    };
    for label in &labels {
        for domain in launchd::candidate_domain_targets(sudo || launchd::is_root()) {
            if !launchd::service_loaded(label, sudo) {
                break;
            }
            launchd::try_bootout(&domain, label, sudo);
            if launchd::service_loaded(label, sudo) {
                launchd::stop(label, sudo);
            }
        }
    }

    let still_loaded = labels.iter().any(|l| launchd::service_loaded(l, sudo));
    if !still_loaded {
        remove_service_files(cfg, &svc, sudo);
        output::ohai(&format!(
            "Successfully stopped `{}` (label: {})",
            svc.name,
            labels.join(", ")
        ));
    } else {
        output::opoo(&format!(
            "Unable to stop `{}` (label: {})",
            svc.name,
            labels.join(", ")
        ));
    }
    Ok(())
}

pub fn restart(cfg: &Config, name: &str, sudo: bool) -> Result<()> {
    let svc = find(cfg, name)?;
    let info = svc.info(cfg);
    // `restart.rb`: a loaded-but-unregistered service was `run`, so re-`run` it.
    let rerun = info.loaded && !info.registered;
    if info.loaded {
        stop(cfg, name, sudo)?;
    }
    if rerun {
        run(cfg, name, sudo)
    } else {
        start(cfg, name, sudo)
    }
}

pub fn kill(cfg: &Config, name: &str, sudo: bool) -> Result<()> {
    let svc = find(cfg, name)?;
    let info = svc.info(cfg);
    if !info.running {
        println!("Service `{}` is not started.", svc.name);
        return Ok(());
    }
    if svc.keep_alive {
        println!(
            "Service `{}` is set to automatically restart and can't be killed.",
            svc.name
        );
        return Ok(());
    }
    println!("Killing `{}`... (might take a while)", svc.name);
    let domain = launchd::domain_target(sudo || launchd::is_root());
    if launchd::kill(&domain, &info.label, "SIGTERM", sudo).is_err() {
        launchd::stop(&info.label, sudo);
    }
    let after = svc.info(cfg);
    if after.running {
        output::opoo(&format!(
            "Unable to kill `{}` (label: {})",
            svc.name, info.label
        ));
    } else {
        output::ohai(&format!(
            "Successfully killed `{}` (label: {})",
            svc.name, info.label
        ));
    }
    Ok(())
}

pub fn info(cfg: &Config, name: &str) -> Result<ServiceInfo> {
    Ok(find(cfg, name)?.info(cfg))
}

/// Remove service files for uninstalled formulae.
pub fn cleanup(cfg: &Config) -> Result<()> {
    let mut cleaned = 0usize;
    let known: Vec<String> = installed_services(cfg)
        .iter()
        .flat_map(|s| s.labels.clone())
        .collect();

    for dir in [launchd::dest_dir(cfg, false)] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                let Some(base) = p.file_name().and_then(|s| s.to_str()) else {
                    return false;
                };
                (base.starts_with(plist::UNIT_PREFIX) || base.starts_with(plist::CANONICAL_PREFIX))
                    && (base.ends_with(".plist")
                        || base.ends_with(".service")
                        || base.ends_with(".timer"))
            })
            .collect();
        files.sort();
        for file in files {
            let label = launchd::read_plist_label(&file).unwrap_or_else(|| {
                file.file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default()
            });
            if known.contains(&label) {
                continue;
            }
            if launchd::service_loaded(&label, false) {
                output::opoo(&format!(
                    "Service {label} not managed by `{BIN}` => skipping"
                ));
                continue;
            }
            println!("Removing unused service file: {}", file.display());
            std::fs::remove_file(&file)?;
            cleaned += 1;
        }
    }

    if cleaned == 0 {
        let kind = if launchd::is_root() {
            "root"
        } else {
            "user-space"
        };
        println!("All {kind} services OK, nothing cleaned...");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// output formats (COMPAT 7)
// ---------------------------------------------------------------------------

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const DEFAULT: &str = "\x1b[39m";

fn tty(code: &str) -> &str {
    if output::color_enabled() { code } else { "" }
}

/// `ListSubcommand.get_status_string`: the status cell, colours included.
fn status_cell(status: &ServiceStatus) -> String {
    let (color, text) = match status {
        ServiceStatus::Started => (GREEN, "started"),
        ServiceStatus::Scheduled => (GREEN, "scheduled"),
        ServiceStatus::Stopped => (DEFAULT, "stopped"),
        ServiceStatus::None => (DEFAULT, "none"),
        // Homebrew pads `error` with two spaces inside the colour escape.
        ServiceStatus::Error(_) => (RED, "error  "),
        ServiceStatus::Unknown => (YELLOW, "unknown"),
        ServiceStatus::Other => (YELLOW, "other"),
    };
    format!("{}{text}{}", tty(color), tty(RESET))
}

/// Ruby's `%-<width>.<width>s`: pad on the right, truncate to `width` characters.
fn pad(s: &str, width: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() >= width {
        return chars[..width].iter().collect();
    }
    let mut out: String = s.to_string();
    out.extend(std::iter::repeat_n(' ', width - chars.len()));
    out
}

/// `brew services list` as a table (`ListSubcommand.print_table`).
///
/// The status column is 15 wide because Homebrew sizes it for the nine bytes of
/// colour escapes; without colour the header and the rows are offset by nine
/// spaces, exactly as Homebrew prints it.
pub fn format_list_table(services: &[ServiceInfo]) -> String {
    if services.is_empty() {
        return String::new();
    }
    let rows: Vec<(String, String, Option<String>, Option<String>)> = services
        .iter()
        .map(|s| {
            let mut status = status_cell(&s.status);
            if let ServiceStatus::Error(code) = s.status {
                status.push_str(&code.to_string());
            }
            let file = if s.loaded {
                s.file
                    .as_ref()
                    .map(|f| tildify(&f.to_string_lossy()))
                    .filter(|f| !f.is_empty())
            } else {
                None
            };
            (s.name.clone(), status, s.user.clone(), file)
        })
        .collect();

    let longest_name = rows
        .iter()
        .map(|r| r.0.chars().count())
        .chain([4])
        .max()
        .unwrap();
    let longest_status = rows
        .iter()
        .map(|r| r.1.chars().count())
        .chain([15])
        .max()
        .unwrap();
    let longest_user = rows
        .iter()
        .filter_map(|r| r.2.as_ref().map(|u| u.chars().count()))
        .chain([4])
        .max()
        .unwrap();

    let mut out = String::new();
    out.push_str(&format!(
        "{}{} {} {} File{}\n",
        tty(BOLD),
        pad("Name", longest_name),
        pad("Status", longest_status - 9),
        pad("User", longest_user),
        tty(RESET)
    ));
    for (name, status, user, file) in rows {
        out.push_str(&format!(
            "{} {} {} {}\n",
            pad(&name, longest_name),
            pad(&status, longest_status),
            pad(user.as_deref().unwrap_or(""), longest_user),
            file.unwrap_or_default()
        ));
    }
    out
}

/// Replace the home directory with `~`, like `print_table` does.
fn tildify(path: &str) -> String {
    match dirs::home_dir() {
        Some(home) if !home.as_os_str().is_empty() => path.replace(&*home.to_string_lossy(), "~"),
        _ => path.to_string(),
    }
}

/// `brew services list --json` (`ListSubcommand.print_json`).
pub fn list_json(services: &[ServiceInfo]) -> String {
    let rows: Vec<serde_json::Value> = services
        .iter()
        .map(|s| {
            serde_json::json!({
                "name": s.name,
                "status": s.status.as_str(),
                "user": s.user,
                "file": s.file.as_ref().map(|f| f.to_string_lossy().into_owned()),
                "exit_code": s.exit_code,
            })
        })
        .collect();
    serde_json::to_string_pretty(&rows).unwrap_or_else(|_| "[]".to_string())
}

/// `brew services info` for one service (`InfoSubcommand.output`).
pub fn format_info(service: &ServiceInfo, verbose: bool) -> String {
    let b = |v: bool| -> String {
        if !output::stdout_is_tty() || std::env::var_os("HOMEBREW_NO_EMOJI").is_some() {
            return v.to_string();
        }
        if v {
            format!("{}{}✔{}", tty(BOLD), tty(GREEN), tty(RESET))
        } else {
            format!("{}{}✘{}", tty(BOLD), tty(RED), tty(RESET))
        }
    };
    let mut out = format!(
        "{}{}{} ({})\n",
        tty(BOLD),
        service.name,
        tty(RESET),
        service.label
    );
    out.push_str(&format!("Running: {}\n", b(service.running)));
    out.push_str(&format!("Loaded: {}\n", b(service.loaded)));
    out.push_str(&format!("Schedulable: {}\n", b(service.schedulable)));
    if let Some(pid) = service.pid {
        out.push_str(&format!(
            "User: {}\n",
            service.user.clone().unwrap_or_default()
        ));
        out.push_str(&format!("PID: {pid}\n"));
    }
    if !verbose {
        return out;
    }
    let file = service
        .file
        .as_ref()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();
    out.push_str(&format!("File: {file} {}\n", b(!file.is_empty())));
    out.push_str(&format!("Registered at login: {}\n", b(service.registered)));
    if let Some(c) = &service.command {
        out.push_str(&format!("Command: {c}\n"));
    }
    if let Some(c) = &service.working_dir {
        out.push_str(&format!("Working directory: {c}\n"));
    }
    if let Some(c) = &service.root_dir {
        out.push_str(&format!("Root directory: {c}\n"));
    }
    if let Some(c) = &service.log_path {
        out.push_str(&format!("Log: {c}\n"));
    }
    if let Some(c) = &service.error_log_path {
        out.push_str(&format!("Error log: {c}\n"));
    }
    if let Some(c) = service.interval {
        out.push_str(&format!("Interval: {c}s\n"));
    }
    if let Some(c) = &service.cron {
        out.push_str(&format!("Cron: {c}\n"));
    }
    out
}

/// `brew services info --json`: the whole `to_hash`.
pub fn info_json(services: &[ServiceInfo]) -> String {
    let rows: Vec<serde_json::Value> = services
        .iter()
        .map(|s| {
            serde_json::json!({
                "name": s.name,
                "service_name": s.label,
                "running": s.running,
                "loaded": s.loaded,
                "schedulable": s.schedulable,
                "pid": s.pid,
                "exit_code": s.exit_code,
                "user": s.user,
                "status": s.status.as_str(),
                "file": s.file.as_ref().map(|f| f.to_string_lossy().into_owned()),
                "registered": s.registered,
                "loaded_file": s.loaded_file,
                "command": s.command,
                "working_dir": s.working_dir,
                "root_dir": s.root_dir,
                "log_path": s.log_path,
                "error_log_path": s.error_log_path,
                "interval": s.interval,
                "cron": s.cron,
            })
        })
        .collect();
    serde_json::to_string_pretty(&rows).unwrap_or_else(|_| "[]".to_string())
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// A `Config` pointing at an arbitrary prefix, bypassing the sandbox guard
    /// (unit tests never touch the filesystem through it).
    pub fn config_with_prefix(prefix: &str, home: &str) -> Config {
        Config {
            prefix: PathBuf::from(prefix),
            cellar: PathBuf::from(format!("{prefix}/Cellar")),
            repository: PathBuf::from(prefix),
            library: PathBuf::from(format!("{prefix}/Library")),
            cache: PathBuf::from(format!("{home}/Library/Caches/Homebrew")),
            logs: PathBuf::from(format!("{home}/Library/Logs/Homebrew")),
            temp: PathBuf::from("/private/tmp"),
            home: PathBuf::from(home),
            api_domain: crate::config::DEFAULT_API_DOMAIN.to_string(),
            bottle_domain: crate::config::DEFAULT_BOTTLE_DOMAIN.to_string(),
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
            no_delegate: false,
            require_sandbox: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(name: &str, status: ServiceStatus, user: Option<&str>, file: &str) -> ServiceInfo {
        ServiceInfo {
            name: name.to_string(),
            label: format!("homebrew.mxcl.{name}"),
            exit_code: match status {
                ServiceStatus::Error(c) => Some(c),
                _ => None,
            },
            loaded: !matches!(status, ServiceStatus::None),
            running: matches!(status, ServiceStatus::Started),
            status,
            user: user.map(str::to_string),
            file: Some(PathBuf::from(file)),
            pid: None,
            schedulable: false,
            registered: true,
            loaded_file: None,
            source_file: PathBuf::from(file),
            command: None,
            working_dir: None,
            root_dir: None,
            log_path: None,
            error_log_path: None,
            interval: None,
            cron: None,
        }
    }

    #[test]
    fn list_table_layout() {
        let services = vec![
            info(
                "foo",
                ServiceStatus::None,
                None,
                "/opt/homebrew/Cellar/foo/1.0/homebrew.mxcl.foo.plist",
            ),
            info(
                "postgresql@17",
                ServiceStatus::Started,
                Some("alice"),
                "/tmp/sandbox/home/Library/LaunchAgents/homebrew.mxcl.postgresql@17.plist",
            ),
            info("znc", ServiceStatus::Error(78), Some("root"), "/x.plist"),
        ];
        let table = format_list_table(&services);
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 4);
        // `Name` is padded to the longest name, `Status` to 15 - 9 = 6.
        assert_eq!(lines[0], "Name          Status User  File");
        assert_eq!(lines[1], "foo           none                  ");
        assert!(
            lines[2].starts_with("postgresql@17 started         alice "),
            "{:?}",
            lines[2]
        );
        assert!(lines[2].ends_with("homebrew.mxcl.postgresql@17.plist"));
        assert_eq!(lines[3], "znc           error  78       root  /x.plist");
    }

    #[test]
    fn list_json_fields() {
        let services = vec![info(
            "foo",
            ServiceStatus::Error(3),
            Some("alice"),
            "/x.plist",
        )];
        let json: serde_json::Value = serde_json::from_str(&list_json(&services)).unwrap();
        assert_eq!(json[0]["name"], "foo");
        assert_eq!(json[0]["status"], "error");
        assert_eq!(json[0]["user"], "alice");
        assert_eq!(json[0]["file"], "/x.plist");
        assert_eq!(json[0]["exit_code"], 3);
        // Key order matches Homebrew's `JSON_FIELDS`.
        let text = list_json(&services);
        let order: Vec<usize> = ["name", "status", "user", "file", "exit_code"]
            .iter()
            .map(|k| text.find(&format!("\"{k}\"")).unwrap())
            .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{text}");
    }

    #[test]
    fn empty_table_is_empty() {
        assert_eq!(format_list_table(&[]), "");
        assert_eq!(list_json(&[]), "[]");
    }
}
