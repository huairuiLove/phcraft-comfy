//! ComfyUI Generation panel (Window › ComfyUI Generation): the bridge `ai.comfyui.fill` and
//! friends get a Photoshop-generative-fill-style workspace.
//!
//! Top to bottom: the server connection (its URL is the preference), the workflow picker (the
//! built-ins plus everything in the workflows folder, with an importer for ComfyUI saves), what
//! to send (composite / active layer / selection crop), the prompt, the run parameters, and a
//! serial task queue — one task runs at a time, exactly what a ComfyUI server does — whose
//! results land on the engine's result shelf, shown as a gallery where each image can become a
//! selection-masked layer, its own document, or a saved file.

use std::collections::HashMap;

use egui::{Color32, TextureHandle, Vec2};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::PhotocraftApp;

/// Where the panel keeps its options between launches (`prefs.dialogs`).
const OPTIONS_PREF: &str = "comfyPanel";

// ------------------------------------------------------------------ persisted options

/// The panel's options, kept between launches.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ComfyPanelState {
    pub prompt: String,
    pub negative: String,
    /// The selected workflow's name (empty: the first listed).
    pub workflow: String,
    /// canvas | layer | selection.
    pub source: String,
    /// none | layer | document.
    pub place: String,
    /// Empty or a number; -1/empty means random.
    pub seed: String,
    /// Empty: the workflow's own value.
    pub steps: String,
    /// How many seed variants one Add to Queue run enqueues (1..=8).
    pub batch: u32,
    /// The selected prompt template ("" none, "fill", "atmosphere").
    pub template: String,
    /// Template phrase label → checked (missing: the phrase's own default).
    pub template_on: std::collections::BTreeMap<String, bool>,
    /// Template choice label → the picked value's label ("": none).
    pub template_pick: std::collections::BTreeMap<String, String>,
}

impl Default for ComfyPanelState {
    fn default() -> Self {
        Self {
            prompt: String::new(),
            negative: String::new(),
            workflow: String::new(),
            source: "canvas".into(),
            place: "none".into(),
            seed: String::new(),
            steps: String::new(),
            batch: 1,
            template: String::new(),
            template_on: std::collections::BTreeMap::new(),
            template_pick: std::collections::BTreeMap::new(),
        }
    }
}

/// Saves the panel options with the preferences.
pub fn persist_options(app: &mut PhotocraftApp) {
    if let Ok(value) = serde_json::to_value(app.ui.comfy_panel.clone()) {
        app.session.prefs.edit(|p| {
            p.dialogs.insert(OPTIONS_PREF.into(), value);
        });
    }
}

/// Restores the saved panel options; unreadable ones keep the defaults.
pub fn restore_options(app: &mut PhotocraftApp) {
    if let Some(v) = app.session.prefs().dialogs.get(OPTIONS_PREF)
        && let Ok(state) = serde_json::from_value(v.clone())
    {
        app.ui.comfy_panel = state;
    }
}

// ------------------------------------------------------------------ queue (this session)

/// One queued generation.
#[derive(Clone, Debug, PartialEq)]
pub struct QueueTask {
    /// waiting | running | done | failed.
    pub state: String,
    pub prompt: String,
    pub workflow: String,
    pub source: String,
    pub place: String,
    /// The seed as submitted (empty: random; the real one shows once done).
    pub seed: String,
    pub error: String,
    /// Result-shelf indices this task produced.
    pub results: Vec<usize>,
    /// The shelf length when the task started (completion detection).
    pub results_before: usize,
    /// The seed the run actually used, once done.
    pub used_seed: Option<u64>,
    pub seconds: u64,
}

/// The panel's session state: the queue and its caches (never saved).
#[derive(Default)]
pub struct ComfyQueue {
    pub tasks: Vec<QueueTask>,
    /// The workflow list as last listed: (name, builtin, analyze json).
    pub workflows: Vec<(String, bool, Value)>,
    /// True once the first listing has happened.
    pub listed: bool,
    /// The connection check's one-line answer.
    pub check: Option<String>,
    /// Result thumbnails, keyed by (created_ms, seed).
    pub thumbs: HashMap<(u64, u64), TextureHandle>,
}

// ------------------------------------------------------------------ helpers

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The parsed seed the entry holds (None: random).
fn seed_of(raw: &str) -> Option<i64> {
    let t = raw.trim();
    if t.is_empty() || t == "-1" {
        return None;
    }
    t.parse::<i64>().ok().filter(|v| *v >= 0)
}

// ------------------------------------------------------------------ prompt templates

/// One line a template offers: a checkbox (empty `values`) or a pick-one choice whose picked
/// value carries its own English phrase. `text` is what lands in the prompt when on; a choice's
/// `{v}` is replaced by the picked value's text.
struct Phrase {
    label: &'static str,
    text: &'static str,
    values: &'static [(&'static str, &'static str)],
    default_on: bool,
}

const fn on(label: &'static str, text: &'static str) -> Phrase {
    Phrase { label, text, values: &[], default_on: true }
}
const fn off(label: &'static str, text: &'static str) -> Phrase {
    Phrase { label, text, values: &[], default_on: false }
}

/// 生图 · 扩图：lock the person, keep the background and tone, then lens, composition and the
/// render-style words, in the order they should read in the prompt.
const FILL_PHRASES: &[Phrase] = &[
    on("严格限制人物不变", "strictly keep the person unchanged"),
    on("严格限制人物位置不变", "strictly keep the person's position in the frame unchanged"),
    on("严格限制人物动作不变", "strictly keep the person's pose and action unchanged"),
    on("严格限制人物五官不变", "strictly keep the person's facial features unchanged"),
    on("保留背景", "keep the original background unchanged"),
    Phrase {
        label: "焦段",
        text: "{v}",
        values: &[
            ("24mm 超广角", "24mm ultra-wide angle lens"),
            ("35mm 广角", "35mm wide angle lens"),
            ("50mm 标准", "50mm standard lens"),
            ("85mm 人像", "85mm portrait lens"),
            ("135mm 长焦", "135mm telephoto lens"),
        ],
        default_on: false,
    },
    Phrase {
        label: "构图",
        text: "{v}",
        values: &[
            ("三分法", "rule of thirds composition"),
            ("居中对称", "centered symmetrical composition"),
            ("引导线", "leading lines composition"),
            ("框架构图", "natural framing composition"),
            ("对角线", "diagonal composition"),
            ("全身像", "full-body composition"),
            ("半身像", "half-body portrait composition"),
        ],
        default_on: false,
    },
    on("色调明暗与原图一致", "keep the color tone and brightness consistent with the original image"),
    off("Blender Cycles 渲染", "Blender Cycles render"),
    off("超现实 3D", "surreal hyperrealistic 3D"),
    off("光线追踪", "ray tracing"),
    off("次世代 PBR 材质", "next-generation PBR materials"),
    Phrase {
        label: "高清分辨率",
        text: "{v}",
        values: &[
            ("8K", "8K UHD, ultra high detail"),
            ("16K", "16K ultra high definition, extreme detail"),
        ],
        default_on: false,
    },
];

/// 大气透视：the person locks plus the volumetric depth set.
const ATMOSPHERE_PHRASES: &[Phrase] = &[
    on("严格限制人物不变", "strictly keep the person unchanged"),
    on("严格限制人物位置不变", "strictly keep the person's position in the frame unchanged"),
    on("严格限制人物动作不变", "strictly keep the person's pose and action unchanged"),
    on("严格限制人物五官不变", "strictly keep the person's facial features unchanged"),
    on("给场景增加体积雾", "add volumetric fog to the scene"),
    on("增强体积雾效果", "enhance the volumetric fog effect"),
    on("体现场景纵深", "emphasize the depth of the scene"),
    on("增加空气尘埃", "add floating dust particles in the air"),
    on("体现大气透视", "strong atmospheric perspective"),
];

/// The named templates the picker offers.
fn template_phrases(id: &str) -> Option<&'static [Phrase]> {
    match id {
        "fill" => Some(FILL_PHRASES),
        "atmosphere" => Some(ATMOSPHERE_PHRASES),
        _ => None,
    }
}

fn template_label(id: &str) -> &'static str {
    match id {
        "fill" => "生图 · 扩图",
        "atmosphere" => "大气透视",
        _ => "None",
    }
}

/// Whether a phrase is checked: the stored pick, else the phrase's default.
fn phrase_on(state: &ComfyPanelState, p: &Phrase) -> bool {
    *state.template_on.get(p.label).unwrap_or(&p.default_on)
}

/// The English text a phrase contributes right now ("" when off or a choice with no value).
fn phrase_text(state: &ComfyPanelState, p: &Phrase) -> String {
    if p.values.is_empty() {
        return if phrase_on(state, p) { p.text.to_string() } else { String::new() };
    }
    let picked = state.template_pick.get(p.label).map(String::as_str).unwrap_or("");
    p.values
        .iter()
        .find(|(l, _)| *l == picked)
        .map(|(_, t)| p.text.replace("{v}", t))
        .unwrap_or_default()
}

/// The full prompt this run sends: the user's own words first, then the template's phrases.
pub fn compose_prompt(state: &ComfyPanelState) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !state.prompt.trim().is_empty() {
        parts.push(state.prompt.trim().to_string());
    }
    if let Some(phrases) = template_phrases(&state.template) {
        for p in phrases {
            let t = phrase_text(state, p);
            if !t.is_empty() {
                parts.push(t);
            }
        }
    }
    parts.join(", ")
}

/// Enqueue `batch` tasks from the current options.
fn add_to_queue(app: &mut PhotocraftApp) {
    let p = app.ui.comfy_panel.clone();
    let batch = p.batch.clamp(1, 8);
    let fixed = seed_of(&p.seed);
    for i in 0..batch {
        // A fixed seed fans out to seed, seed+1, …; an empty one leaves the seed random.
        let seed = match fixed {
            Some(s) => (s + i as i64).to_string(),
            None => String::new(),
        };
        app.comfy_queue.tasks.push(QueueTask {
            state: "waiting".into(),
            prompt: compose_prompt(&p),
            workflow: p.workflow.clone(),
            source: p.source.clone(),
            place: p.place.clone(),
            seed,
            error: String::new(),
            results: Vec::new(),
            results_before: 0,
            used_seed: None,
            seconds: 0,
        });
    }
    persist_options(app);
}

/// Start the next waiting task when the engine is idle, and retire the running one when its
/// job ended (the shelf grew: done; otherwise: failed with the status line's error).
fn schedule(app: &mut PhotocraftApp) {
    let job_running = !app.session.jobs().is_empty();
    let shelf = app.session.comfy.results.len();
    if let Some(i) = app.comfy_queue.tasks.iter().position(|t| t.state == "running") {
        if !job_running {
            let status_error = app.ui.status_error;
            let status = app.ui.status.clone();
            let t = &mut app.comfy_queue.tasks[i];
            if shelf > t.results_before {
                t.state = "done".into();
                t.results = (t.results_before..shelf).collect();
                t.used_seed = app.session.comfy.results.last().map(|r| r.seed);
                t.seconds = app
                    .session
                    .comfy
                    .results
                    .last()
                    .map(|r| now_ms().saturating_sub(r.created_ms) / 1000)
                    .unwrap_or(0);
            } else {
                t.state = "failed".into();
                t.error = if status_error && !status.is_empty() { status } else { "generation did not produce an image".into() };
            }
        } else {
            return; // One task at a time, like the server.
        }
    }
    if job_running {
        return;
    }
    if let Some(i) = app.comfy_queue.tasks.iter().position(|t| t.state == "waiting") {
        let (prompt, workflow, source, place, seed, steps) = {
            let t = &app.comfy_queue.tasks[i];
            (t.prompt.clone(), t.workflow.clone(), t.source.clone(), t.place.clone(), t.seed.clone(), String::new())
        };
        let mut params = json!({"prompt": prompt, "workflow": workflow, "source": source, "place": place});
        if let Some(s) = seed_of(&seed) {
            params["seed"] = json!(s);
        }
        if let Ok(n) = steps.trim().parse::<u64>() {
            params["steps"] = json!(n);
        }
        let before = app.session.comfy.results.len();
        match app.run("ai.comfyui.fill", params) {
            Ok(_) => {
                let t = &mut app.comfy_queue.tasks[i];
                t.state = "running".into();
                t.results_before = before;
                t.error = String::new();
            }
            Err(e) => {
                let t = &mut app.comfy_queue.tasks[i];
                t.state = "failed".into();
                t.error = e;
            }
        }
    }
}

fn refresh_workflows(app: &mut PhotocraftApp) {
    let mut list = Vec::new();
    if let Ok(r) = app.run("ai.comfyui.workflows.list", json!({})) {
        for w in r.get("workflows").and_then(Value::as_array).into_iter().flatten() {
            let name = w.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
            if name.is_empty() {
                continue;
            }
            list.push((name, w.get("builtin").and_then(Value::as_bool).unwrap_or(false), w.get("analyze").cloned().unwrap_or(Value::Null)));
        }
    }
    let q = &mut app.comfy_queue;
    q.workflows = list;
    q.listed = true;
}

// ------------------------------------------------------------------ panel

/// The ComfyUI Generation panel (Window › ComfyUI Generation).
pub fn show(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    if !app.comfy_queue.listed {
        refresh_workflows(app);
    }
    egui::ScrollArea::vertical().show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 6.0;
        connection(app, ui);
        ui.separator();
        workflow(app, ui);
        ui.separator();
        source(app, ui);
        ui.separator();
        prompt(app, ui);
        params(app, ui);
        queue(app, ui);
        ui.separator();
        gallery(app, ui);
    });
    schedule(app);
}

// ---- connection

fn connection(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.strong("Server");
        let check = app.comfy_queue.check.clone().unwrap_or_else(|| "not checked".into());
        let bad = check.starts_with("✗");
        ui.colored_label(if bad { Color32::LIGHT_RED } else { Color32::LIGHT_GREEN }, if bad { "✗" } else { "✓" });
        ui.label(egui::RichText::new(check).small().weak());
    });
    let mut url = app.session.prefs().integrations.comfy_url.clone();
    ui.horizontal(|ui| {
        ui.label("URL");
        let r = egui::TextEdit::singleline(&mut url).desired_width(ui.available_width() - 70.0).show(ui);
        if r.response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            app.session.prefs.edit(|p| p.integrations.comfy_url = url.trim().trim_end_matches('/').to_string());
            app.comfy_queue.check = None;
        }
        if ui.button("Check").clicked() {
            match app.run("ai.comfyui.check", json!({})) {
                Ok(v) => {
                    let version = v.pointer("/server/system/comfyui_version").and_then(Value::as_str).unwrap_or("?").to_string();
                    app.comfy_queue.check = Some(format!("ComfyUI {version}"));
                }
                Err(e) => app.comfy_queue.check = Some(format!("✗ {e}")),
            }
        }
    });
}

// ---- workflow picker

fn workflow(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.strong("Workflow");
        let entries: Vec<(String, bool)> = app.comfy_queue.workflows.iter().map(|w| (w.0.clone(), w.1)).collect();
        let selected = if app.ui.comfy_panel.workflow.is_empty() { entries.first().map(|w| w.0.clone()).unwrap_or_default() } else { app.ui.comfy_panel.workflow.clone() };
        craft_ui::ComboBox::from_id_salt("comfy-workflow").selected_text(if selected.is_empty() { "(none — import one)".to_string() } else { selected.clone() }).show_ui(ui, |ui| {
            for (name, builtin) in &entries {
                let label = if *builtin { format!("{name} (built-in)") } else { name.clone() };
                ui.selectable_value(&mut app.ui.comfy_panel.workflow, name.clone(), label).clicked();
            }
        });
        if ui.button("Import…").clicked() {
            let _ = app.pick_file_bytes_filtered(&["json"], |app, name, _bytes| {
                let r = app.run("ai.comfyui.workflows.import", json!({"path": name})).map_err(|e| e.to_string())?;
                refresh_workflows(app);
                if let Some(n) = r.get("name").and_then(Value::as_str) {
                    app.ui.comfy_panel.workflow = n.to_string();
                    persist_options(app);
                }
                Ok(json!({"imported": r["name"]}))
            });
        }
        if ui.button("Refresh").clicked() {
            refresh_workflows(app);
        }
        let removable = !selected.is_empty() && !entries.iter().any(|(n, b)| *n == selected && !*b);
        if removable && ui.button("Remove").clicked() {
            if let Err(e) = app.run("ai.comfyui.workflows.remove", json!({"name": selected})) {
                app.ui.status = e;
                app.ui.status_error = true;
            }
            app.ui.comfy_panel.workflow.clear();
            refresh_workflows(app);
        }
    });
    // What generation would write into, from the analysis.
    let selected = app.ui.comfy_panel.workflow.clone();
    if let Some((_, _, analyze)) = app.comfy_queue.workflows.iter().find(|w| w.0 == selected) {
        let n = |k: &str| analyze.get(k).and_then(Value::as_array).map(Vec::len).unwrap_or(0);
        ui.label(
            egui::RichText::new(format!(
                "{} nodes · prompt → {} · image → {} · sampler {}",
                analyze.get("nodes").and_then(Value::as_u64).unwrap_or(0),
                n("promptNodes"),
                n("imageNodes"),
                n("samplerNodes")
            ))
            .small()
            .weak(),
        );
    }
}

// ---- source and scope

fn source(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    ui.strong("Send");
    ui.horizontal(|ui| {
        for (key, label) in [("canvas", "Composite"), ("layer", "Active layer"), ("selection", "Selection crop")] {
            if ui
                .selectable_value(&mut app.ui.comfy_panel.source, key.to_string(), label)
                .clicked()
            {
                persist_options(app);
            }
        }
    });
    // What each source would send right now.
    let d = app.session.active();
    let line = match (app.ui.comfy_panel.source.as_str(), d) {
        (_, None) => "no document open".to_string(),
        ("layer", Some(d)) => match d.active_layer.and_then(|id| d.doc.layer(id)) {
            Some(l) => format!("layer “{}”", l.name),
            None => "no active layer".to_string(),
        },
        ("selection", Some(d)) => match &d.doc.selection {
            Some(sel) => {
                let b = sel.content_bounds().intersect(&d.doc.bounds());
                format!("selection {}×{} at ({}, {})", b.width(), b.height(), b.x0, b.y0)
            }
            None => "no selection (make one, or send the composite)".to_string(),
        },
        (_, Some(d)) => format!("composite {}×{}", d.doc.size.width, d.doc.size.height),
    };
    ui.label(egui::RichText::new(line).small().weak());
}

// ---- prompts

fn prompt(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    let mut p = app.ui.comfy_panel.clone();
    let mut changed = false;
    // The template picker; "无" leaves just the user's own words.
    ui.horizontal(|ui| {
        ui.strong("Prompt");
        ui.label(egui::RichText::new("Template").small());
        craft_ui::ComboBox::from_id_salt("comfy-template").selected_text(template_label(&p.template)).show_ui(ui, |ui| {
            for id in ["", "fill", "atmosphere"] {
                if ui.selectable_label(p.template == id, template_label(id)).clicked() {
                    p.template = id.to_string();
                    changed = true;
                }
            }
        });
    });
    // The template's phrases: checkboxes and pick-one choices, two columns to stay compact.
    if let Some(phrases) = template_phrases(&p.template) {
        let per_col = phrases.len().div_ceil(2);
        ui.columns(2, |cols| {
            for (ci, col) in cols.iter_mut().enumerate() {
                for ph in phrases.iter().skip(ci * per_col).take(per_col) {
                    if ph.values.is_empty() {
                        let mut on = phrase_on(&p, ph);
                        if col.checkbox(&mut on, ph.label).changed() {
                            p.template_on.insert(ph.label.to_string(), on);
                            changed = true;
                        }
                    } else {
                        let old = p.template_pick.get(ph.label).cloned().unwrap_or_default();
                        let mut picked = old.clone();
                        col.horizontal(|col| {
                            col.label(egui::RichText::new(format!("{}:", ph.label)).small());
                            craft_ui::ComboBox::from_id_salt(format!("comfy-tpl-{}", ph.label))
                                .selected_text(if picked.is_empty() { "none".into() } else { picked.clone() })
                                .show_ui(col, |ui| {
                                    ui.selectable_value(&mut picked, String::new(), "none");
                                    for (vl, _) in ph.values {
                                        ui.selectable_value(&mut picked, vl.to_string(), *vl);
                                    }
                                });
                        });
                        if picked != old {
                            p.template_pick.insert(ph.label.to_string(), picked);
                            changed = true;
                        }
                    }
                }
            }
        });
    }
    // The user's own description, then the composed prompt exactly as it will be sent.
    ui.label(egui::RichText::new("Your description").small());
    let r = egui::TextEdit::multiline(&mut p.prompt).desired_rows(2).desired_width(f32::INFINITY).hint_text("what to change (English works best)").show(ui);
    if r.response.changed() {
        app.ui.comfy_panel.prompt = p.prompt.clone();
        persist_options(app);
    }
    ui.label(egui::RichText::new("Negative").small());
    let r = egui::TextEdit::multiline(&mut p.negative).desired_rows(1).desired_width(f32::INFINITY).show(ui);
    if r.response.changed() {
        app.ui.comfy_panel.negative = p.negative.clone();
        persist_options(app);
    }
    if changed {
        app.ui.comfy_panel.template = p.template.clone();
        app.ui.comfy_panel.template_on = p.template_on.clone();
        app.ui.comfy_panel.template_pick = p.template_pick.clone();
        persist_options(app);
    }
    let full = compose_prompt(&p);
    let used = template_phrases(&p.template).map(|ph| ph.iter().filter(|ph| !phrase_text(&p, ph).is_empty()).count()).unwrap_or(0);
    ui.label(egui::RichText::new(format!("Full prompt ({used} template phrases)")).small().strong());
    let mut show = full;
    ui.add(egui::TextEdit::multiline(&mut show).interactive(false).desired_rows(3).desired_width(f32::INFINITY).font(egui::TextStyle::Monospace));
}

// ---- parameters and enqueue

fn params(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    let mut p = app.ui.comfy_panel.clone();
    ui.horizontal(|ui| {
        ui.label("Seed");
        let r = egui::TextEdit::singleline(&mut p.seed).desired_width(70.0).hint_text("random").show(ui);
        if r.response.changed() {
            app.ui.comfy_panel.seed = p.seed.clone();
            persist_options(app);
        }
        if ui.button("🎲").clicked() {
            let s = (now_ms() % 900_000_000_000) as i64;
            app.ui.comfy_panel.seed = s.to_string();
            persist_options(app);
        }
        ui.separator();
        ui.label("Steps");
        let r = egui::TextEdit::singleline(&mut p.steps).desired_width(40.0).hint_text("default").show(ui);
        if r.response.changed() {
            app.ui.comfy_panel.steps = p.steps.clone();
            persist_options(app);
        }
        ui.separator();
        ui.label("Batch");
        let mut batch = p.batch as i32;
        let r = egui::DragValue::new(&mut batch).range(1..=8);
        if ui.add(r).changed() {
            app.ui.comfy_panel.batch = batch.clamp(1, 8) as u32;
            persist_options(app);
        }
    });
    ui.horizontal(|ui| {
        ui.label("Results");
        craft_ui::ComboBox::from_id_salt("comfy-place").selected_text(place_label(&p.place)).show_ui(ui, |ui| {
            for (key, label) in [("none", "Stay in the panel"), ("layer", "Place as layer (masked)"), ("document", "Open as documents")] {
                ui.selectable_value(&mut p.place, key.to_string(), label).clicked();
            }
        });
        if p.place != app.ui.comfy_panel.place {
            app.ui.comfy_panel.place = p.place.clone();
            persist_options(app);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let n = p.batch.clamp(1, 8);
            let label = if n > 1 { format!("Add {} to Queue", n) } else { "Add to Queue".to_string() };
            let enabled = !app.comfy_queue.workflows.is_empty() && app.session.active().is_some();
            if ui.add_enabled(enabled, egui::Button::new(label)).clicked() {
                if app.ui.comfy_panel.workflow.is_empty() {
                    app.ui.comfy_panel.workflow = app.comfy_queue.workflows.first().map(|w| w.0.clone()).unwrap_or_default();
                }
                add_to_queue(app);
            }
        });
    });
}

fn place_label(place: &str) -> String {
    match place {
        "layer" => "Place as layer (masked)".to_string(),
        "document" => "Open as documents".to_string(),
        _ => "Stay in the panel".to_string(),
    }
}

// ---- queue

fn queue(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    if app.comfy_queue.tasks.is_empty() {
        return;
    }
    ui.strong("Queue");
    let mut clear_done = false;
    let mut retry = None;
    let mut remove = None;
    for (i, t) in app.comfy_queue.tasks.iter().enumerate() {
        ui.horizontal(|ui| {
            let (mark, color) = match t.state.as_str() {
                "running" => ("▶", Color32::LIGHT_YELLOW),
                "done" => ("✓", Color32::LIGHT_GREEN),
                "failed" => ("✗", Color32::LIGHT_RED),
                _ => ("…", Color32::GRAY),
            };
            ui.colored_label(color, mark);
            let seed = t.used_seed.map(|s| s.to_string()).unwrap_or_else(|| if t.seed.is_empty() { "random".into() } else { t.seed.clone() });
            let short: String = t.prompt.chars().take(48).collect();
            let mut line = format!("{} · {} · seed {}", short, t.workflow, seed);
            if t.state == "done" && t.seconds > 0 {
                line.push_str(&format!(" · {}s", t.seconds));
            }
            ui.label(egui::RichText::new(line).small());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("✕").clicked() {
                    remove = Some(i);
                }
                if t.state == "failed" && ui.small_button("Retry").clicked() {
                    retry = Some(i);
                }
            });
        });
        if t.state == "failed" && !t.error.is_empty() {
            ui.label(egui::RichText::new(t.error.as_str()).small().color(Color32::LIGHT_RED));
        }
    }
    ui.horizontal(|ui| {
        if ui.small_button("Clear Finished").clicked() {
            clear_done = true;
        }
        if ui.small_button("Stop All").clicked() {
            app.comfy_queue.tasks.clear();
        }
    });
    if clear_done {
        app.comfy_queue.tasks.retain(|t| t.state == "waiting" || t.state == "running");
    }
    if let Some(i) = remove {
        app.comfy_queue.tasks.remove(i);
    }
    if let Some(t) = retry.and_then(|i| app.comfy_queue.tasks.get_mut(i)) {
        t.state = "waiting".into();
        t.error = String::new();
        t.results.clear();
    }
}

// ---- result gallery

fn gallery(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    let shelf: Vec<(usize, u64, u64, String)> = app
        .session
        .comfy
        .results
        .iter()
        .enumerate()
        .map(|(i, r)| (i, r.created_ms, r.seed, r.workflow.clone()))
        .collect();
    if shelf.is_empty() {
        return;
    }
    ui.strong("Results");
    // Newest first, the newest eight.
    for (i, created, seed, workflow) in shelf.iter().rev().take(8) {
        let key = (*created, *seed);
        let app_ = &mut *app;
        let ctx = ui.ctx().clone();
        let tex = thumb(app_, &ctx, key, *i);
        ui.horizontal(|ui| {
            let size = 64.0;
            if let Some(tex) = &tex {
                let (w, h) = (tex.size_vec2().x.max(1.0), tex.size_vec2().y.max(1.0));
                let k = size / w.max(h);
                ui.image((tex.id(), Vec2::new(w * k, h * k)));
            }
            ui.vertical(|ui| {
                ui.label(egui::RichText::new(format!("seed {seed}")).small().weak());
                ui.label(egui::RichText::new(workflow).small().weak());
                ui.horizontal(|ui| {
                    if ui.small_button("As Layer").clicked()
                        && let Err(e) = app_.run("ai.comfyui.place", json!({"result": i, "as": "layer"}))
                    {
                        app_.ui.status = e;
                        app_.ui.status_error = true;
                    }
                    if ui.small_button("New Doc").clicked()
                        && let Err(e) = app_.run("ai.comfyui.place", json!({"result": i, "as": "document"}))
                    {
                        app_.ui.status = e;
                        app_.ui.status_error = true;
                    }
                    if ui.small_button("Save…").clicked() {
                        let idx = *i;
                        let _ = app_.pick_save(&format!("comfyui-{seed}.png"), move |app, path| {
                            app.run("ai.comfyui.save", json!({"result": idx, "path": path})).map_err(|e| e.to_string())
                        });
                    }
                });
            });
        });
    }
}

/// The shelf entry's thumbnail, decoded once and cached by (created, seed).
fn thumb(app: &mut PhotocraftApp, ctx: &egui::Context, key: (u64, u64), index: usize) -> Option<TextureHandle> {
    if let Some(t) = app.comfy_queue.thumbs.get(&key) {
        return Some(t.clone());
    }
    let bytes = app.session.comfy.results.get(index)?.bytes.clone();
    let img = decode_png(&bytes)?;
    let tex = ctx.load_texture(format!("comfy-{}-{}", key.0, key.1), img, egui::TextureOptions::LINEAR);
    app.comfy_queue.thumbs.insert(key, tex.clone());
    // Keep the cache near the shelf's size.
    while app.comfy_queue.thumbs.len() > 40 {
        let oldest = *app.comfy_queue.thumbs.keys().next()?;
        app.comfy_queue.thumbs.remove(&oldest);
    }
    Some(tex)
}

/// A generated PNG as an egui image (the same conversion the canvas display uses).
fn decode_png(bytes: &[u8]) -> Option<egui::ColorImage> {
    let imported = photocraft_io::import("result.png", bytes).ok()?;
    let doc = imported.document;
    let buf = photocraft_compose::render(&doc, doc.bounds());
    let img = buf.to_rgba8();
    let size = [img.width as usize, img.height as usize];
    let src: Vec<u8> = img.pixels;
    (size[0] > 0 && size[1] > 0 && src.len() == size[0] * size[1] * 4).then(|| egui::ColorImage::from_rgba_unmultiplied(size, &src))
}

// ------------------------------------------------------------------ template tests

#[cfg(test)]
mod tests {
    use super::*;

    fn state(template: &str, prompt: &str) -> ComfyPanelState {
        ComfyPanelState { template: template.into(), prompt: prompt.into(), ..Default::default() }
    }

    #[test]
    fn fill_template_defaults_read_person_background_tone() {
        let s = state("fill", "add a red rose to her hair");
        let p = compose_prompt(&s);
        assert!(p.starts_with("add a red rose to her hair"), "{p}");
        for needle in [
            "strictly keep the person unchanged",
            "position in the frame unchanged",
            "pose and action unchanged",
            "facial features unchanged",
            "keep the original background unchanged",
            "color tone and brightness consistent",
        ] {
            assert!(p.contains(needle), "missing `{needle}` in {p}");
        }
        // The off-by-default style words and the unpicked choices contribute nothing.
        assert!(!p.contains("ray tracing") && !p.contains("Cycles") && !p.contains("8K") && !p.contains("lens"), "{p}");
    }

    #[test]
    fn choices_and_toggles_change_the_composition() {
        let mut s = state("fill", "rebuild the scene");
        s.template_on.insert("保留背景".into(), false);
        s.template_pick.insert("焦段".into(), "85mm 人像".into());
        s.template_pick.insert("构图".into(), "三分法".into());
        s.template_pick.insert("高清分辨率".into(), "16K".into());
        s.template_on.insert("光线追踪".into(), true);
        s.template_on.insert("Blender Cycles 渲染".into(), true);
        let p = compose_prompt(&s);
        assert!(p.contains("rebuild the scene"));
        assert!(p.contains("85mm portrait lens") && p.contains("rule of thirds composition"), "{p}");
        assert!(p.contains("16K ultra high definition") && p.contains("ray tracing") && p.contains("Blender Cycles render"), "{p}");
        assert!(!p.contains("keep the original background"), "{p}");
        // A choice cleared back to 无 stops contributing.
        s.template_pick.insert("焦段".into(), String::new());
        assert!(!compose_prompt(&s).contains("lens"));
    }

    #[test]
    fn atmosphere_template_is_the_full_fog_set() {
        let p = compose_prompt(&state("atmosphere", ""));
        for needle in ["volumetric fog to the scene", "enhance the volumetric fog effect", "depth of the scene", "floating dust particles", "atmospheric perspective", "strictly keep the person unchanged"] {
            assert!(p.contains(needle), "missing `{needle}` in {p}");
        }
        // One lock turned off is honoured.
        let mut s = state("atmosphere", "");
        s.template_on.insert("增加空气尘埃".into(), false);
        assert!(!compose_prompt(&s).contains("dust"));
    }

    #[test]
    fn no_template_is_the_users_words_alone() {
        assert_eq!(compose_prompt(&state("", "just this")), "just this");
        assert_eq!(compose_prompt(&state("fill", "")), compose_prompt(&state("fill", ""))); // stable
        assert!(!compose_prompt(&state("", "just this")).contains("strictly"));
    }

    #[test]
    fn old_panel_state_without_template_fields_loads() {
        let old = serde_json::json!({"prompt": "p", "batch": 4});
        let s: ComfyPanelState = serde_json::from_value(old).unwrap();
        assert_eq!((s.prompt.as_str(), s.batch, s.template.as_str()), ("p", 4, ""));
        // And a full state round-trips.
        let mut s = state("atmosphere", "foggy");
        s.template_on.insert("体现大气透视".into(), false);
        s.template_pick.insert("焦段".into(), "50mm 标准".into());
        let back: ComfyPanelState = serde_json::from_value(serde_json::to_value(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }
}
