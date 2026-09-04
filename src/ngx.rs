//! NVIDIA NGX OTA server (https://ngx.download.nvidia.com) access.
//!
//! The NGX update servers are public S3 buckets whose object keys follow the pattern
//! `<namespace>/org/nvidia/team/ngx/models/<feature>/versions/<version-id>/files/<name>`.
//!
//! Relevant features:
//!  - `dlss`  -> DLSS Super Resolution, payload `160_E658700.bin` (= nvngx_dlss.dll)
//!  - `dlssd` -> DLSS Ray Reconstruction, payload `160_E658700.bin` (= nvngx_dlssd.dll)
//!  - `dlssg` -> DLSS Frame Generation, payload `160_E658700.bin` (= nvngx_dlssg.dll)
//!  - `sl_sdk_0` -> Streamline SDK, payload `160_E658703.zip` (contains sl.common.dll, ...)
//!
//! `<version-id>` packs as `major << 16 | minor << 8 | patch`
//! (e.g. 20317442 -> 310.5.2, 133888 -> 2.11.0; verified against the PE resources
//! and `nvngx_package_config.txt` shipped in the packages).
//!
//! The DLLs are stored as plain PE files (the `.bin` suffix is not an encryption
//! wrapper), so they only need to be renamed to their consumer names.

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const NGX_HOST: &str = "https://ngx.download.nvidia.com";
const LIST_ARGS: &str = "?list-type=2&max-keys=1000";
const MODELS_MARKER: &str = "org/nvidia/team/ngx/models/";

// ---------------------------------------------------------------------------
// Features
// ---------------------------------------------------------------------------

/// A server feature, identified by its directory under `.../ngx/models/`.
///
/// Deliberately dynamic: any NVIDIA `dlss*` feature that shows up on the OTA
/// servers in the future (e.g. `dlssnr` for Neural Rendering) is picked up
/// automatically — the consumer file name follows the existing
/// `nvngx_<dir>.dll` convention.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Feature {
    dir: String,
}

impl Feature {
    /// Validate a server feature dir. Supported: the DLSS family (`dlss*`,
    /// excluding the `dlss_override` zip bundles) and Streamline (`sl_sdk*`).
    pub fn from_dir(dir: &str) -> Option<Feature> {
        let ok = dir.starts_with("sl_sdk")
            || (dir.starts_with("dlss") && !dir.contains("override"));
        if ok {
            Some(Feature { dir: dir.to_string() })
        } else {
            None
        }
    }

    /// Directory on the NGX server.
    pub fn dir(&self) -> &str {
        &self.dir
    }

    pub fn is_streamline(&self) -> bool {
        self.dir.starts_with("sl_sdk")
    }

    /// Human readable title (used in the GUI).
    pub fn title(&self) -> String {
        match self.dir.as_str() {
            "dlss" => "DLSS Super Resolution".into(),
            "dlssd" => "DLSS Ray Reconstruction".into(),
            "dlssg" => "DLSS Frame Generation".into(),
            "sl_sdk_0" => "Streamline SDK".into(),
            other => other.to_string(),
        }
    }

    /// Name the payload must get on the consumer side (`None` for zips that
    /// already contain properly named DLLs).
    pub fn consumer_name(&self) -> Option<String> {
        if self.is_streamline() {
            None
        } else {
            Some(format!("nvngx_{}.dll", self.dir))
        }
    }

    /// The NGX app id NVIDIA currently ships one universal file for.
    fn canonical_app_id(&self) -> &'static str {
        if self.is_streamline() {
            "E658703"
        } else {
            "E658700"
        }
    }

    fn payload_ext(&self) -> &'static str {
        if self.is_streamline() { "zip" } else { "bin" }
    }

    fn canonical_payload(&self) -> String {
        format!("160_{}.{}", self.canonical_app_id(), self.payload_ext())
    }

    /// Sort order in the UI: the well-known features first, then new ones.
    fn rank(&self) -> (u8, String) {
        match self.dir.as_str() {
            "dlss" => (0, String::new()),
            "dlssd" => (1, String::new()),
            "dlssg" => (2, String::new()),
            "sl_sdk_0" => (3, String::new()),
            other => (4, other.to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// Model of what the server offers
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Candidate {
    pub url: String,
    pub sha256_url: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Offer {
    pub feature: Feature,
    pub version_id: u32,
    /// Decoded human version, e.g. "310.7.128".
    pub version: String,
    /// Exact payload file name, e.g. "160_E658700.bin" or "1B0_E658703.zip".
    pub payload: String,
    /// Payload size in bytes.
    pub size: u64,
    /// Mirrors (different bucket namespaces), tried in order. Candidates with a
    /// sha256 sidecar come first.
    pub candidates: Vec<Candidate>,
}

#[derive(Clone, Debug)]
pub struct Group {
    pub feature: Feature,
    /// Sorted by version id, newest first.
    pub offers: Vec<Offer>,
}

/// Decode a packed NGX version id: `major << 16 | minor << 8 | patch`.
pub fn decode_version(v: u32) -> String {
    format!("{}.{}.{}", v >> 16, (v >> 8) & 0xFF, v & 0xFF)
}

/// Split a payload into (snippet-prefix, app-id), extension stripped:
/// `160_E658700.bin` -> ("160", "E658700").
fn split_payload(payload: &str) -> (&str, &str) {
    let stem = payload
        .strip_suffix(".bin")
        .or_else(|| payload.strip_suffix(".zip"))
        .unwrap_or(payload);
    let mut it = stem.splitn(2, '_');
    (it.next().unwrap_or(""), it.next().unwrap_or(""))
}

fn payload_rank(payload: &str, feature: &Feature) -> u8 {
    let (prefix, app) = split_payload(payload);
    (app.eq_ignore_ascii_case(feature.canonical_app_id()) as u8) * 2
        + prefix.eq_ignore_ascii_case("160") as u8
}

impl Offer {
    /// Tag for non-standard payloads, e.g. " [1B0]" (new arch prefix) or
    /// " [app_865EFBC]" (legacy per-game DLSS 2.x snippet).
    pub fn tag(&self) -> String {
        let (prefix, app) = split_payload(&self.payload);
        let mut tags: Vec<String> = Vec::new();
        if !prefix.eq_ignore_ascii_case("160") {
            tags.push(prefix.to_ascii_uppercase());
        }
        if !app.eq_ignore_ascii_case(self.feature.canonical_app_id()) {
            tags.push(format!("app_{}", app.to_ascii_uppercase()));
        }
        if tags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", tags.join("/"))
        }
    }
}

struct RawEntry {
    ns: String,
    feature: Feature,
    version_id: u32,
    payload: String,
    is_sidecar: bool,
    size: u64,
}

/// A version pin from `nvngx_server_config.txt`, e.g. `[dlss] app_E658700 = 310.9.0`.
struct ConfigPin {
    feature: Feature,
    app_id: String,
    version: String,
}

fn is_hex(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `<snippet-id>_<app-id>.bin`, e.g. `160_E658700.bin`.
/// 160 = NV_GPU_ARCHITECTURE_ID of Turing; NVIDIA ships this file for every
/// arch today, but other prefixes (170/180/190/1B0, ...) may appear later.
fn is_bin_payload(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".bin") else { return false };
    let mut it = stem.split('_');
    let (Some(a), Some(b)) = (it.next(), it.next()) else { return false };
    it.next().is_none() && is_hex(a) && is_hex(b)
}

/// `<snippet-id>_<app-id>.zip`, e.g. `160_E658703.zip`.
fn is_zip_payload(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".zip") else { return false };
    let mut it = stem.split('_');
    let (Some(a), Some(b)) = (it.next(), it.next()) else { return false };
    it.next().is_none() && is_hex(a) && is_hex(b)
}

fn parse_key(key: &str, size: u64) -> Option<RawEntry> {
    let pos = key.find(MODELS_MARKER)?;
    let ns = key[..pos].to_string();
    let rest = &key[pos + MODELS_MARKER.len()..];
    let mut it = rest.split('/');
    let feature = Feature::from_dir(it.next()?)?;
    if it.next()? != "versions" {
        return None;
    }
    let version_id: u32 = it.next()?.parse().ok()?;
    if it.next()? != "files" {
        return None;
    }
    let raw_name = it.next()?;
    if it.next().is_some() {
        return None;
    }
    let (name, is_sidecar) = match raw_name.strip_suffix(".sha256") {
        Some(stem) => (stem, true),
        None => (raw_name, false),
    };
    let payload_ok = if feature.is_streamline() {
        is_zip_payload(name)
    } else {
        is_bin_payload(name)
    };
    if !payload_ok {
        return None;
    }
    Some(RawEntry {
        ns,
        feature,
        version_id,
        payload: name.to_string(),
        is_sidecar,
        size,
    })
}

// ---------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------

/// Agent for existence probes: strict overall timeout so dozens of HEAD
/// requests cannot stall the update check.
fn build_probe_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout(Duration::from_secs(15))
        .user_agent("dlssdl/0.1 (NGX DLSS update checker)")
        .build()
}

fn build_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .user_agent("dlssdl/0.1 (NGX DLSS update checker)")
        .build()
}

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn child_text<'a>(node: roxmltree::Node<'a, 'a>, name: &str) -> Option<&'a str> {
    node.children()
        .find(|c| c.tag_name().name() == name)
        .and_then(|c| c.text())
}

/// List the whole bucket (all pages) and return `(key, size)` pairs.
fn list_all(agent: &ureq::Agent, log: &mut dyn FnMut(String)) -> Result<Vec<(String, u64)>> {
    let mut out = Vec::new();
    let mut token = String::new();
    for page in 1..=100 {
        let url = if token.is_empty() {
            format!("{NGX_HOST}/{LIST_ARGS}")
        } else {
            format!("{NGX_HOST}/{LIST_ARGS}&continuation-token={}", url_encode(&token))
        };
        let body = agent
            .get(&url)
            .call()
            .map_err(|e| anyhow!("listing request failed: {e}"))?
            .into_string()
            .map_err(|e| anyhow!("reading listing failed: {e}"))?;
        let doc = roxmltree::Document::parse(&body).context("parsing S3 listing XML")?;

        let mut n = 0;
        for contents in doc.descendants().filter(|n| n.tag_name().name() == "Contents") {
            let key = child_text(contents, "Key").unwrap_or_default();
            let size: u64 = child_text(contents, "Size").unwrap_or("0").parse().unwrap_or(0);
            if !key.is_empty() {
                out.push((key.to_string(), size));
                n += 1;
            }
        }
        log(format!(
            "listing page {page}: {n} objects (total {})",
            out.len()
        ));

        token = doc
            .descendants()
            .find(|n| n.tag_name().name() == "NextContinuationToken")
            .and_then(|n| n.text())
            .unwrap_or("")
            .to_string();
        if token.is_empty() {
            return Ok(out);
        }
    }
    bail!("too many listing pages")
}

/// Query the NGX OTA servers and build the list of downloadable offers.
/// Fetch and parse a namespace's `nvngx_server_config.txt`, returning the
/// version pins for the features we care about.
fn fetch_server_config_pins(agent: &ureq::Agent, ns: &str, log: &mut dyn FnMut(String)) -> Vec<ConfigPin> {
    let url = format!(
        "{NGX_HOST}/{ns}/{MODELS_MARKER}config/versions/2/files/nvngx_server_config.txt"
    );
    let Ok(resp) = agent.get(&url).call() else { return Vec::new() };
    let Ok(text) = resp.into_string() else { return Vec::new() };
    let pins = parse_server_config(&text);
    if !pins.is_empty() {
        log(format!("read server config from {ns} ({} pins)", pins.len()));
    }
    pins
}

fn parse_server_config(text: &str) -> Vec<ConfigPin> {
    let mut out = Vec::new();
    let mut section = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].to_ascii_lowercase();
            continue;
        }
        let Some(feature) = Feature::from_dir(&section) else { continue };
        let Some((k, v)) = line.split_once('=') else { continue };
        let Some(app) = k.trim().strip_prefix("app_") else { continue };
        out.push(ConfigPin {
            feature,
            app_id: app.trim().to_ascii_uppercase(),
            version: v.trim().to_string(),
        });
    }
    out
}

/// Pack a `major.minor.patch` string into an NGX version id.
fn pack_semver(s: &str) -> Option<u32> {
    let mut it = s.split('.');
    let major: u32 = it.next()?.trim().parse().ok()?;
    let minor: u32 = it.next().unwrap_or("0").trim().parse().ok()?;
    let patch: u32 = it.next().unwrap_or("0").trim().parse().ok()?;
    Some((major << 16) | (minor << 8) | patch)
}

/// HEAD a payload (+ its sha256 sidecar, if present) and return synthetic
/// listing entries when the object exists.
fn probe_entries(agent: &ureq::Agent, ns: &str, feature: &Feature, version_id: u32) -> Vec<RawEntry> {
    let payload = feature.canonical_payload();
    let base = format!(
        "{NGX_HOST}/{ns}/{MODELS_MARKER}{}/versions/{version_id}/files/{payload}",
        feature.dir()
    );
    let Some(size) = head_size(agent, &base) else { return Vec::new() };
    let mut out = vec![RawEntry {
        ns: ns.to_string(),
        feature: feature.clone(),
        version_id,
        payload: payload.clone(),
        is_sidecar: false,
        size,
    }];
    if head_size(agent, &format!("{base}.sha256")).is_some() {
        out.push(RawEntry {
            ns: ns.to_string(),
            feature: feature.clone(),
            version_id,
            payload,
            is_sidecar: true,
            size: 0,
        });
    }
    out
}

fn head_size(agent: &ureq::Agent, url: &str) -> Option<u64> {
    let resp = agent.head(url).call().ok()?;
    if resp.status() != 200 {
        return None;
    }
    let len = resp.header("content-length").and_then(|v| v.parse().ok());
    Some(len.unwrap_or(0))
}

pub fn fetch_offers(log: &mut dyn FnMut(String)) -> Result<Vec<Group>> {
    log(format!("querying {NGX_HOST} (S3 bucket listing)..."));
    let agent = build_agent();
    let entries = list_all(&agent, log)?;
    log(format!("scanning {} objects for DLSS / Streamline packages...", entries.len()));

    let mut raw: Vec<RawEntry> = Vec::new();
    let mut other_dirs: BTreeMap<String, usize> = BTreeMap::new();
    for (key, size) in &entries {
        match parse_key(key, *size) {
            Some(e) => raw.push(e),
            None => {
                // transparency: report what we deliberately skipped
                if key.ends_with('/') {
                    continue; // directory marker keys
                }
                if let Some(pos) = key.find(MODELS_MARKER) {
                    let dir = key[pos + MODELS_MARKER.len()..].split('/').next().unwrap_or("");
                    if !dir.is_empty() && dir != "config" {
                        *other_dirs.entry(dir.to_string()).or_insert(0) += 1;
                    }
                }
            }
        }
    }
    if raw.is_empty() {
        bail!("server answered but no DLSS/Streamline objects were found - the layout may have changed");
    }
    if !other_dirs.is_empty() {
        let names: Vec<String> = other_dirs
            .iter()
            .map(|(d, n)| format!("{d} ({n})"))
            .collect();
        log(format!(
            "ignoring unrelated NGX feature dirs: {}",
            names.join(", ")
        ));
    }

    // namespace discovery (every top-level prefix that carries model objects)
    let mut namespaces: Vec<String> = Vec::new();
    for e in &raw {
        if !namespaces.contains(&e.ns) {
            namespaces.push(e.ns.clone());
        }
    }
    log(format!("discovered namespaces: {}", namespaces.join(", ")));

    // ---- deep discovery pass 1: server-config version pins ---------------
    // The listing endpoint caps at 1000 keys and strips continuation/prefix
    // params, so the newest namespace can be cut off. NVIDIA's per-namespace
    // nvngx_server_config.txt still pins the current versions (e.g.
    // `app_E658700 = 310.9.0`) - HEAD-verify those objects directly.
    let mut known: BTreeMap<(Feature, u32), ()> = raw
        .iter()
        .filter(|e| !e.is_sidecar)
        .map(|e| ((e.feature.clone(), e.version_id), ()))
        .collect();
    for ns in &namespaces {
        for pin in fetch_server_config_pins(&agent, ns, log) {
            if pin.app_id != pin.feature.canonical_app_id() {
                continue;
            }
            let Some(id) = pack_semver(&pin.version) else { continue };
            let found = probe_entries(&agent, ns, &pin.feature, id);
            if !found.is_empty() {
                log(format!(
                    "config pin verified: {} {} on {}",
                    pin.feature.title(),
                    decode_version(id),
                    ns
                ));
            }
            for e in found {
                known.insert((e.feature.clone(), e.version_id), ());
                raw.push(e);
            }
        }
    }

    // ---- deep discovery pass 2: HEAD probes for staged builds ------------
    let probe_agent = build_probe_agent();
    let mut features: Vec<Feature> = Vec::new();
    for e in &raw {
        if !features.iter().any(|f| f.dir == e.feature.dir) {
            features.push(e.feature.clone());
        }
    }
    features.sort_by_key(|f| f.rank());
    for feature in &features {
        let Some(max_id) = raw
            .iter()
            .filter(|e| e.feature.dir == feature.dir)
            .map(|e| e.version_id)
            .max()
        else {
            continue;
        };
        let major = max_id >> 16;
        let minor = (max_id >> 8) & 0xFF;
        for minor in minor + 1..=(minor + 3).min(0xFF) {
            for build in [0u32, 128] {
                let id = (major << 16) | (minor << 8) | build;
                if known.contains_key(&(feature.clone(), id)) {
                    continue;
                }
                for ns in &namespaces {
                    let found = probe_entries(&probe_agent, ns, feature, id);
                    if !found.is_empty() {
                        log(format!(
                            "probe found staged {} {} on {}",
                            feature.title(),
                            decode_version(id),
                            ns
                        ));
                        for e in found {
                            known.insert((e.feature.clone(), e.version_id), ());
                            raw.push(e);
                        }
                    }
                }
            }
        }
    }

    // group by (feature, version, payload) -> ns -> (size, has sha256 sidecar?)
    let mut acc: BTreeMap<(Feature, u32, String), BTreeMap<String, (u64, bool)>> = BTreeMap::new();
    for e in raw {
        let slot = acc
            .entry((e.feature, e.version_id, e.payload))
            .or_default()
            .entry(e.ns)
            .or_insert((0, false));
        slot.0 = slot.0.max(e.size);
        slot.1 |= e.is_sidecar;
    }

    let mut per_feature: BTreeMap<Feature, Vec<Offer>> = BTreeMap::new();
    for ((feature, version_id, payload), per_ns) in acc {
        // prefer mirrors that publish a sha256 sidecar, then alphabetical
        let mut items: Vec<(String, u64, bool)> = per_ns
            .into_iter()
            .map(|(ns, (size, sidecar))| (ns, size, sidecar))
            .collect();
        items.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));

        let mut size: u64 = 0;
        let candidates = items
            .into_iter()
            .map(|(ns, entry_size, sidecar)| {
                size = size.max(entry_size);
                Candidate {
                    url: format!(
                        "{NGX_HOST}/{ns}/{MODELS_MARKER}{}/versions/{}/files/{}",
                        feature.dir(), version_id, payload
                    ),
                    sha256_url: sidecar.then(|| {
                        format!(
                            "{NGX_HOST}/{ns}/{MODELS_MARKER}{}/versions/{}/files/{}.sha256",
                            feature.dir(), version_id, payload
                        )
                    }),
                }
            })
            .collect();

        let offer = Offer {
            feature: feature.clone(),
            version_id,
            version: decode_version(version_id),
            payload,
            size,
            candidates,
        };
        per_feature.entry(feature).or_default().push(offer);
    }
    let mut groups: Vec<Group> = Vec::new();
    for (feature, offers) in per_feature {
        // Collapse legacy per-app-id duplicates of the same version: prefer the
        // canonical universal payload (E658700/E658703 on prefix 160), but keep
        // non-canonical ones if no canonical variant exists (future-proofing).
        let mut best: BTreeMap<u32, Offer> = BTreeMap::new();
        for offer in offers {
            match best.entry(offer.version_id) {
                std::collections::btree_map::Entry::Vacant(v) => {
                    v.insert(offer);
                }
                std::collections::btree_map::Entry::Occupied(mut o) => {
                    if payload_rank(&offer.payload, &feature)
                        > payload_rank(&o.get().payload, &feature)
                    {
                        o.insert(offer);
                    }
                }
            }
        }
        let mut offers: Vec<Offer> = best.into_values().collect();
        offers.sort_by(|a, b| b.version_id.cmp(&a.version_id));
        groups.push(Group { feature, offers });
    }
    groups.sort_by(|a, b| a.feature.rank().cmp(&b.feature.rank()));
    groups.retain(|g| !g.offers.is_empty());
    Ok(groups)
}

// ---------------------------------------------------------------------------
// Download
// ---------------------------------------------------------------------------

fn sha256_hex_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

fn fetch_sidecar(agent: &ureq::Agent, url: &str) -> Result<String> {
    let body = agent
        .get(url)
        .call()
        .map_err(|e| anyhow!("sha256 sidecar fetch failed: {e}"))?
        .into_string()
        .map_err(|e| anyhow!("sha256 sidecar read failed: {e}"))?;
    let hex: String = body
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .collect();
    if hex.len() != 64 {
        bail!("sha256 sidecar has unexpected format");
    }
    Ok(hex.to_lowercase())
}

fn http_get(agent: &ureq::Agent, url: &str) -> Result<ureq::Response> {
    match agent.get(url).call() {
        Ok(r) => Ok(r),
        Err(ureq::Error::Status(code, _)) => Err(anyhow!("HTTP {code}")),
        Err(e) => Err(anyhow!("{e}")),
    }
}

/// Try one mirror: stream payload to `<out_dir>/.part`, verify sha256 if available.
fn try_download(
    agent: &ureq::Agent,
    cand: &Candidate,
    size: u64,
    out_dir: &Path,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<PathBuf> {
    let part = out_dir.join(".part");
    let resp = http_get(agent, &cand.url)?;
    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(&part).with_context(|| format!("creating {}", part.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    let mut done: u64 = 0;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        hasher.update(&buf[..n]);
        done += n as u64;
        progress(done, size);
    }
    drop(file);

    if let Some(sidecar) = &cand.sha256_url {
        let expect = fetch_sidecar(agent, sidecar)?;
        let actual = sha256_hex_file(&part)?;
        if expect != actual {
            let _ = std::fs::remove_file(&part);
            bail!("sha256 mismatch (expected {expect}, got {actual})");
        }
    } else if size > 0 && done != size {
        let _ = std::fs::remove_file(&part);
        bail!("truncated download ({done}/{size} bytes, no sidecar to verify)");
    }
    Ok(part)
}

/// Download one offer into `out_dir`, renaming payloads to consumer names.
/// Returns the written file names (relative to `out_dir`).
pub fn download_offer(
    offer: &Offer,
    out_dir: &Path,
    progress: &mut dyn FnMut(u64, u64),
    log: &mut dyn FnMut(String),
) -> Result<Vec<String>> {
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("creating directory {}", out_dir.display()))?;
    let agent = build_agent();
    let mut last_err = None;
    for (i, cand) in offer.candidates.iter().enumerate() {
        if i > 0 {
            log("trying next mirror...".into());
        }
        match try_download(&agent, cand, offer.size, out_dir, progress) {
            Ok(part) => match finalize(offer, &part, out_dir, log) {
                Ok(names) => return Ok(names),
                Err(e) => {
                    let _ = std::fs::remove_file(&part);
                    last_err = Some(e);
                }
            },
            Err(e) => {
                let _ = std::fs::remove_file(out_dir.join(".part"));
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("no mirrors available")))
}

fn finalize(offer: &Offer, part: &Path, out_dir: &Path, log: &mut dyn FnMut(String)) -> Result<Vec<String>> {
    match offer.feature.consumer_name() {
        Some(dest_name) => {
            let dest = out_dir.join(&dest_name);
            std::fs::rename(part, &dest).with_context(|| format!("renaming to {dest_name}"))?;
            Ok(vec![dest_name])
        }
        None => {
            // Streamline: zip archive with sl.*.dll members (already consumer names).
            let bytes = std::fs::read(part)?;
            let mut arch = zip::ZipArchive::new(std::io::Cursor::new(bytes))
                .context("parsing Streamline zip archive")?;
            let mut written = Vec::new();
            for i in 0..arch.len() {
                let mut zf = arch.by_index(i).context("reading zip member")?;
                let name = zf.name().to_string();
                if name.ends_with('/') {
                    continue;
                }
                let fname = name
                    .rsplit(['/', '\\'])
                    .next()
                    .unwrap_or(&name)
                    .to_string();
                if !fname.to_ascii_lowercase().ends_with(".dll") {
                    continue;
                }
                let dest = out_dir.join(&fname);
                let mut out = std::fs::File::create(&dest)
                    .with_context(|| format!("creating {}", dest.display()))?;
                std::io::copy(&mut zf, &mut out)?;
                written.push(fname);
            }
            let _ = std::fs::remove_file(part);
            if written.is_empty() {
                bail!("no .dll members found in Streamline archive");
            }
            written.sort();
            log(format!("  archive contained: {}", written.join(", ")));
            Ok(written)
        }
    }
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_decode() {
        assert_eq!(decode_version(20317442), "310.5.2");
        assert_eq!(decode_version(20317696), "310.6.0");
        assert_eq!(decode_version(20318080), "310.7.128");
        assert_eq!(decode_version(133888), "2.11.0");
        assert_eq!(decode_version(134272), "2.12.128");
    }

    #[test]
    fn key_parsing() {
        let e = parse_key(
            "d6e9b45e-d4f6-4a84-a460-bf61decae3e8/org/nvidia/team/ngx/models/dlssd/versions/20317442/files/160_E658700.bin",
            59922544,
        )
        .unwrap();
        assert_eq!(e.feature, Feature::from_dir("dlssd").unwrap());
        assert_eq!(e.version_id, 20317442);
        assert_eq!(e.payload, "160_E658700.bin");
        assert!(!e.is_sidecar);
        assert_eq!(e.size, 59922544);

        // future arch prefixes must not be missed
        let b = parse_key(
            "x/org/nvidia/team/ngx/models/dlssg/versions/20319000/files/1B0_E658700.bin",
            1,
        )
        .unwrap();
        assert_eq!(b.payload, "1B0_E658700.bin");

        // wrong payload types / junk names are rejected
        assert!(parse_key(
            "x/org/nvidia/team/ngx/models/dlss/versions/1/files/160_E658700.zip",
            1
        )
        .is_none());
        assert!(parse_key(
            "x/org/nvidia/team/ngx/models/sl_sdk_0/versions/1/files/160_E658703_err.zip",
            1
        )
        .is_none());
        assert!(parse_key(
            "x/org/nvidia/team/ngx/models/dlss/versions/131529/files/nvngx_dlss-gaida.dll",
            1
        )
        .is_none());

        let s = parse_key(
            "3e933c08-ea30-45ae-93d1-5114edf9c3b9/org/nvidia/team/ngx/models/sl_sdk_0/versions/134272/files/160_E658703.zip.sha256",
            65,
        )
        .unwrap();
        assert_eq!(s.feature, Feature::from_dir("sl_sdk_0").unwrap());
        assert!(s.is_sidecar);

        assert!(parse_key(
            "x/org/nvidia/team/ngx/models/nvbcast/versions/1793/files/160_E658703.zip",
            1
        )
        .is_none());
        assert!(parse_key(
            "x/org/nvidia/team/ngx/models/dlss/versions/20318080/files/160_E658700.bin.sha256",
            1
        )
        .unwrap()
        .is_sidecar);
        assert!(parse_key("unrelated/key.txt", 1).is_none());
    }

    #[test]
    fn server_config_parsing() {
        let cfg = "[dlisp]\napp_E658703 = 310.0.0\n[dlss]\napp_865EFBC = 2.1.201\napp_E658700 = 310.9.0\n[dlssd]\napp_E658700 = 310.9.0\n[dlssg]\napp_E658700 = 310.9.0\n";
        let pins = parse_server_config(cfg);
        let e658700: Vec<_> = pins.iter().filter(|p| p.app_id == "E658700").collect();
        assert_eq!(e658700.len(), 3);
        assert!(e658700.iter().all(|p| p.version == "310.9.0"));
        assert!(pins.iter().any(|p| p.feature.dir == "dlssd"));
        assert!(pins.iter().any(|p| p.feature.dir == "dlssg"));
        assert!(pins.iter().any(|p| p.feature.dir == "dlss"));
    }

    #[test]
    fn feature_dir_generalization() {
        // Any future DLSS-family feature dir is accepted, and the consumer
        // name follows the nvngx_<dir>.dll convention automatically.
        let nr = Feature::from_dir("dlssnr").unwrap();
        assert_eq!(nr.consumer_name().as_deref(), Some("nvngx_dlssnr.dll"));
        assert_eq!(nr.canonical_payload(), "160_E658700.bin");
        assert_eq!(nr.title(), "dlssnr");
        assert_eq!(Feature::from_dir("dlss").unwrap().title(), "DLSS Super Resolution");
        // Non-feature dirs stay rejected.
        assert!(Feature::from_dir("dlss_override").is_none());
        assert!(Feature::from_dir("dlisp").is_none());
        assert!(Feature::from_dir("nvbcast").is_none());
        assert_eq!(Feature::from_dir("sl_sdk_0").unwrap().consumer_name(), None);
    }

    #[test]
    fn semver_packing() {
        assert_eq!(pack_semver("310.9.0"), Some(20318464));
        assert_eq!(pack_semver("310.7.128"), Some(20318080));
        assert_eq!(pack_semver("2.14.0"), Some(134656));
        assert_eq!(pack_semver("310.5"), Some(20317440));
        assert_eq!(pack_semver("x.y.z"), None);
    }

    #[test]
    fn human_size_fmt() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(74_208_880), "70.8 MB");
    }
}
