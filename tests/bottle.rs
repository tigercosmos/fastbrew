//! End-to-end bottle pours inside the sandbox.
//!
//! Run with `FASTBREW_TEST_NETWORK=1 scripts/sandbox.sh test bottle`. Without
//! `FASTBREW_TEST_NETWORK=1` the network tests skip; outside the sandbox
//! (`scripts/sandbox.sh`, which exports `FASTBREW_REQUIRE_SANDBOX=1`) every
//! test skips, so `cargo test` never touches the host Homebrew.
//!
//! The bottle checksums below come from the host's read-only API cache. To
//! refresh them:
//!
//! ```sh
//! python3 -c 'import json;d=json.load(open("'"$HOME"'/Library/Caches/Homebrew/api/internal/packages.arm64_tahoe.jws.json"));
//! f=json.loads(d["payload"])["formulae"]
//! print({n: (f[n]["stable_version"], f[n].get("bottle_rebuild"), f[n]["bottle_checksum"], f[n].get("bottle_cellar")) for n in ("jq","oniguruma","hello")})'
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use fastbrew::bottle::relocate::{RelocateArgs, relocate_keg};
use fastbrew::bottle::{BottleRef, extract, fetch};
use fastbrew::config::Config;
use fastbrew::keg::Keg;
use fastbrew::keg::link::{LinkOptions, link, optlink, unlink};
use fastbrew::model::formula::BottleCellar;
use fastbrew::platform::Host;

/// `(name, pkg_version, rebuild, bottle_checksum, bottle_cellar)`.
const ONIGURUMA: Bottle = Bottle {
    name: "oniguruma",
    version: "6.9.10",
    rebuild: 0,
    sha256: "eb6bda3b333f497b5d294388f39fd0902a5c79a52ae16858eff711d2d104cc4d",
};
const JQ: Bottle = Bottle {
    name: "jq",
    version: "1.8.2",
    rebuild: 1,
    sha256: "ca67c64d0aaf1e5472790ec2cc081ff7972316f27095d8a8aab81b3321247036",
};
const HELLO: Bottle = Bottle {
    name: "hello",
    version: "2.12.3",
    rebuild: 1,
    sha256: "ae6237e3001bd354783f469d754cee875ee9828910461b85a5803f5990213dde",
};
/// A `bottle_tag: ":all"` bottle: the manifest entry is `<version>.all` and the
/// cache name carries the `all` tag. It is also `:any_skip_relocation` with a
/// non-empty `changed_files`, so it proves the text step still runs there.
const ACK: Bottle = Bottle {
    name: "ack",
    version: "3.10.0",
    rebuild: 0,
    sha256: "0f50e7b207da891500f42b5671413f290d4db5fea49943cfefcc74a3684760d9",
};

struct Bottle {
    name: &'static str,
    version: &'static str,
    rebuild: u32,
    sha256: &'static str,
}

impl Bottle {
    fn reference(&self, cfg: &Config) -> BottleRef {
        BottleRef {
            name: self.name.to_string(),
            pkg_version: self.version.to_string(),
            rebuild: self.rebuild,
            tag: Host::detect().bottle_tag(),
            root_url: cfg.bottle_domain.clone(),
            sha256: self.sha256.to_string(),
        }
    }
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

/// Fetch, extract and relocate one bottle; returns the keg and the timings.
fn pour(cfg: &Config, bottle: &Bottle, cellar: &BottleCellar) -> (Keg, Duration, Duration) {
    let reference = bottle.reference(cfg);
    let manifest = fetch::fetch_manifest(cfg, &reference, true).expect("manifest");
    let blob = fetch::fetch_blob(cfg, &reference, true).expect("blob");
    assert!(blob.is_file(), "cached blob missing");

    let started = Instant::now();
    let keg_path =
        extract::extract_bottle(cfg, &blob, bottle.name, bottle.version, true).expect("extract");
    let extracted = started.elapsed();

    let started = Instant::now();
    relocate_keg(
        cfg,
        RelocateArgs {
            keg_path: &keg_path,
            cellar_kind: cellar,
            tab: &manifest.tab,
            openjdk_dep: None,
        },
    )
    .expect("relocate");
    let relocated = started.elapsed();

    (
        Keg::new(cfg, bottle.name, bottle.version),
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

    let (onig, onig_extract, onig_relocate) = pour(&cfg, &ONIGURUMA, &BottleCellar::Any);
    let (jq, jq_extract, jq_relocate) = pour(&cfg, &JQ, &BottleCellar::Any);
    println!(
        "oniguruma: extract {onig_extract:?}, relocate {onig_relocate:?}\n\
         jq: extract {jq_extract:?}, relocate {jq_relocate:?}"
    );

    // The blobs and manifests are cached under Homebrew's names, with the
    // short symlinks beside them.
    let blob_link = cfg.cache.join("jq--1.8.2");
    assert!(blob_link.is_symlink(), "{} missing", blob_link.display());
    assert_eq!(
        std::fs::read_link(&blob_link).unwrap(),
        PathBuf::from(format!(
            "downloads/{}--jq--1.8.2.{}.bottle.1.tar.gz",
            fetch::url_hash(&JQ.reference(&cfg).blob_url()),
            Host::detect().bottle_tag()
        ))
    );
    assert!(cfg.cache.join("jq_bottle_manifest--1.8.2-1").is_symlink());

    // `changed_files` text relocation reached the pkg-config file.
    let pc = std::fs::read_to_string(jq.path.join("lib/pkgconfig/libjq.pc")).unwrap();
    assert!(
        pc.contains(&cfg.prefix.to_string_lossy().into_owned()),
        "libjq.pc was not relocated:\n{pc}"
    );
    assert!(
        !pc.contains("@@HOMEBREW_"),
        "placeholder left in libjq.pc:\n{pc}"
    );

    // Mach-O relocation reached the linkage, and the signature is intact.
    let jq_bin = jq.path.join("bin/jq");
    let linkage = otool_l(&jq_bin);
    let want = format!("{}/opt/oniguruma/lib/libonig.5.dylib", cfg.prefix.display());
    assert!(linkage.contains(&want), "expected {want} in:\n{linkage}");
    assert!(
        !linkage.contains("@@HOMEBREW_"),
        "placeholder left in:\n{linkage}"
    );
    assert!(codesign_verifies(&jq_bin), "jq signature is invalid");
    assert!(
        codesign_verifies(&onig.path.join("lib/libonig.5.dylib")),
        "libonig signature is invalid"
    );

    // Link both kegs and run the linked binary.
    relink(&cfg, &onig);
    relink(&cfg, &jq);

    assert!(cfg.opt_record("oniguruma").is_symlink());
    assert!(jq.is_linked(&cfg) && jq.is_optlinked(&cfg));
    let linked_jq = cfg.prefix.join("bin/jq");
    assert!(linked_jq.is_symlink(), "bin/jq was not linked");
    assert_eq!(run(&linked_jq, &["--version"]), "jq-1.8.2");
    assert_eq!(run(&linked_jq, &["-n", "1+1"]), "2");

    // `unlink` takes the prefix links away again, leaving the keg alone.
    let removed = unlink(&cfg, &jq, LinkOptions::default()).unwrap();
    assert!(removed > 0);
    assert!(!cfg.prefix.join("bin/jq").exists());
    assert!(jq_bin.is_file(), "unlink must not touch the keg");
    // Relink so a human poking at the sandbox afterwards finds a working jq.
    relink(&cfg, &jq);
}

#[test]
fn pours_hello_without_relocation() {
    let Some(cfg) = sandbox() else { return };
    if !network() {
        return;
    }

    let (hello, extracted, relocated) = pour(&cfg, &HELLO, &BottleCellar::AnySkipRelocation);
    println!("hello: extract {extracted:?}, relocate {relocated:?}");

    relink(&cfg, &hello);
    let linked = cfg.prefix.join("bin/hello");
    assert!(linked.is_symlink());
    assert_eq!(run(&linked, &["--greeting=fastbrew"]), "fastbrew");
    assert!(run(&linked, &["--version"]).starts_with("hello (GNU Hello) 2.12.3"));
}

#[test]
fn pours_an_all_tag_skip_relocation_bottle() {
    let Some(cfg) = sandbox() else { return };
    if !network() {
        return;
    }

    // The manifest is selected by `<version>.all` and cached under the `all`
    // tag, exactly as Homebrew names it.
    let reference = BottleRef {
        tag: fastbrew::platform::BottleTag::all(),
        ..ACK.reference(&cfg)
    };
    assert_eq!(reference.ref_name(), "3.10.0.all");
    assert_eq!(reference.filename(), "ack--3.10.0.all.bottle.tar.gz");
    let manifest = fetch::fetch_manifest(&cfg, &reference, true).expect("manifest");
    assert_eq!(
        manifest.tab.changed_files.as_deref(),
        Some(&["bin/ack".to_string()][..])
    );
    let blob = fetch::fetch_blob(&cfg, &reference, true).expect("blob");
    assert!(cfg.cache.join("ack--3.10.0").is_symlink());

    let keg_path = extract::extract_bottle(&cfg, &blob, ACK.name, ACK.version, true).unwrap();
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
    assert_eq!(report.text_files_changed, vec!["bin/ack".to_string()]);
    assert!(report.macho_files_changed.is_empty());
    let shebang = std::fs::read_to_string(keg_path.join("bin/ack")).unwrap();
    let shebang = shebang.lines().next().unwrap().to_string();
    // `ack` declares `perl` directly, so the brewed perl is used.
    assert_eq!(
        shebang,
        format!("#!{}/opt/perl/bin/perl", cfg.prefix.display()),
        "perl placeholder was not expanded"
    );
}

#[test]
fn reuses_a_cached_blob_without_redownloading() {
    let Some(cfg) = sandbox() else { return };
    if !network() {
        return;
    }
    let reference = ONIGURUMA.reference(&cfg);
    let first = fetch::fetch_blob(&cfg, &reference, true).expect("blob");
    let mtime = std::fs::metadata(&first).unwrap().modified().unwrap();
    assert_eq!(
        fetch::cached_blob_path(&cfg, &reference).as_ref(),
        Some(&first)
    );
    let second = fetch::fetch_blob(&cfg, &reference, true).expect("blob");
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
