//! Scratch probe: fetch, extract and relocate real bottles in the sandbox.
//! Run with `scripts/sandbox.sh run -- cargo run --example fetch_probe`.

use std::time::Instant;

use fastbrew::bottle::relocate::{RelocateArgs, relocate_keg};
use fastbrew::bottle::{BottleRef, extract, fetch};
use fastbrew::config::Config;
use fastbrew::model::formula::BottleCellar;
use fastbrew::platform::Host;

fn main() {
    let cfg = Config::from_env().unwrap();
    let tag = Host::detect().bottle_tag();
    let refs = [
        (
            "oniguruma",
            "6.9.10",
            0,
            "eb6bda3b333f497b5d294388f39fd0902a5c79a52ae16858eff711d2d104cc4d",
        ),
        (
            "jq",
            "1.8.2",
            1,
            "ca67c64d0aaf1e5472790ec2cc081ff7972316f27095d8a8aab81b3321247036",
        ),
    ];
    let bottles: Vec<BottleRef> = refs
        .iter()
        .map(|(name, version, rebuild, sha)| BottleRef {
            name: (*name).into(),
            pkg_version: (*version).into(),
            rebuild: *rebuild,
            tag: tag.clone(),
            root_url: cfg.bottle_domain.clone(),
            sha256: (*sha).into(),
        })
        .collect();

    let started = Instant::now();
    let fetched = fetch::fetch_all(&cfg, &bottles, false);
    println!("fetch_all: {:?}", started.elapsed());

    for (b, result) in bottles.iter().zip(fetched) {
        let (info, blob) = result.unwrap();
        let t0 = Instant::now();
        let keg = extract::extract_bottle(&cfg, &blob, &b.name, &b.pkg_version, true).unwrap();
        let extracted = t0.elapsed();
        let t1 = Instant::now();
        let report = relocate_keg(
            &cfg,
            RelocateArgs {
                keg_path: &keg,
                cellar_kind: &BottleCellar::Any,
                tab: &info.tab,
                openjdk_dep: None,
            },
        )
        .unwrap();
        println!(
            "{}: extract {:?}, relocate {:?}, text {:?}, mach-o {:?}",
            b.name,
            extracted,
            t1.elapsed(),
            report.text_files_changed,
            report.macho_files_changed
        );
    }
}
