//! End-to-end bottle pours inside the sandbox.
//!
//! Run with `FASTBREW_TEST_NETWORK=1 scripts/sandbox.sh test bottle`. Without
//! `FASTBREW_TEST_NETWORK=1` the network tests skip; outside the sandbox
//! (`scripts/sandbox.sh`, which exports `FASTBREW_REQUIRE_SANDBOX=1`) every
//! test skips, so `cargo test` never touches the host Homebrew.
//!
//! Nothing here is pinned to a release or a bottle tag: every version,
//! revision, rebuild, checksum and cellar kind is read from the sandbox's own
//! `api/internal/packages.<tag>.jws.json` through [`Index`], and the bottle
//! references are built by the same `ops::plan::bottle_for` the installer
//! uses. A formula that loses the property a test needs (a `:all` bottle, a
//! fixed cellar) makes that test pick another candidate or skip with a reason,
//! never fail.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use fastbrew::api::index::Index;
use fastbrew::bottle::fetch::ManifestInfo;
use fastbrew::bottle::relocate::{self, RelocateArgs, relocate_keg};
use fastbrew::bottle::{BottleRef, extract, fetch};
use fastbrew::config::Config;
use fastbrew::keg::Keg;
use fastbrew::keg::link::{LinkOptions, link, optlink, unlink};
use fastbrew::model::FormulaEntry;
use fastbrew::model::formula::BottleCellar;
use fastbrew::ops::plan::bottle_for;
use fastbrew::platform::Host;

/// How many index candidates a property-based search downloads manifests for.
const MAX_CANDIDATES: usize = 12;
/// Bottles bigger than this are not worth downloading in a test.
const MAX_BOTTLE_BYTES: u64 = 20 * 1024 * 1024;

/// A formula's API entry together with the bottle reference the installer
/// would build from it.
struct Bottle {
    entry: FormulaEntry,
    reference: BottleRef,
}

impl Bottle {
    fn name(&self) -> &str {
        &self.entry.name
    }

    /// The upstream version, without Homebrew's `_<revision>` suffix: what the
    /// program itself prints.
    fn version(&self) -> &str {
        self.entry.stable_version.as_deref().unwrap_or_default()
    }

    /// `<version>[_<revision>]`, the keg and cache directory name.
    fn pkg_version(&self) -> &str {
        &self.reference.pkg_version
    }

    fn cellar(&self) -> BottleCellar {
        self.entry.bottle_cellar_kind()
    }
}

/// The fast index over the sandbox's API file, built once per test binary.
fn index() -> Option<&'static Index> {
    static INDEX: OnceLock<Option<Index>> = OnceLock::new();
    INDEX
        .get_or_init(|| {
            let cfg = Config::from_env().ok()?;
            Index::load(&cfg, &Host::detect().bottle_tag()).ok()
        })
        .as_ref()
}

/// The bottle for `name` exactly as `ops::install` resolves it.
fn bottle(cfg: &Config, name: &str) -> Option<Bottle> {
    let entry = index()?.formula(name)?;
    let reference = bottle_for(cfg, &entry)?;
    Some(Bottle { entry, reference })
}

/// [`bottle`], reporting a skip when the tag under test has no such bottle.
fn require_bottle(cfg: &Config, name: &str) -> Option<Bottle> {
    match bottle(cfg, name) {
        Some(b) => Some(b),
        None => {
            eprintln!(
                "skipping: no {name} bottle for {} in the cached API",
                Host::detect().bottle_tag()
            );
            None
        }
    }
}

/// True when `bottle` still has the cellar kind the test is about; otherwise
/// the caller skips, because the property moved rather than broke.
fn has_cellar(bottle: &Bottle, want: &BottleCellar) -> bool {
    let got = bottle.cellar();
    if &got == want {
        return true;
    }
    eprintln!(
        "skipping: {}'s bottle is now {got:?}, not {want:?}",
        bottle.name()
    );
    false
}

/// The sandbox configuration, or `None` when the test must skip.
fn sandbox() -> Option<Config> {
    if std::env::var_os("FASTBREW_REQUIRE_SANDBOX").is_none() {
        eprintln!("skipping: run through scripts/sandbox.sh");
        return None;
    }
    match Config::from_env() {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            eprintln!("skipping: {e}");
            None
        }
    }
}

fn network() -> bool {
    let on = std::env::var("FASTBREW_TEST_NETWORK").as_deref() == Ok("1");
    if !on {
        eprintln!("skipping: set FASTBREW_TEST_NETWORK=1 to run network tests");
    }
    on
}

/// A second sandbox whose prefix is exactly `len` bytes long, so a bottle
/// built for a `len`-byte prefix can be relocated into it. Shares the outer
/// sandbox's cache so nothing is downloaded twice.
fn short_prefix_sandbox(outer: &Config, len: usize) -> Option<Config> {
    const BASE: &str = "/tmp/fb-";
    if len <= BASE.len() {
        eprintln!("skipping: cannot build a {len}-byte prefix under {BASE}");
        return None;
    }
    let mut cfg = outer.clone();
    cfg.prefix = PathBuf::from(format!("{BASE}{}", "x".repeat(len - BASE.len())));
    cfg.cellar = cfg.prefix.join("Cellar");
    cfg.repository = cfg.prefix.clone();
    cfg.library = cfg.prefix.join("Library");
    assert_eq!(cfg.prefix.to_string_lossy().len(), len);
    if std::fs::create_dir_all(&cfg.prefix).is_err() {
        eprintln!("skipping: cannot create {}", cfg.prefix.display());
        return None;
    }
    Some(cfg)
}

/// Fetch, extract and relocate one bottle; returns the keg and the timings.
fn pour(cfg: &Config, bottle: &Bottle) -> (Keg, Duration, Duration) {
    let manifest = fetch::fetch_manifest(cfg, &bottle.reference, true).expect("manifest");
    let blob = fetch::fetch_blob(cfg, &bottle.reference, true).expect("blob");
    assert!(blob.is_file(), "cached blob missing");

    let started = Instant::now();
    let keg_path = extract::extract_bottle(cfg, &blob, bottle.name(), bottle.pkg_version(), true)
        .expect("extract")
        .keg;
    let extracted = started.elapsed();

    let started = Instant::now();
    relocate_keg(
        cfg,
        RelocateArgs {
            keg_path: &keg_path,
            cellar_kind: &bottle.cellar(),
            tab: &manifest.tab,
            openjdk_dep: None,
        },
    )
    .expect("relocate");
    let relocated = started.elapsed();

    (
        Keg::new(cfg, bottle.name(), bottle.pkg_version()),
        extracted,
        relocated,
    )
}

fn otool_l(path: &Path) -> String {
    let out = Command::new("/usr/bin/otool")
        .arg("-L")
        .arg(path)
        .output()
        .expect("otool -L");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn codesign_verifies(path: &Path) -> bool {
    Command::new("/usr/bin/codesign")
        .args(["--verify", "--strict"])
        .arg(path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// `optlink` then `link`, clearing a stale record first the way
/// `formula_installer.rb#link` does ("This keg was marked linked already").
/// Keeps the test idempotent when the sandbox is reused.
fn relink(cfg: &Config, keg: &Keg) {
    if cfg.linked_record(&keg.name).is_symlink() {
        unlink(cfg, keg, LinkOptions::default()).expect("unlink");
        let _ = std::fs::remove_file(cfg.linked_record(&keg.name));
    }
    optlink(cfg, keg, &[], &[]).expect("optlink");
    link(cfg, keg, &[], LinkOptions::default()).expect("link");
}

fn run(path: &Path, args: &[&str]) -> String {
    let out = Command::new(path).args(args).output().expect("run");
    assert!(
        out.status.success(),
        "{} {args:?} failed: {}",
        path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn pours_jq_and_its_oniguruma_dependency() {
    let Some(cfg) = sandbox() else { return };
    if !network() {
        return;
    }
    let (Some(oniguruma), Some(jq)) = (
        require_bottle(&cfg, "oniguruma"),
        require_bottle(&cfg, "jq"),
    ) else {
        return;
    };
    if !has_cellar(&jq, &BottleCellar::Any) {
        return;
    }

    let (onig, onig_extract, onig_relocate) = pour(&cfg, &oniguruma);
    let (jq_keg, jq_extract, jq_relocate) = pour(&cfg, &jq);
    println!(
        "oniguruma: extract {onig_extract:?}, relocate {onig_relocate:?}\n\
         jq: extract {jq_extract:?}, relocate {jq_relocate:?}"
    );

    // The blobs and manifests are cached under Homebrew's names, with the
    // short symlinks beside them.
    let blob_link = cfg.cache.join(format!("jq--{}", jq.pkg_version()));
    assert!(blob_link.is_symlink(), "{} missing", blob_link.display());
    assert_eq!(
        std::fs::read_link(&blob_link).unwrap(),
        PathBuf::from(format!(
            "downloads/{}--{}",
            fetch::url_hash(&jq.reference.blob_url()),
            jq.reference.filename()
        ))
    );
    assert!(
        cfg.cache
            .join(format!(
                "jq_bottle_manifest--{}",
                jq.reference.manifest_tag()
            ))
            .is_symlink()
    );

    // `changed_files` text relocation reached the pkg-config file.
    let pc = std::fs::read_to_string(jq_keg.path.join("lib/pkgconfig/libjq.pc")).unwrap();
    assert!(
        pc.contains(&cfg.prefix.to_string_lossy().into_owned()),
        "libjq.pc was not relocated:\n{pc}"
    );
    assert!(
        !pc.contains("@@HOMEBREW_"),
        "placeholder left in libjq.pc:\n{pc}"
    );

    // Mach-O relocation reached the linkage, and the signature is intact.
    let jq_bin = jq_keg.path.join("bin/jq");
    let linkage = otool_l(&jq_bin);
    let want = format!("{}/opt/oniguruma/lib/libonig", cfg.prefix.display());
    assert!(linkage.contains(&want), "expected {want} in:\n{linkage}");
    assert!(
        !linkage.contains("@@HOMEBREW_"),
        "placeholder left in:\n{linkage}"
    );
    assert!(codesign_verifies(&jq_bin), "jq signature is invalid");
    let libonig = std::fs::read_dir(onig.path.join("lib"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("libonig.") && n.ends_with(".dylib"))
        })
        .expect("the oniguruma keg ships a libonig dylib");
    assert!(
        codesign_verifies(&libonig),
        "{} signature is invalid",
        libonig.display()
    );

    // Link both kegs and run the linked binary.
    relink(&cfg, &onig);
    relink(&cfg, &jq_keg);

    assert!(cfg.opt_record("oniguruma").is_symlink());
    assert!(jq_keg.is_linked(&cfg) && jq_keg.is_optlinked(&cfg));
    let linked_jq = cfg.prefix.join("bin/jq");
    assert!(linked_jq.is_symlink(), "bin/jq was not linked");
    assert_eq!(
        run(&linked_jq, &["--version"]),
        format!("jq-{}", jq.version())
    );
    assert_eq!(run(&linked_jq, &["-n", "1+1"]), "2");

    // `unlink` takes the prefix links away again, leaving the keg alone.
    let removed = unlink(&cfg, &jq_keg, LinkOptions::default()).unwrap();
    assert!(removed > 0);
    assert!(!cfg.prefix.join("bin/jq").exists());
    assert!(jq_bin.is_file(), "unlink must not touch the keg");
    // Relink so a human poking at the sandbox afterwards finds a working jq.
    relink(&cfg, &jq_keg);
}

#[test]
fn pours_hello_without_relocation() {
    let Some(cfg) = sandbox() else { return };
    if !network() {
        return;
    }
    let Some(hello) = require_bottle(&cfg, "hello") else {
        return;
    };
    if !has_cellar(&hello, &BottleCellar::AnySkipRelocation) {
        return;
    }

    let (keg, extracted, relocated) = pour(&cfg, &hello);
    println!("hello: extract {extracted:?}, relocate {relocated:?}");

    relink(&cfg, &keg);
    let linked = cfg.prefix.join("bin/hello");
    assert!(linked.is_symlink());
    assert_eq!(run(&linked, &["--greeting=fastbrew"]), "fastbrew");
    assert!(
        run(&linked, &["--version"]).starts_with(&format!("hello (GNU Hello) {}", hello.version()))
    );
}

#[test]
fn pours_an_all_tag_skip_relocation_bottle() {
    let Some(cfg) = sandbox() else { return };
    if !network() {
        return;
    }
    // `ack` is served under `bottle_tag: ":all"`: the manifest is selected by
    // `<version>.all` and the blob cached under the `all` tag. It is also
    // `:any_skip_relocation` with a non-empty `changed_files`, so it proves the
    // text step still runs there.
    let Some(ack) = require_bottle(&cfg, "ack") else {
        return;
    };
    if !ack.reference.tag.is_all() {
        eprintln!(
            "skipping: ack's bottle is served under {}, not :all",
            ack.reference.tag
        );
        return;
    }
    if !has_cellar(&ack, &BottleCellar::AnySkipRelocation) {
        return;
    }

    assert_eq!(
        ack.reference.ref_name(),
        format!("{}.all", ack.pkg_version()),
        "the `all` manifest is selected by <version>.all"
    );
    assert!(
        ack.reference
            .filename()
            .starts_with(&format!("ack--{}.all.bottle", ack.pkg_version())),
        "the cached blob carries the `all` tag: {}",
        ack.reference.filename()
    );

    let manifest = fetch::fetch_manifest(&cfg, &ack.reference, true).expect("manifest");
    let changed = manifest.tab.changed_files.clone().unwrap_or_default();
    assert!(
        !changed.is_empty(),
        "ack's bottle no longer records changed_files"
    );
    let blob = fetch::fetch_blob(&cfg, &ack.reference, true).expect("blob");
    assert!(
        cfg.cache
            .join(format!("ack--{}", ack.pkg_version()))
            .is_symlink()
    );

    let keg_path = extract::extract_bottle(&cfg, &blob, ack.name(), ack.pkg_version(), true)
        .unwrap()
        .keg;
    let report = relocate_keg(
        &cfg,
        RelocateArgs {
            keg_path: &keg_path,
            // `:any_skip_relocation`: no Mach-O work, but the text step must
            // still expand `@@HOMEBREW_PERL@@` in the shebang.
            cellar_kind: &BottleCellar::AnySkipRelocation,
            tab: &manifest.tab,
            openjdk_dep: None,
        },
    )
    .unwrap();
    assert_eq!(report.text_files_changed, changed);
    assert!(report.macho_files_changed.is_empty());
    if changed.iter().any(|f| f == "bin/ack") {
        let shebang = std::fs::read_to_string(keg_path.join("bin/ack")).unwrap();
        let shebang = shebang.lines().next().unwrap().to_string();
        // `ack` declares `perl` directly, so the brewed perl is used.
        assert_eq!(
            shebang,
            format!("#!{}/opt/perl/bin/perl", cfg.prefix.display()),
            "perl placeholder was not expanded"
        );
    }
}

/// Formulae whose bottle is built for a fixed cellar and has no runtime
/// dependency, so a single small download exercises build-prefix relocation.
/// `epic5` first: it has been such a bottle for years and is tiny.
fn fixed_cellar_names(index: &Index) -> Vec<String> {
    let is_fixed = |f: &FormulaEntry| matches!(f.bottle_cellar_kind(), BottleCellar::Fixed(_));
    index
        .all_formulae()
        .into_iter()
        .filter(|f| {
            f.name != "epic5"
                && f.has_bottle()
                && is_fixed(f)
                && !f.dependencies().iter().any(|d| d.is_runtime())
        })
        .map(|f| f.name)
        .take(MAX_CANDIDATES)
        .collect()
}

/// The first of `names` whose bottle is small, built for a fixed cellar the
/// sandbox prefix is too long for, and records `binary_relocation_files`.
fn fixed_cellar_case(cfg: &Config, names: &[String]) -> Option<(Bottle, ManifestInfo)> {
    names.iter().find_map(|name| {
        let candidate = bottle(cfg, name)?;
        let cellar = candidate.cellar();
        if !matches!(cellar, BottleCellar::Fixed(_)) {
            return None;
        }
        let manifest = fetch::fetch_manifest(cfg, &candidate.reference, true).ok()?;
        let relocations = manifest
            .tab
            .binary_relocation_files
            .as_deref()
            .unwrap_or(&[]);
        let small = manifest.bottle_size.is_none_or(|s| s < MAX_BOTTLE_BYTES);
        // The refusal branch is only reachable while the sandbox prefix is
        // longer than the build prefix, which is what this test is about.
        let refused = !relocate::compatible_locations(cfg, &cellar, &manifest.tab);
        (!relocations.is_empty() && small && refused).then_some((candidate, manifest))
    })
}

#[test]
fn relocates_the_build_prefix_of_a_fixed_cellar_bottle() {
    let Some(outer) = sandbox() else { return };
    if !network() {
        return;
    }
    let Some(index) = index() else { return };
    let found = fixed_cellar_case(&outer, &["epic5".to_string()])
        .or_else(|| fixed_cellar_case(&outer, &fixed_cellar_names(index)));
    let Some((bottle, manifest)) = found else {
        eprintln!(
            "skipping: no small fixed-cellar bottle with binary_relocation_files for {}",
            Host::detect().bottle_tag()
        );
        return;
    };
    let name = bottle.name().to_string();
    let fixed = bottle.cellar();
    let BottleCellar::Fixed(cellar) = &fixed else {
        unreachable!("fixed_cellar_case only returns fixed-cellar bottles")
    };
    let build_prefix = Path::new(cellar)
        .parent()
        .expect("a cellar has a parent prefix")
        .to_string_lossy()
        .into_owned();
    let relocations = manifest
        .tab
        .binary_relocation_files
        .clone()
        .expect("fixed_cellar_case requires them");

    // The sandbox prefix is far longer than the build prefix, so the bottle is
    // refused before anything is extracted, exactly as `pour_bottle?` does.
    assert!(!relocate::compatible_locations(
        &outer,
        &fixed,
        &manifest.tab
    ));
    let message = relocate::incompatible_locations_message(&outer, &name, &fixed, &manifest.tab);
    assert!(
        message.starts_with(&format!(
            "{name} was built for {build_prefix} and can only be relocated to a prefix with a \
             maximum length of {} characters",
            build_prefix.len()
        )),
        "{message}"
    );

    // A prefix of exactly the build prefix's length fits, so the raw prefix
    // strings can be patched in place.
    let Some(cfg) = short_prefix_sandbox(&outer, build_prefix.len()) else {
        return;
    };
    let short_prefix = cfg.prefix.to_string_lossy().into_owned();
    assert!(relocate::compatible_locations(&cfg, &fixed, &manifest.tab));
    let blob = fetch::fetch_blob(&cfg, &bottle.reference, true).expect("blob");
    let keg_path = extract::extract_bottle(&cfg, &blob, &name, bottle.pkg_version(), true)
        .expect("extract")
        .keg;
    let report = relocate_keg(
        &cfg,
        RelocateArgs {
            keg_path: &keg_path,
            cellar_kind: &fixed,
            tab: &manifest.tab,
            openjdk_dep: None,
        },
    )
    .expect("relocate");

    assert_eq!(
        report.relocated_build_prefix.as_deref(),
        Some(build_prefix.as_str())
    );
    assert_eq!(report.relocated_files, relocations);
    let binary = keg_path.join(&relocations[0]);
    let bytes = std::fs::read(&binary).unwrap();
    let needle = build_prefix.as_bytes();
    assert!(
        !bytes.windows(needle.len()).any(|w| w == needle),
        "the build prefix is still baked into {}",
        binary.display()
    );
    let needle = short_prefix.as_bytes();
    assert!(
        bytes.windows(needle.len()).any(|w| w == needle),
        "the new prefix was not written to {}",
        binary.display()
    );
    assert!(
        codesign_verifies(&binary),
        "the patched binary was not re-signed"
    );
    // The mode survived the rewrite, so the binary is still executable.
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&binary).unwrap().permissions().mode() & 0o111,
        0o111
    );
    let _ = std::fs::remove_dir_all(&cfg.prefix);
}

#[test]
fn fetch_all_keeps_input_order_and_isolates_failures() {
    let Some(cfg) = sandbox() else { return };
    if !network() {
        return;
    }
    let (Some(oniguruma), Some(hello), Some(ack)) = (
        require_bottle(&cfg, "oniguruma"),
        require_bottle(&cfg, "hello"),
        require_bottle(&cfg, "ack"),
    ) else {
        return;
    };
    let mut broken = hello.reference.clone();
    broken.sha256 = "0".repeat(64);
    // `ack` is served under the `all` tag; `bottle_for` already knows that.
    let refs = vec![oniguruma.reference.clone(), broken, ack.reference.clone()];

    let results = fetch::fetch_all(&cfg, &refs, true);
    assert_eq!(results.len(), 3);
    let (manifest, blob) = results[0].as_ref().expect("oniguruma");
    assert!(
        manifest.bottle_size.is_some_and(|size| size > 0),
        "the manifest records the bottle size: {:?}",
        manifest.bottle_size
    );
    assert!(
        blob.to_string_lossy()
            .ends_with(&oniguruma.reference.filename()),
        "{}",
        blob.display()
    );
    let err = results[1].as_ref().unwrap_err().to_string();
    assert!(err.contains("bottle checksum"), "{err}");
    let (_, ack_blob) = results[2].as_ref().expect("ack");
    assert!(
        ack_blob
            .to_string_lossy()
            .ends_with(&ack.reference.filename()),
        "{}",
        ack_blob.display()
    );
}

#[test]
fn reuses_a_cached_blob_without_redownloading() {
    let Some(cfg) = sandbox() else { return };
    if !network() {
        return;
    }
    let Some(oniguruma) = require_bottle(&cfg, "oniguruma") else {
        return;
    };
    let reference = &oniguruma.reference;
    let first = fetch::fetch_blob(&cfg, reference, true).expect("blob");
    let mtime = std::fs::metadata(&first).unwrap().modified().unwrap();
    assert_eq!(
        fetch::cached_blob_path(&cfg, reference).as_ref(),
        Some(&first)
    );
    let second = fetch::fetch_blob(&cfg, reference, true).expect("blob");
    assert_eq!(first, second);
    assert_eq!(
        std::fs::metadata(&second).unwrap().modified().unwrap(),
        mtime,
        "a cached blob must not be downloaded again"
    );
}

/// Runs without the network: the sandbox prefix must behave like a real one.
#[test]
fn links_and_unlinks_a_synthetic_keg_in_the_sandbox() {
    let Some(cfg) = sandbox() else { return };
    let keg = Keg::new(&cfg, "fastbrew-demo", "1.0");
    std::fs::create_dir_all(keg.path.join("bin")).unwrap();
    std::fs::create_dir_all(keg.path.join("share/man/man1")).unwrap();
    std::fs::write(keg.path.join("bin/demo"), "#!/bin/sh\necho demo\n").unwrap();
    std::fs::write(keg.path.join("share/man/man1/demo.1"), ".TH DEMO 1\n").unwrap();
    std::fs::write(
        keg.path.join("INSTALL_RECEIPT.json"),
        br#"{"aliases":[],"source":{}}"#,
    )
    .unwrap();

    optlink(&cfg, &keg, &[], &[]).unwrap();
    let created = link(&cfg, &keg, &[], LinkOptions::default()).unwrap();
    assert!(created >= 2);
    assert!(cfg.prefix.join("bin/demo").is_symlink());
    assert!(cfg.prefix.join("share/man/man1/demo.1").is_symlink());

    unlink(&cfg, &keg, LinkOptions::default()).unwrap();
    assert!(!cfg.prefix.join("bin/demo").exists());
    assert!(!keg.is_linked(&cfg));
    std::fs::remove_dir_all(cfg.rack("fastbrew-demo")).unwrap();
    let _ = std::fs::remove_file(cfg.opt_record("fastbrew-demo"));
}
