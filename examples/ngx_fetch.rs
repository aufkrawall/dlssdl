//! Headless downloader (for verification without GUI).
//!
//! Usage: cargo run --example ngx_fetch -- <dlss|dlssd|dlssg|sl> [version]
//!
//! Downloads the given feature (newest version, or the exact version string)
//! into ./ngx_fetch_out/<version>/ using the same code path as the GUI.

use dlssdl::ngx::{self, Feature};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: ngx_fetch <dlss|dlssd|dlssg|sl> [version]");
        std::process::exit(2);
    }
    let feature = match args[1].as_str() {
        "dlss" => Feature::DlssSr,
        "dlssd" => Feature::DlssRr,
        "dlssg" => Feature::DlssFg,
        "sl" => Feature::Streamline,
        other => anyhow::bail!("unknown feature '{other}'"),
    };
    let want_version = args.get(2).map(|s| s.as_str());

    let groups = ngx::fetch_offers(&mut |m| eprintln!("… {m}"))?;
    let group = groups
        .iter()
        .find(|g| g.feature == feature)
        .ok_or_else(|| anyhow::anyhow!("feature not offered"))?;
    let offer = match want_version {
        Some(v) => group
            .offers
            .iter()
            .find(|o| o.version == v)
            .ok_or_else(|| anyhow::anyhow!("version {v} not offered (available: {:?})", group.offers.iter().map(|o| o.version.clone()).collect::<Vec<_>>()))?,
        None => group.offers.first().expect("at least one offer"),
    };

    let out = std::path::Path::new("ngx_fetch_out").join(&offer.version);
    println!("downloading {} {} -> {}", feature.title(), offer.version, out.display());
    let mut progress = |done: u64, total: u64| {
        if total > 0 {
            eprint!("\r  {:>5.1}%", done as f64 / total as f64 * 100.0);
        }
    };
    let files = ngx::download_offer(offer, &out, &mut progress, &mut |m| eprintln!("  {m}"))?;
    eprintln!();
    println!("wrote:");
    for f in &files {
        let meta = std::fs::metadata(out.join(f))?;
        println!("  {:40} {}", f, ngx::human_size(meta.len()));
    }
    Ok(())
}
