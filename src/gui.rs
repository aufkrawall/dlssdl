//! GUI application (egui on wgpu/D3D12).

use crate::ngx::{self, Feature, Group, Offer};
use egui::Color32;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Worker events
// ---------------------------------------------------------------------------

enum Ev {
    Log(String),
    CheckDone(Result<Vec<Group>, String>),
    Prog {
        done: u64,
        total: u64,
        label: String,
    },
    /// One offer finished (Ok = file list, Err = message).
    Item(Result<String, String>),
    /// Whole download job finished. Ok(path) if anything succeeded.
    Done(Result<PathBuf, String>),
}

// ---------------------------------------------------------------------------
// UI model
// ---------------------------------------------------------------------------

struct RowUi {
    offer: Offer,
    checked: bool,
}

impl RowUi {
    fn sidecar(&self) -> bool {
        self.offer.candidates.first().map_or(false, |c| c.sha256_url.is_some())
    }
    fn mirrors(&self) -> usize {
        self.offer.candidates.len().max(1)
    }
}

struct GroupUi {
    feature: Feature,
    rows: Vec<RowUi>,
}

impl GroupUi {
    fn latest_checked_mut(&mut self, checked: bool) {
        if let Some(row) = self.rows.first_mut() {
            row.checked = checked;
        }
    }
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

pub struct DlssApp {
    tx: Sender<Ev>,
    rx: Receiver<Ev>,

    checking: bool,
    downloading: bool,
    status: String,
    last_check: Option<String>,

    groups: Vec<GroupUi>,
    log: Vec<(Option<bool>, String)>, // (ok, text)

    base_dir: PathBuf,
    sub: String,
    auto_name: bool,

    prog_done: u64,
    prog_total: u64,
    prog_label: String,

    done_dir: Option<PathBuf>,
}

impl DlssApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let base_dir = dirs::download_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            tx,
            rx,
            checking: false,
            downloading: false,
            status: "Not checked yet. Click \"Check NGX servers\".".into(),
            last_check: None,
            groups: Vec::new(),
            log: Vec::new(),
            base_dir,
            sub: String::new(),
            auto_name: true,
            prog_done: 0,
            prog_total: 0,
            prog_label: String::new(),
            done_dir: None,
        }
    }

    fn push_log(&mut self, ok: Option<bool>, text: String) {
        self.log.push((ok, text));
        if self.log.len() > 500 {
            self.log.drain(..100);
        }
    }

    fn spawn_check(&mut self) {
        if self.checking || self.downloading {
            return;
        }
        self.checking = true;
        self.groups.clear();
        self.done_dir = None;
        self.status = "Checking NVIDIA NGX servers...".into();
        self.push_log(None, format!("Checking {} ...", ngx::NGX_HOST));
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let mut logger = |m: String| {
                let _ = tx.send(Ev::Log(m));
            };
            match ngx::fetch_offers(&mut logger) {
                Ok(groups) => {
                    let _ = tx.send(Ev::CheckDone(Ok(groups)));
                }
                Err(e) => {
                    let _ = tx.send(Ev::CheckDone(Err(format!("{e:#}"))));
                }
            }
        });
    }

    /// (selected count, total bytes)
    fn selection(&self) -> (usize, u64) {
        let mut n = 0usize;
        let mut bytes = 0u64;
        for g in &self.groups {
            for r in &g.rows {
                if r.checked {
                    n += 1;
                    bytes += r.offer.size;
                }
            }
        }
        (n, bytes)
    }

    fn auto_folder_name(&self) -> String {
        let mut best_dlss: Option<u32> = None;
        let mut best_sl: Option<u32> = None;
        for g in &self.groups {
            for r in &g.rows {
                if !r.checked {
                    continue;
                }
                if g.feature.is_streamline() {
                    best_sl = Some(best_sl.map_or(r.offer.version_id, |v| v.max(r.offer.version_id)));
                } else {
                    best_dlss = Some(best_dlss.map_or(r.offer.version_id, |v| v.max(r.offer.version_id)));
                }
            }
        }
        match best_dlss.or(best_sl) {
            Some(v) => ngx::decode_version(v),
            None => String::new(),
        }
    }

    fn effective_sub(&self) -> String {
        if self.auto_name {
            self.auto_folder_name()
        } else {
            self.sub.trim().trim_matches(['\\', '/']).to_string()
        }
    }

    fn spawn_download(&mut self, dest_root: PathBuf) {
        let mut jobs: Vec<Offer> = Vec::new();
        for g in &self.groups {
            for r in &g.rows {
                if r.checked {
                    jobs.push(r.offer.clone());
                }
            }
        }
        if jobs.is_empty() || self.downloading || self.checking {
            return;
        }
        let total: u64 = jobs.iter().map(|o| o.size).sum();
        self.downloading = true;
        self.prog_total = total;
        self.prog_done = 0;
        self.prog_label = String::new();
        self.done_dir = None;
        self.push_log(
            None,
            format!(
                "Downloading {} package(s) ({}) into {}",
                jobs.len(),
                ngx::human_size(total),
                dest_root.display()
            ),
        );

        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let mut overall = 0u64;
            let mut successes = 0usize;
            for offer in &jobs {
                let label = format!("{} {}", offer.feature.title(), offer.version);
                let _ = tx.send(Ev::Prog { done: overall, total, label: label.clone() });
                let mut logged = |m: String| {
                    let _ = tx.send(Ev::Log(m));
                };
                let base = overall;
                let mut progress = |done: u64, _t: u64| {
                    let _ = tx.send(Ev::Prog { done: base + done, total, label: label.clone() });
                };
                match ngx::download_offer(offer, &dest_root, &mut progress, &mut logged) {
                    Ok(files) => {
                        overall += offer.size;
                        successes += 1;
                        let _ = tx.send(Ev::Item(Ok(format!(
                            "✓ {} {}: {}",
                            label,
                            ngx::human_size(offer.size),
                            files.join(", ")
                        ))));
                    }
                    Err(e) => {
                        let _ = tx.send(Ev::Item(Err(format!("✗ {} failed: {e:#}", label))));
                    }
                }
            }
            if successes > 0 {
                let _ = tx.send(Ev::Done(Ok(dest_root)));
            } else {
                let _ = tx.send(Ev::Done(Err("all downloads failed".into())));
            }
        });
    }

    fn drain_events(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Ev::Log(m) => self.push_log(None, m),
                Ev::CheckDone(Ok(groups)) => {
                    self.checking = false;
                    self.last_check = Some(now_hms());
                    let mut total_offers = 0;
                    self.groups = groups
                        .into_iter()
                        .map(|g| {
                            let rows: Vec<RowUi> = g
                                .offers
                                .into_iter()
                                .enumerate()
                                .map(|(i, offer)| RowUi {
                                    checked: i == 0, // pre-select newest
                                    offer,
                                })
                                .collect();
                            total_offers += rows.len();
                            GroupUi { feature: g.feature, rows }
                        })
                        .collect();
                    self.status = format!(
                        "Server check ok: {} versions across {} features ({} newest pre-selected).",
                        total_offers,
                        self.groups.len(),
                        self.groups.len()
                    );
                    self.push_log(None, self.status.clone());
                }
                Ev::CheckDone(Err(e)) => {
                    self.checking = false;
                    self.last_check = Some(now_hms());
                    self.status = format!("Check failed: {e}");
                    self.push_log(Some(false), self.status.clone());
                }
                Ev::Prog { done, total, label } => {
                    self.prog_done = done;
                    self.prog_total = total;
                    self.prog_label = label;
                }
                Ev::Item(Ok(m)) => self.push_log(Some(true), m),
                Ev::Item(Err(m)) => self.push_log(Some(false), m),
                Ev::Done(Ok(path)) => {
                    self.downloading = false;
                    self.done_dir = Some(path.clone());
                    self.status = format!("Done. Files are in {}", path.display());
                    self.push_log(Some(true), self.status.clone());
                }
                Ev::Done(Err(e)) => {
                    self.downloading = false;
                    self.status = format!("Download failed: {e}");
                    self.push_log(Some(false), self.status.clone());
                }
            }
        }
    }
}

impl eframe::App for DlssApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_events();
        if self.checking || self.downloading {
            ui.ctx().request_repaint_after(Duration::from_millis(120));
        }

        // ---- top panel -----------------------------------------------------
        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("NGX / DLSS Update Fetcher");
                ui.separator();
                ui.hyperlink_to("ngx.download.nvidia.com", ngx::NGX_HOST);
                if let Some(t) = &self.last_check {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.weak(format!("last check: {t}"));
                    });
                }
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let busy = self.checking || self.downloading;
                if ui
                    .add_enabled(!busy, egui::Button::new(if self.checking { "Checking…" } else { "Check NGX servers" }))
                    .clicked()
                {
                    self.spawn_check();
                }
                if self.checking {
                    ui.spinner();
                }
                ui.weak(&self.status);
            });
            ui.add_space(4.0);
        });

        // ---- bottom panel (progress + log) ----------------------------------
        egui::Panel::bottom("bottom").show(ui, |ui| {
            ui.add_space(4.0);
            if self.downloading {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::ProgressBar::new(
                            (self.prog_done as f32 / self.prog_total.max(1) as f32).clamp(0.0, 1.0),
                        )
                        .desired_width(ui.available_width() - 260.0),
                    );
                    ui.weak(format!(
                        "{} / {} — {}",
                        ngx::human_size(self.prog_done),
                        ngx::human_size(self.prog_total),
                        self.prog_label
                    ));
                });
            }
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .max_height(140.0)
                .show(ui, |ui| {
                    for (ok, text) in &self.log {
                        let mut rt = egui::RichText::new(text).monospace().size(11.5);
                        match ok {
                            Some(true) => rt = rt.color(Color32::from_rgb(120, 220, 120)),
                            Some(false) => rt = rt.color(Color32::from_rgb(240, 130, 120)),
                            None => rt = rt.color(Color32::from_rgb(190, 190, 200)),
                        }
                        ui.label(rt);
                    }
                });
            ui.add_space(4.0);
        });

        // ---- central panel ---------------------------------------------------
        egui::CentralPanel::default().show(ui, |ui| {
            // output selection
            ui.horizontal(|ui| {
                ui.label("Output:");
                let sub = self.effective_sub();
                let target = if sub.is_empty() {
                    self.base_dir.clone()
                } else {
                    self.base_dir.join(&sub)
                };
                ui.monospace(target.display().to_string());
                if ui.button("Base dir…").clicked() {
                    if let Some(dir) = rfd::FileDialog::new()
                        .set_title("Choose base output folder")
                        .pick_folder()
                    {
                        self.base_dir = dir;
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.add_enabled(
                    !self.auto_name,
                    egui::TextEdit::singleline(&mut self.sub)
                        .hint_text("subfolder (version)")
                        .desired_width(140.0),
                );
                ui.checkbox(&mut self.auto_name, "name subfolder after version automatically");
            });
            ui.add_space(6.0);

            let (sel_n, sel_bytes) = self.selection();
            ui.horizontal(|ui| {
                let sub = self.effective_sub();
                let busy = self.checking || self.downloading;
                let can = !busy && sel_n > 0 && !sub.is_empty();
                let btn = if self.downloading {
                    egui::Button::new("Downloading…")
                } else {
                    egui::Button::new(format!("Download selected ({})", ngx::human_size(sel_bytes)))
                };
                let resp = ui.add_enabled(can, btn);
                if !can && !busy {
                    if sel_n == 0 {
                        resp.clone().on_disabled_hover_text("Nothing selected");
                    } else {
                        resp.clone().on_disabled_hover_text("Empty subfolder name");
                    }
                }
                if resp.clicked() {
                    let dest = self.base_dir.join(sub);
                    if self.auto_name {
                        self.sub = self.auto_folder_name();
                    }
                    self.spawn_download(dest);
                }
                if sel_n > 0 {
                    ui.weak(format!("{sel_n} package(s) selected"));
                }
                if self.downloading {
                    ui.spinner();
                }
            });
            ui.separator();

            if self.groups.is_empty() {
                ui.add_space(12.0);
                ui.vertical_centered(|ui| {
                    ui.weak(if self.checking {
                        "Fetching package list from NVIDIA NGX servers…"
                    } else {
                        "Press \"Check NGX servers\" to list available DLSS / Streamline packages."
                    });
                });
                return;
            }

            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                for g in &mut self.groups {
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            ui.strong(g.feature.title());
                            let hint = g.feature.consumer_name().unwrap_or_else(|| {
                                "sl.common.dll, sl.dlss.dll, sl.dlss_g.dll, sl.reflex.dll, …".to_string()
                            });
                            ui.weak(format!("→ {hint}"));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let any = g.rows.iter().any(|r| r.checked);
                                let label = if any { "clear" } else { "latest" };
                                if ui.small_button(label).clicked() {
                                    let latest = !any;
                                    g.latest_checked_mut(latest);
                                }
                            });
                        });
                        for r in &mut g.rows {
                            ui.horizontal(|ui| {
                                let text = egui::RichText::new(format!(
                                    "v{:>9}{:<14} {:>8}   {} mirror(s), sha256 {}",
                                    r.offer.version,
                                    r.offer.tag(),
                                    ngx::human_size(r.offer.size),
                                    r.mirrors(),
                                    if r.sidecar() { "✓" } else { "—" }
                                ))
                                .monospace();
                                if ui.checkbox(&mut r.checked, text).changed() {
                                    self.done_dir = None;
                                }
                            });
                        }
                    });
                    ui.add_space(2.0);
                }
            });

            if let Some(dir) = &self.done_dir {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button("Open output folder").clicked() {
                        let _ = open::that(dir);
                    }
                });
            }
        });
    }
}

fn now_hms() -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (h, rem) = (t / 3600 % 24, t % 3600);
    let (m, s) = (rem / 60, rem % 60);
    format!("{h:02}:{m:02}:{s:02} UTC")
}
