//! Headless dump of the NGX server offering (for verification without GUI).
//!
//! Usage: cargo run --example ngx_dump

use dlssdl::ngx;

fn main() -> anyhow::Result<()> {
    let groups = ngx::fetch_offers(&mut |m| eprintln!("… {m}"))?;
    println!();
    for g in groups {
        println!(
            "== {} (server dir: {})",
            g.feature.title(),
            g.feature.dir()
        );
        for o in &g.offers {
            let sidecar = o
                .candidates
                .first()
                .map_or(false, |c| c.sha256_url.is_some());
            println!(
                "  v{:>9}{:<14} {:>8}  mirrors={} sha256={}  -> {}",
                o.version,
                o.tag(),
                ngx::human_size(o.size),
                o.candidates.len(),
                if sidecar { "yes" } else { "no" },
                o.feature.consumer_name().unwrap_or_else(|| "<sl.*.dll from zip>".to_string())
            );
        }
        println!();
    }
    Ok(())
}
