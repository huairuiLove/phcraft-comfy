//! The ComfyUI workflow store and selection-aware generation: `ai.comfyui.fill`,
//! `ai.comfyui.place` / `.save` and `ai.comfyui.workflows.*`.
//!
//! [`comfy_cmds`] runs built-in pipelines on a local ComfyUI server. This module extends it the
//! way a Photoshop generative-fill plug-in works: pick what to send (the composite, the selected
//! layer, or the selection's crop), queue it through a managed workflow (imported from ComfyUI's
//! own UI-format saves and converted to the API format here — or an Export (API) file), then keep
//! the generated images on a result shelf where each can become a layer — masked to the
//! selection, so any workflow does local edits — or be saved to a file.
//!
//! The generation/`place` split is deliberate: a queue of tasks runs one after another (ComfyUI
//! itself is serial), and the user decides per image, after seeing it, whether it lands.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use photocraft_algo::resample::{Resample, resize_surface, translate_surface};
use photocraft_doc::{Document, Layer, LayerContent, LayerId, LayerMask};
use photocraft_geom::Size;
use photocraft_raster::Surface;
use serde_json::{Map, Value, json};

use super::comfy_cmds::{self, ComfyCall, ComfyFn, ComfyRequest};
use super::params::bad;
use crate::commands::CommandSpec;
use crate::jobs::{self, JobCtx};
use crate::{EngineError, Result, Session};

/// Prompt/negative caps and store limits, mirroring `comfy_cmds`.
const PROMPT_MAX: usize = 8_000;
const NAME_MAX: usize = 200;
/// One workflow file (UI-format saves carry canvas metadata; twice the API cap).
const FILE_MAX_BYTES: usize = 2_000_000;
/// Results kept on the shelf (the panel's gallery) and their total size.
const RESULT_LIMIT: usize = 32;
const RESULT_BYTES_MAX: u64 = 512 * 1024 * 1024;

// ------------------------------------------------------------------ result shelf

/// One generated image kept after `ai.comfyui.fill`: its bytes plus what to reproduce and where
/// it came from. `selection` is the snapshot taken when it was generated, so placing it as a
/// layer is right even if the user has re-selected since. `file` is where the bytes persist in
/// the generations folder (`None` when there is none, or in the web build).
pub struct StoredResult {
    pub bytes: Vec<u8>,
    pub seed: u64,
    pub prompt: String,
    pub workflow: String,
    pub selection: Option<Surface>,
    pub created_ms: u64,
    pub file: Option<PathBuf>,
}

impl super::comfy_cmds::Comfy {
    /// Reload the persisted shelf at startup: newest first under the same bounds as the live
    /// shelf ([`RESULT_LIMIT`] entries, [`RESULT_BYTES_MAX`] bytes); older files stay on disk for
    /// when the newer ones are removed. The selection snapshot is not persisted — a restored
    /// result places without a mask.
    pub fn restore_results(&mut self) {
        let Some(dir) = self.generations_dir.clone() else { return };
        let entries = read_index(&dir);
        // Walk newest first, stop at the shelf bounds, then insert oldest first so the shelf
        // order matches the order they were generated in.
        let mut picked: Vec<&Value> = Vec::new();
        let mut total = 0u64;
        for e in entries.iter().rev() {
            let Some(name) = e.get("file").and_then(Value::as_str).filter(|n| safe_generation_name(n)) else { continue };
            let Ok(meta) = std::fs::metadata(dir.join(name)) else { continue };
            if picked.len() >= RESULT_LIMIT || total + meta.len() > RESULT_BYTES_MAX {
                break;
            }
            total += meta.len();
            picked.push(e);
        }
        for e in picked.into_iter().rev() {
            let Some(name) = e.get("file").and_then(Value::as_str).filter(|n| safe_generation_name(n)) else { continue };
            let path = dir.join(name);
            let Ok(bytes) = photocraft_format::read_file(&path) else { continue };
            self.results.push(StoredResult {
                bytes,
                seed: e.get("seed").and_then(Value::as_u64).unwrap_or(0),
                prompt: e.get("prompt").and_then(Value::as_str).unwrap_or("").to_string(),
                workflow: e.get("workflow").and_then(Value::as_str).unwrap_or("").to_string(),
                selection: None,
                created_ms: e.get("createdMs").and_then(Value::as_u64).unwrap_or(0),
                file: Some(path),
            });
        }
    }
}

/// A persisted PNG's name: `<created_ms>-<seed>.png`, `-N` before the extension on collision.
/// Timestamped, so the folder sorts oldest first like the shelf does.
fn persist_result(dir: &std::path::Path, r: &StoredResult) -> Option<PathBuf> {
    if std::fs::create_dir_all(dir).is_err() {
        return None;
    }
    let mut path = dir.join(format!("{}-{}.png", r.created_ms, r.seed));
    let mut n = 1;
    while path.exists() {
        path = dir.join(format!("{}-{}-{}.png", r.created_ms, r.seed, n));
        n += 1;
    }
    photocraft_format::atomic_write(&path, &r.bytes).ok()?;
    Some(path)
}

/// Rewrite `index.json` from the shelf: one entry per result, in shelf order. The PNGs and the
/// index together mirror the shelf exactly (evictions and removals delete their file).
fn write_index(comfy: &super::comfy_cmds::Comfy) {
    let Some(dir) = &comfy.generations_dir else { return };
    let entries: Vec<Value> = comfy
        .results
        .iter()
        .map(|r| {
            json!({
                "file": r.file.as_ref().and_then(|p| p.file_name()).and_then(|n| n.to_str()),
                "seed": r.seed,
                "prompt": r.prompt,
                "workflow": r.workflow,
                "createdMs": r.created_ms,
            })
        })
        .collect();
    let Ok(text) = serde_json::to_vec_pretty(&entries) else { return };
    let _ = photocraft_format::atomic_write(&dir.join("index.json"), &text);
}

/// `index.json` as written by [`write_index`]; an unreadable or malformed file restores nothing.
fn read_index(dir: &std::path::Path) -> Vec<Value> {
    let Ok(bytes) = photocraft_format::read_file(&dir.join("index.json")) else { return Vec::new() };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

/// A persisted name we will restore: digits, dashes and `.png` only, so an edited index cannot
/// point outside the generations folder.
fn safe_generation_name(name: &str) -> bool {
    name.len() < 100
        && name.ends_with(".png")
        && name[..name.len() - 4].bytes().all(|b| b.is_ascii_digit() || b == b'-')
        && name[..name.len() - 4].contains('-')
}

fn remove_file_quiet(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
}

fn push_result(s: &mut Session, mut r: StoredResult) -> usize {
    r.file = s.comfy.generations_dir.as_deref().and_then(|dir| persist_result(dir, &r));
    let results = &mut s.comfy.results;
    results.push(r);
    // Bound the shelf: oldest first out (their persisted file goes too), then never over the
    // byte budget.
    while results.len() > RESULT_LIMIT {
        remove_file_quiet(results.remove(0).file.as_deref().unwrap_or(Path::new("")));
    }
    while results.len() > 1 && results.iter().map(|r| r.bytes.len() as u64).sum::<u64>() > RESULT_BYTES_MAX {
        remove_file_quiet(results.remove(0).file.as_deref().unwrap_or(Path::new("")));
    }
    let last = results.len() - 1;
    write_index(&s.comfy);
    last
}

// ------------------------------------------------------------------ UI-format conversion

/// One `/object_info` input of a class: its name, definition (`[type, options]`) and whether it
/// is required.
struct InputSpec<'a> {
    name: String,
    def: &'a Value,
    required: bool,
}

/// The ordered inputs of `class_type` out of `/object_info`: `input_order` when the server gives
/// it, else the required/optional key order. `None`: the server has no such node.
fn input_specs<'a>(object_info: &'a Value, class_type: &str) -> Option<Vec<InputSpec<'a>>> {
    let node = object_info.get(class_type)?;
    let input = node.get("input")?;
    let mut out = Vec::new();
    for (group, required) in [("required", true), ("optional", false)] {
        let group_obj = input.get(group).and_then(Value::as_object);
        let ordered = node.pointer(&format!("/input_order/{group}")).and_then(Value::as_array);
        match (group_obj, ordered) {
            (Some(g), Some(order)) => {
                for name in order.iter().filter_map(Value::as_str) {
                    if let Some(def) = g.get(name) {
                        out.push(InputSpec { name: name.to_string(), def, required });
                    }
                }
            }
            // No input_order (older servers): JSON objects keep insertion order.
            (Some(g), None) => {
                for (name, def) in g {
                    out.push(InputSpec { name: name.clone(), def, required });
                }
            }
            (None, _) => {}
        }
    }
    Some(out)
}

/// Whether an input definition is widget-capable (holds a value) rather than connection-only
/// (IMAGE, MODEL, …): a combo list, or one of the scalar widget types. A socketless input
/// (`{"socketless": true}`, e.g. ComfyUI 0.44's `IMAGECOMPARE` compare view) has no socket, so it
/// can only ever hold a widget value whatever its type name is.
fn widget_capable(def: &Value) -> bool {
    let Some(a) = def.as_array() else { return false };
    if socketless(def) {
        return true;
    }
    match a.first() {
        Some(Value::Array(_)) => true,
        Some(Value::String(t)) => {
            matches!(t.as_str(), "INT" | "FLOAT" | "STRING" | "STRING_MULTILINE" | "BOOLEAN" | "COMBO" | "COMFY_DYNAMICCOMBO_V3")
        }
        _ => false,
    }
}

/// A frontend-only widget (`socketless: true`): the node's execute ignores it, a UI save
/// serialises no value for it, but the API format still requires the key.
fn socketless(def: &Value) -> bool {
    def.as_array()
        .and_then(|a| a.get(1))
        .and_then(Value::as_object)
        .is_some_and(|o| o.get("socketless").and_then(Value::as_bool) == Some(true))
}

/// Whether the input's widget carries a trailing control (`seed` + "randomize"): such a widget
/// serializes two values into `widgets_values`.
fn has_control_after(def: &Value) -> bool {
    def.as_array()
        .and_then(|a| a.get(1))
        .and_then(Value::as_object)
        .is_some_and(|o| o.get("control_after_generate").and_then(Value::as_bool) == Some(true))
}

fn is_dynamic_combo(def: &Value) -> bool {
    def.as_array().and_then(|a| a.first()).and_then(Value::as_str) == Some("COMFY_DYNAMICCOMBO_V3")
}

/// The sub-inputs a dynamic combo's chosen value reveals (`"resolution": "resize"` →
/// `resolution.megapixels`), from the option table the definition carries.
fn dynamic_combo_children(def: &Value, chosen: &Value) -> Vec<String> {
    let Some(opts) = def.as_array().and_then(|a| a.get(1)).and_then(|o| o.get("options")).and_then(Value::as_array) else {
        return Vec::new();
    };
    for o in opts {
        if o.get("key") == Some(chosen)
            && let Some(req) = o.pointer("/inputs/required").and_then(Value::as_object)
        {
            return req.keys().cloned().collect();
        }
    }
    Vec::new()
}

/// An input definition's `options.default`, when it has one.
fn default_of(def: &Value) -> Option<Value> {
    def.as_array().and_then(|a| a.get(1)).and_then(|o| o.get("default")).cloned()
}

fn is_note(class: &str) -> bool {
    class == "Note" || class == "MarkdownNote"
}

fn is_primitive(class: &str) -> bool {
    class == "PrimitiveNode" || class.starts_with("Primitive")
}

/// `widgets_values` as a flat queue of values (nodes without widgets: empty).
struct WidgetQueue<'a> {
    vals: Vec<&'a Value>,
    next: usize,
}

impl<'a> WidgetQueue<'a> {
    fn of(node: &'a Value) -> Self {
        Self { vals: node.get("widgets_values").and_then(Value::as_array).into_iter().flatten().collect(), next: 0 }
    }

    fn take(&mut self) -> Option<&'a Value> {
        let v = self.vals.get(self.next).copied();
        if v.is_some() {
            self.next += 1;
        }
        v
    }
}

/// Where a link resolves to once UI-only nodes are gone: a real node's output slot, or the
/// constant a Primitive widget feeds in.
enum Resolved {
    Node(String, usize),
    Const(Value),
}

/// Convert a ComfyUI UI-format save (Workflow › Save: `nodes`/`links` with canvas metadata) into
/// the API format Workflow › Export (API) produces (`node id → {class_type, inputs}`), using the
/// server's own `/object_info` to line `widgets_values` up with named inputs.
///
/// Notes are dropped, Reroutes are followed through, Primitive* widgets become constants in the
/// inputs they feed, and the dotted dynamic names (`images.image0`, `resolution.megapixels`)
/// ComfyUI's edit nodes use are kept verbatim. Anything that cannot be lined up errors naming
/// the node, so the fallback (export API format from ComfyUI) is actionable.
pub fn convert_ui_workflow(ui: &Value, object_info: &Value) -> std::result::Result<Value, String> {
    let nodes = ui
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| "not a ComfyUI UI-format workflow (no `nodes` array); use Workflow › Save from ComfyUI".to_string())?;
    // link id → (from node, from slot)
    let mut links: HashMap<i64, (i64, usize)> = HashMap::new();
    for l in ui.get("links").and_then(Value::as_array).into_iter().flatten() {
        if let Some(a) = l.as_array()
            && a.len() >= 3
            && let (Some(id), Some(from), Some(slot)) = (a[0].as_i64(), a[1].as_i64(), a[2].as_u64())
            && (slot as usize) <= 63
        {
            links.insert(id, (from, slot as usize));
        }
    }
    let type_of = |id: i64| -> Option<&str> {
        nodes.iter().find(|n| n.get("id").and_then(Value::as_i64) == Some(id)).and_then(|n| n.get("type")).and_then(Value::as_str)
    };
    // A Primitive's single widget value, injected into every input it feeds.
    let mut primitives: HashMap<i64, Value> = HashMap::new();
    for n in nodes {
        let class = n.get("type").and_then(Value::as_str).unwrap_or("");
        if is_primitive(class)
            && let Some(id) = n.get("id").and_then(Value::as_i64)
        {
            primitives.insert(id, WidgetQueue::of(n).vals.first().copied().cloned().unwrap_or(Value::Null));
        }
    }
    // Follow a link back to a real source, through Reroutes and Primitive constants.
    let resolve = |link_id: i64| -> std::result::Result<Resolved, String> {
        let mut link = link_id;
        for _ in 0..64 {
            let (from, slot) = links.get(&link).copied().ok_or_else(|| "the graph has a dangling link".to_string())?;
            let class = type_of(from).ok_or_else(|| "the graph links to a missing node".to_string())?;
            if is_primitive(class) {
                return Ok(Resolved::Const(primitives.get(&from).cloned().unwrap_or(Value::Null)));
            }
            if class == "Reroute" {
                // A Reroute has one input; follow it upward.
                let inner = nodes
                    .iter()
                    .find(|n| n.get("id").and_then(Value::as_i64) == Some(from))
                    .and_then(|n| n.get("inputs"))
                    .and_then(Value::as_array)
                    .and_then(|inputs| inputs.first())
                    .and_then(|i| i.get("link"))
                    .and_then(Value::as_i64)
                    .ok_or_else(|| "a Reroute with nothing connected".to_string())?;
                link = inner;
                continue;
            }
            return Ok(Resolved::Node(from.to_string(), slot));
        }
        Err("the graph has a Reroute loop".to_string())
    };

    let mut out = Map::new();
    for node in nodes {
        let class = node.get("type").and_then(Value::as_str).unwrap_or("");
        if is_note(class) || is_primitive(class) || class == "Reroute" {
            continue;
        }
        let id = node.get("id").and_then(Value::as_i64).ok_or_else(|| "a node without an id".to_string())?;
        let specs = input_specs(object_info, class)
            .ok_or_else(|| format!("node {id} (`{class}`): the ComfyUI server has no such node; install its custom nodes, or export the workflow as API format"))?;
        // The node's connected inputs: name → link id.
        let mut connected: HashMap<String, i64> = HashMap::new();
        for i in node.get("inputs").and_then(Value::as_array).into_iter().flatten() {
            if let (Some(name), Some(link)) = (i.get("name").and_then(Value::as_str), i.get("link").and_then(Value::as_i64)) {
                connected.insert(name.to_string(), link);
            }
        }
        let mut wq = WidgetQueue::of(node);
        let mut inputs = Map::new();
        for spec in &specs {
            if let Some(link) = connected.get(&spec.name) {
                match resolve(*link)? {
                    Resolved::Node(from, slot) => {
                        inputs.insert(spec.name.clone(), json!([from, slot]));
                    }
                    Resolved::Const(v) => {
                        inputs.insert(spec.name.clone(), v);
                    }
                }
                // A widget converted to an input still keeps its slot in `widgets_values`.
                if widget_capable(spec.def) {
                    wq.take();
                    if has_control_after(spec.def) {
                        wq.take();
                    }
                }
                continue;
            }
            if is_dynamic_combo(spec.def) {
                let chosen = wq.take().cloned().or_else(|| default_of(spec.def)).ok_or_else(|| {
                    format!("node {id} (`{class}`): no value for `{}`", spec.name)
                })?;
                inputs.insert(spec.name.clone(), chosen.clone());
                for child in dynamic_combo_children(spec.def, &chosen) {
                    if let Some(v) = wq.take().cloned() {
                        inputs.insert(format!("{}.{}", spec.name, child), v);
                    } else if let Some(d) = default_of(spec.def) {
                        inputs.insert(format!("{}.{}", spec.name, child), d);
                    }
                }
                continue;
            }
            if widget_capable(spec.def) {
                let v = wq.take().cloned().or_else(|| default_of(spec.def));
                match v {
                    Some(v) => {
                        inputs.insert(spec.name.clone(), v);
                    }
                    // The server ignores socketless widgets but rejects a missing required key;
                    // null is the value it accepts (verified against ComfyUI 0.44's /prompt).
                    None if socketless(spec.def) => {
                        inputs.insert(spec.name.clone(), Value::Null);
                    }
                    None if spec.required => {
                        return Err(format!("node {id} (`{class}`): input `{}` has no value; connect it or set it in ComfyUI", spec.name));
                    }
                    None => {}
                }
                if has_control_after(spec.def) {
                    wq.take();
                }
                continue;
            }
            // A connection-only input left unconnected.
            if spec.required {
                return Err(format!("node {id} (`{class}`): required input `{}` is not connected", spec.name));
            }
        }
        // Dotted sub-inputs the node grew on the canvas (`images.image0`): API format keeps the
        // dotted name as-is, so pass the connection through under the same name.
        for (name, link) in &connected {
            if specs.iter().any(|s| &s.name == name) {
                continue;
            }
            match resolve(*link)? {
                Resolved::Node(from, slot) => {
                    inputs.insert(name.clone(), json!([from, slot]));
                }
                Resolved::Const(v) => {
                    inputs.insert(name.clone(), v);
                }
            }
        }
        out.insert(id.to_string(), json!({"class_type": class, "inputs": Value::Object(inputs)}));
    }
    if out.is_empty() {
        return Err("the workflow has no convertible nodes".to_string());
    }
    Ok(Value::Object(out))
}

// ------------------------------------------------------------------ parameter injection

/// The text-encode nodes behind a conditioning source, through wrapper nodes: a wrapper's
/// `positive`/`negative` inputs stay on their own side (`ReferenceMultiLatent` hides its encodes
/// one hop away), unnamed branches (switches) are followed on both.
fn encodes_on_side(obj: &Map<String, Value>, id: &str, side_negative: bool, depth: u32, out: &mut Vec<String>) {
    if depth == 0 {
        return;
    }
    let Some(node) = obj.get(id) else { return };
    let ct = node.get("class_type").and_then(Value::as_str).unwrap_or("");
    if ct.contains("TextEncode") {
        if !out.iter().any(|e| e == id) {
            out.push(id.to_string());
        }
        return;
    }
    let Some(inputs) = node.get("inputs").and_then(Value::as_object) else { return };
    for (name, v) in inputs {
        if side_negative && name == "positive" {
            continue;
        }
        if !side_negative && name == "negative" {
            continue;
        }
        let Some(from) = v.as_array().and_then(|a| a.first()).and_then(Value::as_str) else { continue };
        encodes_on_side(obj, from, side_negative, depth - 1, out);
    }
}

/// The encodes feeding a sampler's negative (and positive) side, side-aware.
fn sides_of_samplers(obj: &Map<String, Value>) -> (Vec<String>, Vec<String>) {
    let mut negative = Vec::new();
    let mut positive = Vec::new();
    for n in obj.values() {
        if !n.get("class_type").and_then(Value::as_str).unwrap_or("").starts_with("KSampler") {
            continue;
        }
        for (key, side) in [("negative", true), ("positive", false)] {
            let Some(src) = n.pointer(&format!("/inputs/{key}")).and_then(Value::as_array).and_then(|a| a.first()).and_then(Value::as_str) else { continue };
            encodes_on_side(obj, src, side, 8, if side { &mut negative } else { &mut positive });
        }
    }
    // An encode on both sides is treated as negative (the conservative fill).
    positive.retain(|id| !negative.iter().any(|n| n == id));
    negative.sort();
    positive.sort();
    negative.dedup();
    positive.dedup();
    (negative, positive)
}

/// The nodes holding a string prompt input for an API-style image node (OpenAI-compatible
/// `prompt`, the Chinese `提示词`), smallest id first: the fallback [`inject`] writes into when
/// the graph has no text encodes, and [`analyze`] lists for the panel.
fn prompt_input_nodes(obj: &Map<String, Value>) -> Vec<String> {
    let mut hits: Vec<String> = obj
        .iter()
        .filter(|(_, n)| {
            n.get("inputs")
                .and_then(Value::as_object)
                .is_some_and(|i| i.iter().any(|(k, v)| (k == "prompt" || k == "提示词") && v.is_string()))
        })
        .map(|(id, _)| id.clone())
        .collect();
    hits.sort_by_key(|id| id.parse::<u64>().unwrap_or(u64::MAX));
    hits
}


/// What [`inject`] touched, echoed back so the panel can show it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Injected {
    pub prompt_nodes: Vec<String>,
    pub negative_nodes: Vec<String>,
    pub image_nodes: Vec<String>,
    pub sampler_nodes: Vec<String>,
}

impl Injected {
    fn to_json(&self) -> Value {
        json!({
            "promptNodes": self.prompt_nodes, "negativeNodes": self.negative_nodes,
            "imageNodes": self.image_nodes, "samplerNodes": self.sampler_nodes,
        })
    }
}

/// Fill a converted or exported API-format workflow with this run's values: the prompt into the
/// text encodes (the ones feeding a sampler's negative side get the negative), the seed and
/// optional steps into every sampler, and the uploaded image name into the first LoadImage
/// (further LoadImages keep their own references — multi-reference workflows). A graph with no
/// text encodes (API-style image nodes) falls back to the first `prompt`/`提示词` string input;
/// there an empty box leaves the workflow's own value in place.
pub fn inject(workflow: &mut Value, prompt: &str, negative: &str, image: Option<&str>, seed: u64, steps: Option<u32>) -> Injected {
    let mut injected = Injected::default();
    let Some(obj) = workflow.as_object_mut() else { return injected };
    for (id, n) in obj.iter() {
        if n.get("class_type").and_then(Value::as_str).unwrap_or("").starts_with("KSampler") {
            injected.sampler_nodes.push(id.clone());
        }
    }
    let (negative_ids, positive_ids) = sides_of_samplers(obj);
    for (id, n) in obj.iter_mut() {
        let ct = n.get("class_type").and_then(Value::as_str).unwrap_or("");
        if !ct.contains("TextEncode") {
            continue;
        }
        let negative_side = negative_ids.iter().any(|n| n == id);
        let positive_side = positive_ids.iter().any(|n| n == id);
        if !negative_side && !positive_side {
            continue;
        }
        if let Some(inputs) = n.get_mut("inputs").and_then(Value::as_object_mut)
            && inputs.contains_key("text")
        {
            inputs.insert("text".into(), json!(if negative_side { negative } else { prompt }));
        }
        if negative_side {
            injected.negative_nodes.push(id.clone());
        } else {
            injected.prompt_nodes.push(id.clone());
        }
    }
    // The fallback for graphs with no text encodes: an API-style image node's own `prompt` /
    // `提示词` input (a string — a wired one is a link and stays untouched) on the smallest-id
    // node holding it. An empty prompt or negative leaves the workflow's baked-in value, so
    // importing a ready-made graph and running it as-is still works.
    if injected.prompt_nodes.is_empty()
        && !prompt.trim().is_empty()
        && let Some(id) = prompt_input_nodes(obj).into_iter().next()
        && let Some(n) = obj.get_mut(&id)
        && let Some(inputs) = n.get_mut("inputs").and_then(Value::as_object_mut)
    {
        for (k, v) in inputs.iter_mut() {
            if v.is_string() {
                if k == "prompt" || k == "提示词" {
                    *v = json!(prompt);
                    injected.prompt_nodes.push(id.clone());
                } else if !negative.trim().is_empty() && (k == "negative_prompt" || k == "负面提示词") {
                    *v = json!(negative);
                    injected.negative_nodes.push(id.clone());
                }
            }
        }
    }
    for id in &injected.sampler_nodes {
        let Some(n) = obj.get_mut(id) else { continue };
        let Some(inputs) = n.get_mut("inputs").and_then(Value::as_object_mut) else { continue };
        for key in ["seed", "noise_seed"] {
            if inputs.contains_key(key) {
                inputs.insert(key.into(), json!(seed));
            }
        }
        if let Some(steps) = steps
            && inputs.contains_key("steps")
        {
            inputs.insert("steps".into(), json!(steps));
        }
    }
    if let Some(image) = image {
        for (id, n) in obj.iter_mut() {
            let ct = n.get("class_type").and_then(Value::as_str).unwrap_or("");
            if ct == "LoadImage"
                && let Some(inputs) = n.get_mut("inputs").and_then(Value::as_object_mut)
                && inputs.contains_key("image")
            {
                inputs.insert("image".into(), json!(image));
                injected.image_nodes.push(id.clone());
                break;
            }
        }
    }
    injected
}

/// The workflow facts the panel lists: node count, and the nodes generation would write into.
pub fn analyze(workflow: &Value) -> Value {
    let mut prompt_nodes = Vec::new();
    let mut image_nodes = Vec::new();
    let mut sampler_nodes = Vec::new();
    if let Some(obj) = workflow.as_object() {
        for (id, n) in obj {
            let ct = n.get("class_type").and_then(Value::as_str).unwrap_or("");
            if ct.starts_with("KSampler") {
                sampler_nodes.push(id.clone());
            } else if ct == "LoadImage" {
                image_nodes.push(id.clone());
            }
        }
        let (_, positive_ids) = sides_of_samplers(obj);
        for id in positive_ids {
            if obj.get(&id).and_then(|n| n.pointer("/inputs/text")).is_some() {
                prompt_nodes.push(id);
            }
        }
        // API-style graphs without text encodes: their `prompt`/`提示词` input is the prompt
        // target (the panel's "prompt → N" must count it).
        if prompt_nodes.is_empty() {
            prompt_nodes = prompt_input_nodes(obj);
        }
    }
    let obj = workflow.as_object().map(|o| o.len()).unwrap_or(0);
    json!({"nodes": obj, "promptNodes": prompt_nodes, "imageNodes": image_nodes, "samplerNodes": sampler_nodes, "api": is_api_workflow(workflow)})
}

// ------------------------------------------------------------------ workflow store

const BUILTIN_FLUX3: &str = "FLUX.3 Klein Image Edit";

/// The fixed OpenAI-compatible Image API workflow (the iPad app): its single node names no
/// ComfyUI class at all — [`fill`] routes it to one direct `ComfyCall::ApiImage` call instead of
/// queueing a graph. The `prompt` input is what the panel's prompt box targets (the API-style
/// fallback in [`inject`]); `negative_prompt` documents what the call has no counterpart for.
pub const BUILTIN_IMAGE_API: &str = "Image API Edit";

/// The "Image API Edit" graph, shaped like an API-format workflow so it flows through the store
/// (`valid_workflow`, `analyze`) like any other.
fn builtin_api_workflow() -> Value {
    json!({"1": {"class_type": "PhotocraftImageApi", "inputs": {"prompt": "", "negative_prompt": ""}}})
}

/// A workflow whose generation is one direct Image API call, not a ComfyUI graph a server runs.
pub fn is_api_workflow(workflow: &Value) -> bool {
    workflow.as_object().is_some_and(|o| {
        o.values().any(|n| n.get("class_type").and_then(Value::as_str) == Some("PhotocraftImageApi"))
    })
}

/// The workflows compiled into the app, importable with zero setup.
fn builtin_workflows() -> Vec<(&'static str, Value)> {
    let v: Value = serde_json::from_str(include_str!("comfy_flux3_klein.json")).unwrap_or(Value::Null);
    if comfy_cmds::valid_workflow(&v) {
        vec![(BUILTIN_FLUX3, v)]
    } else {
        Vec::new()
    }
}

/// A workflow name safe to use as a file stem (no separators, no traversal, bounded).
fn safe_name(name: &str) -> Option<String> {
    let n = name.trim();
    if n.is_empty() || n.len() > NAME_MAX || n == "." || n == ".." || n.starts_with('.') {
        return None;
    }
    if n.contains(['/', '\\', '\0', ':']) {
        return None;
    }
    Some(n.to_string())
}

/// Where imported workflows live: the preference's folder, else the injected config-dir default.
fn workflows_dir(s: &Session) -> Result<PathBuf> {
    let pref = s.prefs().integrations.comfy_workflows_dir.trim().to_string();
    if !pref.is_empty() {
        return Ok(PathBuf::from(pref));
    }
    s.comfy.workflows_dir.clone().ok_or_else(|| {
        EngineError::Other("no workflow folder: set Preferences ▸ Integrations ▸ ComfyUI Workflows Folder (or use the desktop app)".into())
    })
}

/// Load a workflow by name: built-ins first, then the store folder.
fn load_workflow(s: &Session, name: &str) -> Result<Value> {
    for (n, v) in builtin_workflows() {
        if n == name {
            return Ok(v);
        }
    }
    if s.comfy.image_api && name == BUILTIN_IMAGE_API {
        return Ok(builtin_api_workflow());
    }
    let safe = safe_name(name).ok_or_else(|| bad("ai.comfyui.fill", format!("`workflow` `{name}` is not a valid workflow name")))?;
    let path = workflows_dir(s)?.join(format!("{safe}.json"));
    let bytes = read_store_file(&path)?;
    let v: Value = serde_json::from_slice(&bytes)
        .map_err(|e| EngineError::Other(format!("{safe}: not a JSON workflow ({e})")))?;
    if !comfy_cmds::valid_workflow(&v) {
        return Err(EngineError::Other(format!("{safe}: not a ComfyUI API-format graph; re-import it (Export (API) from ComfyUI)")));
    }
    Ok(v)
}

#[cfg(not(target_arch = "wasm32"))]
fn read_store_file(path: &std::path::Path) -> Result<Vec<u8>> {
    photocraft_format::read_file(path).map_err(|e| EngineError::Other(format!("{}: {e}", path.display())))
}

#[cfg(target_arch = "wasm32")]
fn read_store_file(_path: &std::path::Path) -> Result<Vec<u8>> {
    Err(EngineError::Other("workflow files need the desktop app".into()))
}

fn list_workflows(s: &mut Session, p: &Value) -> Result<Value> {
    if p.as_object().is_some_and(|o| !o.is_empty()) {
        return Err(bad("ai.comfyui.workflows.list", "pass nothing"));
    }
    let mut out = Vec::new();
    for (name, v) in builtin_workflows() {
        out.push(json!({"name": name, "builtin": true, "path": Value::Null, "analyze": analyze(&v)}));
    }
    if s.comfy.image_api {
        let v = builtin_api_workflow();
        out.push(json!({"name": BUILTIN_IMAGE_API, "builtin": true, "path": Value::Null, "analyze": analyze(&v)}));
    }
    if let Ok(dir) = workflows_dir(s)
        && let Ok(entries) = std::fs::read_dir(&dir)
    {
        let mut files: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("json")))
            .collect();
        files.sort();
        for path in files {
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
            let Some(name) = safe_name(&stem) else { continue };
            let Ok(bytes) = read_store_file(&path) else { continue };
            let Ok(v) = serde_json::from_slice::<Value>(&bytes) else { continue };
            if !comfy_cmds::valid_workflow(&v) {
                continue;
            }
            out.push(json!({"name": name, "builtin": false, "path": path.display().to_string(), "analyze": analyze(&v)}));
        }
    }
    Ok(json!({"workflows": out}))
}

fn import_workflow(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "ai.comfyui.workflows.import";
    let path = p.get("path").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).ok_or_else(|| bad(CMD, "`path` must be the workflow file to import"))?;
    if path.len() > 4_000 {
        return Err(bad(CMD, "`path` is too long"));
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let path = PathBuf::from(path);
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("workflow").to_string();
        let name = safe_name(&stem).ok_or_else(|| bad(CMD, format!("`{stem}` is not a usable workflow name")))?;
        let bytes = photocraft_format::read_file(&path).map_err(|e| EngineError::Other(format!("{}: {e}", path.display())))?;
        if bytes.len() > FILE_MAX_BYTES {
            return Err(EngineError::Other(format!("{} is {} KiB, over the {} KiB limit", path.display(), bytes.len() / 1024, FILE_MAX_BYTES / 1024)));
        }
        let v: Value = serde_json::from_slice(&bytes).map_err(|e| EngineError::Other(format!("{}: not JSON ({e})", path.display())))?;
        let was_ui = v.get("nodes").and_then(Value::as_array).is_some();
        // UI format (Workflow › Save) needs the server's node definitions to convert; API
        // format (Workflow › Export (API)) is taken as-is.
        let api = if was_ui {
            let transport = transport_fn(s)?;
            let url = comfy_cmds::url_of(s, p)?;
            let oi = comfy_cmds::json_of(
                transport(&ComfyRequest { url, call: ComfyCall::ObjectInfo }).map_err(|e| comfy_cmds::at("reading ComfyUI's node definitions", e))?,
            )?;
            Value::Object(
                convert_ui_workflow(&v, &oi)
                    .map_err(|e| EngineError::Other(format!("{}: {e}", path.display())))?
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            )
        } else if comfy_cmds::valid_workflow(&v) {
            v
        } else {
            return Err(EngineError::Other(format!("{}: neither a ComfyUI UI-format save (no `nodes`) nor an API-format graph", path.display())));
        };
        let dir = workflows_dir(s)?;
        std::fs::create_dir_all(&dir).map_err(|e| EngineError::Other(format!("{}: {e}", dir.display())))?;
        let out_path = dir.join(format!("{name}.json"));
        let text = serde_json::to_vec_pretty(&api).map_err(|e| EngineError::Other(format!("encoding the workflow: {e}")))?;
        photocraft_format::atomic_write(&out_path, &text).map_err(|e| EngineError::Other(format!("{}: {e}", out_path.display())))?;
        let mut r = json!({"name": name, "path": out_path.display().to_string(), "converted": was_ui});
        if let Some(o) = r.as_object_mut() {
            o.insert("analyze".into(), analyze(&api));
        }
        Ok(r)
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = path;
        Err(EngineError::Other("importing workflows needs the desktop app".into()))
    }
}

fn remove_workflow(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "ai.comfyui.workflows.remove";
    let name = p.get("name").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).ok_or_else(|| bad(CMD, "`name` must be the workflow to remove"))?;
    let safe = safe_name(name).ok_or_else(|| bad(CMD, format!("`{name}` is not a valid workflow name")))?;
    if builtin_workflows().iter().any(|(n, _)| *n == safe) || safe == BUILTIN_IMAGE_API {
        return Err(bad(CMD, format!("`{safe}` is built in and cannot be removed")));
    }
    let path = workflows_dir(s)?.join(format!("{safe}.json"));
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::fs::remove_file(&path).map_err(|e| EngineError::Other(format!("{}: {e}", path.display())))?;
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = path;
        return Err(EngineError::Other("removing workflows needs the desktop app".into()));
    }
    Ok(json!({"removed": safe}))
}

// ------------------------------------------------------------------ fill

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FillSource {
    /// The flattened composite of the whole document.
    Canvas,
    /// The active layer on its own.
    Layer,
    /// The selection's bounding-box crop of the composite.
    Selection,
}

impl FillSource {
    fn parse(v: Option<&str>) -> Result<Self> {
        match v {
            None | Some("canvas") => Ok(FillSource::Canvas),
            Some("layer") => Ok(FillSource::Layer),
            Some("selection") => Ok(FillSource::Selection),
            Some(other) => Err(bad("ai.comfyui.fill", format!("`source` must be \"canvas\", \"layer\" or \"selection\", not \"{other}\""))),
        }
    }

}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FillPlace {
    /// Keep the images on the result shelf only (the panel decides per image).
    None,
    /// Place each as a layer above the active one, masked to the selection.
    Layer,
    /// Place each at its native resolution: no cover scaling — a result larger than the canvas
    /// grows the canvas (centred) to fit, a smaller one is centred as-is.
    Native,
    /// Open each as its own document.
    Document,
}

impl FillPlace {
    fn parse(v: Option<&str>) -> Result<Self> {
        match v {
            None | Some("none") => Ok(FillPlace::None),
            Some("layer") => Ok(FillPlace::Layer),
            Some("native") => Ok(FillPlace::Native),
            Some("document") => Ok(FillPlace::Document),
            Some(other) => Err(bad("ai.comfyui.fill", format!("`place` must be \"none\", \"layer\", \"native\" or \"document\", not \"{other}\""))),
        }
    }
}

/// A bounded text param (`prompt`/`negative`), like comfy_cmds' own field parsing.
fn text_field(p: &Value, key: &str, cmd: &str) -> Result<String> {
    let v = comfy_cmds::text(p, key, cmd)?.to_string();
    if v.len() > PROMPT_MAX {
        return Err(bad(cmd, format!("`{key}` is longer than {PROMPT_MAX} characters")));
    }
    Ok(v)
}

struct FillParams {
    prompt: String,
    negative: String,
    workflow: String,
    source: FillSource,
    seed: Option<u64>,
    steps: Option<u32>,
    place: FillPlace,
    timeout_s: u64,
}

fn fill_params(p: &Value) -> Result<FillParams> {
    const CMD: &str = "ai.comfyui.fill";
    let prompt = text_field(p, "prompt", CMD)?;
    let negative = text_field(p, "negative", CMD)?;
    let workflow = p.get("workflow").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).ok_or_else(|| bad(CMD, "`workflow` must name a workflow (ai.comfyui.workflows.list)"))?.to_string();
    if workflow.len() > NAME_MAX {
        return Err(bad(CMD, format!("`workflow` is longer than {NAME_MAX} characters")));
    }
    let seed = match p.get("seed").and_then(Value::as_f64).or_else(|| p.get("seed").and_then(Value::as_str).and_then(|s| s.trim().parse::<f64>().ok())) {
        None => None,
        Some(s) if s < 0.0 => None,
        Some(s) => Some(s.round() as u64),
    };
    Ok(FillParams {
        prompt,
        negative,
        workflow,
        source: FillSource::parse(p.get("source").and_then(Value::as_str))?,
        seed,
        steps: p.get("steps").and_then(Value::as_f64).map(|v| v.clamp(1.0, 150.0) as u32),
        place: FillPlace::parse(p.get("place").and_then(Value::as_str))?,
        timeout_s: p.get("timeout").and_then(Value::as_f64).map(|v| v.clamp(10.0, 3600.0) as u64).unwrap_or(600),
    })
}

/// The upload cap: image-edit endpoints — and the relays in front of them — time out on
/// tens-of-megapixel uploads (a 45 MP composite came back HTTP 524 after two minutes). A source
/// at 3072 px on the long edge (~9 MP at 1:1) stays inside that budget while keeping enough
/// fidelity for a 2K-generation to place at its native size; [`place_result_layer`] scales
/// whatever comes back to cover the canvas again, and [`place_result_native`] keeps its pixels.
const UPLOAD_MAX_EDGE: u32 = 3072;

/// `doc` flattened to PNG for upload, downscaled on the long edge when it exceeds
/// [`UPLOAD_MAX_EDGE`]: the bytes plus the pixel size actually sent.
pub(crate) fn upload_png(doc: &Document, name: &str) -> Result<(Vec<u8>, u32, u32)> {
    let export = |d: &Document| {
        photocraft_io::export(d, name, &photocraft_io::ExportOptions::default())
            .map(|r| r.bytes)
            .map_err(|e| EngineError::Other(format!("encoding the source image: {e}")))
    };
    let long = doc.size.width.max(doc.size.height);
    let k = f64::from(UPLOAD_MAX_EDGE) / f64::from(long.max(1));
    if k >= 1.0 {
        return export(doc).map(|bytes| (bytes, doc.size.width, doc.size.height));
    }
    let px = photocraft_compose::composite_to_surface(doc, doc.bounds(), doc.pixel_format(), None);
    // `content_bounds` (what `resize` scales too), not `tile_bounds`: tiles are 256-aligned and
    // would pad the sent image with blank rows. A fully blank composite has no content to scale.
    if px.content_bounds().is_empty() {
        return export(doc).map(|bytes| (bytes, doc.size.width, doc.size.height));
    }
    let px = resize_surface(&px, k, k, Resample::Bicubic);
    let b = px.content_bounds();
    let mut d = Document::new("upload", Size::new(b.width(), b.height()), doc.mode, doc.depth);
    d.insert_above(None, Layer::new("upload", LayerContent::Raster(px)));
    export(&d).map(|bytes| (bytes, d.size.width, d.size.height))
}

/// The document to send for `source`, as PNG bytes: the composite, the active layer alone, or
/// the selection's crop — each downscaled to [`UPLOAD_MAX_EDGE`] on the long edge by
/// [`upload_png`].
fn source_png(doc: &Document, source: FillSource, active: Option<LayerId>) -> Result<Vec<u8>> {
    match source {
        FillSource::Canvas => upload_png(doc, "photocraft.png").map(|(bytes, _, _)| bytes),
        FillSource::Layer => {
            let id = active.ok_or_else(|| bad("ai.comfyui.fill", "`source` \"layer\" needs an active layer"))?;
            let layer = doc.layer(id).ok_or(EngineError::NoLayer(id))?;
            let mut d = Document::new("layer", doc.size, doc.mode, doc.depth);
            d.insert_above(None, layer.duplicate());
            upload_png(&d, "photocraft.png").map(|(bytes, _, _)| bytes)
        }
        FillSource::Selection => {
            let sel = doc.selection.as_ref().ok_or_else(|| bad("ai.comfyui.fill", "`source` \"selection\" needs a selection; make one, or use \"canvas\""))?;
            let area = sel.content_bounds().intersect(&doc.bounds());
            if area.width() == 0 || area.height() == 0 {
                return Err(bad("ai.comfyui.fill", "the selection is empty on the canvas"));
            }
            // `composite_to_surface` writes at absolute coordinates; move the crop to the origin.
            let px = photocraft_compose::composite_to_surface(doc, area, doc.pixel_format(), None);
            let px = translate_surface(&px, -area.x0, -area.y0);
            let mut d = Document::new("selection", Size::new(area.width(), area.height()), doc.mode, doc.depth);
            d.insert_above(None, Layer::new("source", LayerContent::Raster(px)));
            upload_png(&d, "photocraft.png").map(|(bytes, _, _)| bytes)
        }
    }
}

/// What the worker thread reports back: the generation plus what was written into the workflow.
struct FillOutcome {
    generation: comfy_cmds::Generation,
    injected: Injected,
    prompt: String,
    workflow: String,
    seed: u64,
}

fn transport_fn(s: &Session) -> Result<ComfyFn> {
    comfy_cmds::transport_of(s)
}

fn fill(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "ai.comfyui.fill";
    let params = fill_params(p)?;
    let workflow = load_workflow(s, &params.workflow)?;
    let st = s.active().ok_or_else(|| bad(CMD, "generation needs an open document (the source image comes from it)"))?;
    let doc = st.doc.clone();
    let selection = doc.selection.clone();
    if params.source == FillSource::Selection && selection.is_none() {
        return Err(bad(CMD, "`source` \"selection\" needs a selection; make one, or use \"canvas\""));
    }
    let active = st.active_layer;
    if params.source == FillSource::Layer && active.is_none() {
        return Err(bad(CMD, "`source` \"layer\" needs an active layer"));
    }
    // The Image API workflow bypasses the whole ComfyUI protocol: no server to upload to or poll.
    if is_api_workflow(&workflow) {
        return fill_image_api(s, params, doc, selection, active);
    }
    let transport = transport_fn(s)?;
    let url = comfy_cmds::url_of(s, p)?;
    let seed = params.seed.unwrap_or_else(comfy_cmds::random_seed);
    let client_id = format!("photocraft-fill-{:016x}", comfy_cmds::now_millis());
    let workflow_name = params.workflow.clone();
    let place = params.place;
    jobs::run(
        s,
        "Generate (ComfyUI)",
        false,
        move |ctx: &JobCtx| {
            ctx.progress(0.02, "Preparing the source image…");
            let png = source_png(&doc, params.source, active)?;
            ctx.progress(0.06, "Uploading…");
            let upload_name = format!("photocraft-{}.png", comfy_cmds::now_millis());
            let v = comfy_cmds::json_of(
                transport(&ComfyRequest { url: url.clone(), call: ComfyCall::Upload { name: upload_name.clone(), bytes: png } })
                    .map_err(|e| comfy_cmds::at("uploading the source image", e))?,
            )?;
            let uploaded = comfy_cmds::uploaded_name(&v).unwrap_or(upload_name);
            let mut wf = workflow.clone();
            let injected = inject(&mut wf, &params.prompt, &params.negative, Some(&uploaded), seed, params.steps);
            ctx.progress(0.08, &format!("Generating with {}…", workflow_name));
            let call = |c: ComfyCall| transport(&ComfyRequest { url: url.clone(), call: c });
            let generation = comfy_cmds::queue_and_collect(ctx, &call, &wf, &client_id, &workflow_name, "custom", seed, params.timeout_s)?;
            Ok(FillOutcome { generation, injected, prompt: params.prompt.clone(), workflow: workflow_name.clone(), seed })
        },
        move |s: &mut Session, outcome: FillOutcome| fill_done(s, outcome, &selection, place),
    )
}

/// `ai.comfyui.fill` behind the "Image API Edit" workflow: the source image and the prompt go to
/// the OpenAI-compatible endpoint in one call, the generated image comes straight back — nothing
/// of the ComfyUI protocol runs. Negative prompt, steps and seed have no API counterpart (the
/// seed still varies per run so shelf entries stay distinguishable).
fn fill_image_api(s: &mut Session, params: FillParams, doc: std::sync::Arc<Document>, selection: Option<Surface>, active: Option<LayerId>) -> Result<Value> {
    const CMD: &str = "ai.comfyui.fill";
    if params.prompt.trim().is_empty() {
        return Err(bad(CMD, "the Image API needs a prompt (there is no workflow that could carry a baked-in one)"));
    }
    let cfg = comfy_cmds::image_api_of(s, CMD)?;
    let transport = transport_fn(s)?;
    let seed = params.seed.unwrap_or_else(comfy_cmds::random_seed);
    let workflow_name = params.workflow.clone();
    let prompt = params.prompt.clone();
    let place = params.place;
    jobs::run(
        s,
        "Generate (Image API)",
        false,
        move |ctx: &JobCtx| {
            ctx.progress(0.02, "Preparing the source image…");
            let png = source_png(&doc, params.source, active)?;
            ctx.progress(0.08, &format!("Generating with {} (Image API)…", cfg.model));
            let started = std::time::Instant::now();
            let reply = transport(&ComfyRequest {
                url: cfg.base.clone(),
                call: ComfyCall::ApiImage { base: cfg.base.clone(), key: cfg.key.clone(), model: cfg.model.clone(), prompt: prompt.clone(), image: Some(png), timeout_s: params.timeout_s },
            })
            .map_err(|e| comfy_cmds::at("the Image API call", e))?;
            ctx.check()?;
            let bytes = comfy_cmds::bytes_of(reply)?;
            if bytes.is_empty() {
                return Err(EngineError::Other("the Image API returned an empty image".into()));
            }
            comfy_cmds::image_within_cap(bytes.len() as u64)?;
            ctx.progress(1.0, "Done");
            let generation = comfy_cmds::Generation {
                images: vec![bytes],
                model: cfg.model.clone(),
                family: "api",
                seed,
                prompt_id: format!("api-{:016x}", comfy_cmds::now_millis()),
                waited_ms: started.elapsed().as_millis() as u64,
            };
            Ok(FillOutcome { generation, injected: Injected::default(), prompt, workflow: workflow_name, seed })
        },
        move |s: &mut Session, outcome: FillOutcome| fill_done(s, outcome, &selection, place),
    )
}

/// What the UI thread does with any finished fill, whichever backend ran it: shelf the images,
/// then place them as the task asked.
fn fill_done(s: &mut Session, outcome: FillOutcome, selection: &Option<Surface>, place: FillPlace) -> Result<Value> {
    let created = comfy_cmds::now_millis();
    let mut indices = Vec::with_capacity(outcome.generation.images.len());
    for bytes in outcome.generation.images {
        indices.push(push_result(
            s,
            StoredResult { bytes, seed: outcome.seed, prompt: outcome.prompt.clone(), workflow: outcome.workflow.clone(), selection: selection.clone(), created_ms: created, file: None },
        ));
    }
    let shelf: Vec<(Vec<u8>, u64, Option<Surface>)> = indices
        .iter()
        .filter_map(|&i| s.comfy.results.get(i).map(|r| (r.bytes.clone(), r.seed, r.selection.clone())))
        .collect();
    if shelf.len() != indices.len() {
        return Err(EngineError::Other("a result vanished before it could be placed".into()));
    }
    let mut placed = Vec::new();
    match place {
        FillPlace::Layer => {
            for (bytes, _, selection) in &shelf {
                placed.push(place_result_layer(s, bytes, selection.as_ref())?);
            }
        }
        FillPlace::Native => {
            for (bytes, _, selection) in &shelf {
                placed.push(place_result_native(s, bytes, selection.as_ref())?);
            }
        }
        FillPlace::Document => {
            for (bytes, seed, _) in &shelf {
                placed.push(crate::cmd::file_cmds::open_bytes_as(s, &format!("ComfyUI {seed}.png"), bytes, None, None)?);
            }
        }
        FillPlace::None => {}
    }
    Ok(json!({
        "results": indices, "placed": placed, "place": match place { FillPlace::None => "none", FillPlace::Layer => "layer", FillPlace::Native => "native", FillPlace::Document => "document" },
        "seed": outcome.seed, "seconds": outcome.generation.waited_ms / 1000,
        "promptId": outcome.generation.prompt_id, "injected": outcome.injected.to_json(),
    }))
}

// ------------------------------------------------------------------ place / save

/// Put a generated image onto the canvas: scaled to cover the canvas when the workflow changed
/// its size, as a layer above the active one, masked to the selection snapshot it was generated
/// with. One history step.
fn place_result_layer(s: &mut Session, bytes: &[u8], selection: Option<&Surface>) -> Result<Value> {
    const CMD: &str = "ai.comfyui.place";
    let d = s.active().ok_or(EngineError::NoDocument)?;
    let (cw, ch) = (f64::from(d.doc.size.width), f64::from(d.doc.size.height));
    if cw <= 0.0 || ch <= 0.0 {
        return Err(bad(CMD, "the document canvas is empty"));
    }
    let fmt = d.doc.pixel_format();
    let imported = photocraft_io::import("ComfyUI Result.png", bytes).map_err(|e| EngineError::Other(format!("the generated image: {e}")))?;
    let px = crate::cmd::file_cmds::flattened_checked(&imported.document, fmt, CMD)?;
    let (gw, gh) = (f64::from(imported.document.size.width), f64::from(imported.document.size.height));
    if gw <= 0.0 || gh <= 0.0 {
        return Err(bad(CMD, "the generated image is empty"));
    }
    // The workflow may have resampled (FLUX's megapixel target): scale to cover and centre, so
    // the selection lines up; same-size results (the common case) stay untouched.
    let placed = if (gw - cw).abs() < 0.5 && (gh - ch).abs() < 0.5 {
        px
    } else {
        let k = (cw / gw).max(ch / gh);
        let scaled = resize_surface(&px, k, k, Resample::Bicubic);
        let dx = ((cw - gw * k) / 2.0).round() as i32;
        let dy = ((ch - gh * k) / 2.0).round() as i32;
        translate_surface(&scaled, dx, dy)
    };
    let mask = selection.cloned();
    s.edit("Place ComfyUI Result", |doc, active| {
        let mut layer = Layer::new("ComfyUI Result", LayerContent::Raster(placed.clone()));
        layer.mask = mask.clone().map(|surface| LayerMask { surface, linked: false, ..LayerMask::reveal_all() });
        let id = doc.insert_above(*active, layer);
        *active = Some(id);
        Ok(json!({"layer": id.0, "mask": mask.is_some()}))
    })
}

/// Put a generated image onto the canvas at its **native resolution**: no cover scaling — a
/// result larger than the canvas grows the canvas (centred) to fit it, a smaller one is centred
/// as-is. Above the active layer, masked to the selection snapshot it was generated with (note:
/// the snapshot lines up with a cover-scaled placement, so with a selection it may sit offset
/// here). Growing the canvas is one history step, the placement another.
fn place_result_native(s: &mut Session, bytes: &[u8], selection: Option<&Surface>) -> Result<Value> {
    const CMD: &str = "ai.comfyui.place";
    let (fmt, cw, ch) = {
        let d = s.active().ok_or(EngineError::NoDocument)?;
        (d.doc.pixel_format(), d.doc.size.width, d.doc.size.height)
    };
    let imported = photocraft_io::import("ComfyUI Result.png", bytes).map_err(|e| EngineError::Other(format!("the generated image: {e}")))?;
    let px = crate::cmd::file_cmds::flattened_checked(&imported.document, fmt, CMD)?;
    let (gw, gh) = (imported.document.size.width, imported.document.size.height);
    if gw == 0 || gh == 0 {
        return Err(bad(CMD, "the generated image is empty"));
    }
    if gw > cw || gh > ch {
        // `image.canvasSize` keeps every layer in place relative to the grown canvas.
        s.execute("image.canvasSize", json!({"width": gw.max(cw), "height": gh.max(ch), "anchor": "center"}))?;
    }
    let (cw, ch) = {
        let d = s.active().ok_or(EngineError::NoDocument)?;
        (f64::from(d.doc.size.width), f64::from(d.doc.size.height))
    };
    let dx = ((cw - f64::from(gw)) / 2.0).round() as i32;
    let dy = ((ch - f64::from(gh)) / 2.0).round() as i32;
    let placed = translate_surface(&px, dx, dy);
    let mask = selection.cloned();
    s.edit("Place ComfyUI Result (Full Size)", |doc, active| {
        let mut layer = Layer::new("ComfyUI Result", LayerContent::Raster(placed.clone()));
        layer.mask = mask.clone().map(|surface| LayerMask { surface, linked: false, ..LayerMask::reveal_all() });
        let id = doc.insert_above(*active, layer);
        *active = Some(id);
        Ok(json!({"layer": id.0, "mask": mask.is_some(), "canvas": [doc.size.width, doc.size.height]}))
    })
}

fn place(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "ai.comfyui.place";
    let idx = p.get("result").and_then(Value::as_u64).filter(|v| *v <= RESULT_LIMIT as u64).ok_or_else(|| bad(CMD, "`result` must be a result index from ai.comfyui.fill"))? as usize;
    let as_kind = match p.get("as").and_then(Value::as_str) {
        None | Some("layer") => "layer",
        Some("native") => "native",
        Some("document") => "document",
        Some(other) => return Err(bad(CMD, format!("`as` must be \"layer\", \"native\" or \"document\", not \"{other}\""))),
    };
    let Some(r) = s.comfy.results.get(idx) else {
        return Err(bad(CMD, format!("result {idx} is gone (the shelf keeps the newest {RESULT_LIMIT})")));
    };
    let (bytes, seed, selection) = (r.bytes.clone(), r.seed, r.selection.clone());
    match as_kind {
        "document" => crate::cmd::file_cmds::open_bytes_as(s, &format!("ComfyUI {seed}.png"), &bytes, None, None),
        "native" => place_result_native(s, &bytes, selection.as_ref()),
        _ => place_result_layer(s, &bytes, selection.as_ref()),
    }
}

/// Drop a shelf result — or the whole shelf — deleting its persisted PNG (if any) alongside.
fn remove_results(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "ai.comfyui.results.remove";
    if p.as_object().is_some_and(|o| o.len() > 1) {
        return Err(bad(CMD, "pass either `result` or `all`"));
    }
    if p.get("all").and_then(Value::as_bool) == Some(true) {
        let mut files = Vec::new();
        for r in s.comfy.results.drain(..) {
            if let Some(f) = &r.file {
                files.push(f.display().to_string());
                remove_file_quiet(f);
            }
        }
        write_index(&s.comfy);
        return Ok(json!({"removed": files.len(), "files": files}));
    }
    let idx = p.get("result").and_then(Value::as_u64).filter(|v| *v <= RESULT_LIMIT as u64).ok_or_else(|| bad(CMD, "`result` must be a result index from ai.comfyui.fill, or `all`: true"))? as usize;
    let Some(r) = s.comfy.results.get(idx) else {
        return Err(bad(CMD, format!("result {idx} is gone (the shelf keeps the newest {RESULT_LIMIT})")));
    };
    let file = r.file.clone();
    s.comfy.results.remove(idx);
    if let Some(f) = &file {
        remove_file_quiet(f);
    }
    write_index(&s.comfy);
    Ok(json!({"removed": 1, "files": file.map(|f| vec![f.display().to_string()]).unwrap_or_default()}))
}

fn save(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "ai.comfyui.save";
    let idx = p.get("result").and_then(Value::as_u64).filter(|v| *v <= RESULT_LIMIT as u64).ok_or_else(|| bad(CMD, "`result` must be a result index from ai.comfyui.fill"))? as usize;
    let path = p.get("path").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).ok_or_else(|| bad(CMD, "`path` must be the file to write"))?;
    if path.len() > 4_000 {
        return Err(bad(CMD, "`path` is too long"));
    }
    if !std::path::Path::new(path).extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("png")) {
        return Err(bad(CMD, "`path` must end in .png (the result is a PNG)"));
    }
    let Some(r) = s.comfy.results.get(idx) else {
        return Err(bad(CMD, format!("result {idx} is gone (the shelf keeps the newest {RESULT_LIMIT})")));
    };
    let (bytes, seed, len) = (r.bytes.clone(), r.seed, r.bytes.len());
    #[cfg(not(target_arch = "wasm32"))]
    {
        photocraft_format::atomic_write(std::path::Path::new(path), &bytes)
            .map_err(|e| EngineError::Other(format!("{path}: {e}")))?;
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = bytes;
        return Err(EngineError::Other("saving needs the desktop app".into()));
    }
    Ok(json!({"path": path, "bytes": len, "seed": seed}))
}

// ------------------------------------------------------------------ specs

fn enabled_with_doc(s: &Session) -> std::result::Result<(), String> {
    comfy_cmds::transport_enabled(s)?;
    if s.active().is_none() {
        return Err("generation needs an open document".into());
    }
    Ok(())
}

/// The ComfyUI workflow-store and fill commands.
pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: "ai.comfyui.fill",
            label: "Generate (ComfyUI)…",
            menu: &["File"],
            shortcut: None,
            params: r#"{"prompt":text,"negative":text,"workflow":text,"source":"canvas|layer|selection"="canvas","seed":int=-1,"steps":1..150,"place":"none|layer|native|document"="none","timeout":10..3600=600,"url":text} (generate through a stored ComfyUI workflow (ai.comfyui.workflows.list/import): uploads the composite, the active layer, or the selection's crop, downscaled to 3072 px on the long edge first; writes prompt/negative into the text encodes — or, in a graph with none, into its `prompt`/`提示词` input, an empty prompt keeping the workflow's own value — and the upload into the first LoadImage; the generated images land on the result shelf (place "none"), as selection-masked layers scaled to cover the canvas (place "layer"), at their native resolution with the canvas growing to fit (place "native"), or as their own documents; shelf results persist in the generations folder and survive a restart)"#,
            enabled: enabled_with_doc,
            run: fill,
            journal: true,
        },
        CommandSpec {
            id: "ai.comfyui.place",
            label: "Place ComfyUI Result",
            menu: &[],
            shortcut: None,
            params: r#"{"result":0..32,"as":"layer|native|document"="layer"} (put a shelf result onto the canvas: a layer masked to the selection snapshot it was generated with, scaled to cover the canvas; "native" keeps its resolution with the canvas growing to fit; "document" opens it as its own document)"#,
            enabled: enabled_with_doc,
            run: place,
            journal: true,
        },
        CommandSpec {
            id: "ai.comfyui.results.remove",
            label: "Remove ComfyUI Result",
            menu: &[],
            shortcut: None,
            params: r#"{"result":0..32} | {"all":true} (drop a shelf result — or the whole shelf — from the panel's gallery; its persisted PNG in the generations folder is deleted too)"#,
            enabled: |_| Ok(()),
            run: remove_results,
            journal: false,
        },
        CommandSpec {
            id: "ai.comfyui.save",
            label: "Save ComfyUI Result",
            menu: &[],
            shortcut: None,
            params: r#"{"result":0..32,"path":text} (write a shelf result to a .png file)"#,
            enabled: |_| Ok(()),
            run: save,
            journal: false,
        },
        CommandSpec {
            id: "ai.comfyui.workflows.list",
            label: "List ComfyUI Workflows",
            menu: &[],
            shortcut: None,
            params: "{}",
            enabled: |_| Ok(()),
            run: list_workflows,
            journal: false,
        },
        CommandSpec {
            id: "ai.comfyui.workflows.import",
            label: "Import ComfyUI Workflow…",
            menu: &[],
            shortcut: None,
            params: r#"{"path":text,"url":text} (import a workflow file: a ComfyUI Workflow › Save file is converted to the API format against the server's nodes, a Workflow › Export (API) file is stored as-is; it lands in the workflows folder)"#,
            enabled: comfy_cmds::transport_enabled,
            run: import_workflow,
            journal: false,
        },
        CommandSpec {
            id: "ai.comfyui.workflows.remove",
            label: "Remove ComfyUI Workflow",
            menu: &[],
            shortcut: None,
            params: r#"{"name":text} (remove an imported workflow from the folder)"#,
            enabled: |_| Ok(()),
            run: remove_workflow,
            journal: false,
        },
    ]
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::comfy_cmds::ComfyReply;
    use photocraft_color::{ColorMode, PixelFormat, SampleType};
    use photocraft_doc::Size;
    use photocraft_geom::Rect;
    use serde_json::json;

    // A small `/object_info` covering the fixture's node classes: combos, the seed's trailing
    // control, a dynamic combo with sub-inputs, dotted autogrow inputs, connection-only types.
    fn object_info_fixture() -> Value {
        json!({
            "LoadImage": {"input": {"required": {"image": [["fashion_portrait_blue_silk_gown_vis.png"], {}]}},
                "input_order": {"required": ["image"], "optional": []}},
            "CLIPLoader": {"input": {"required": {
                "clip_name": [["qwen3vl_4b_int8_convrot.safetensors", "qwen3.5_9b_flux3_prompt_upsampler_w4a8.safetensors"], {}],
                "type": [["flux3"], {}], "device": [["default"], {}]}},
                "input_order": {"required": ["clip_name", "type", "device"], "optional": []}},
            "VAELoader": {"input": {"required": {"vae_name": [["flux2-vae.safetensors"], {}]}},
                "input_order": {"required": ["vae_name"], "optional": []}},
            "UNETLoader": {"input": {"required": {
                "unet_name": [["flux3-image-klein-base-int8-convrot.safetensors"], {}],
                "weight_dtype": [["default"], {}]}},
                "input_order": {"required": ["unet_name", "weight_dtype"], "optional": []}},
            "CLIPTextEncode": {"input": {"required": {"text": ["STRING", {"multiline": true, "default": ""}], "clip": ["CLIP", {}]}},
                "input_order": {"required": ["text", "clip"], "optional": []}},
            "KSampler": {"input": {"required": {
                "model": ["MODEL", {}], "positive": ["CONDITIONING", {}], "negative": ["CONDITIONING", {}],
                "latent_image": ["LATENT", {}],
                "seed": ["INT", {"default": 0, "min": 0, "max": 2147483647, "control_after_generate": true}],
                "steps": ["INT", {"default": 20, "min": 1, "max": 10000}],
                "cfg": ["FLOAT", {"default": 8.0, "min": 0.0, "max": 100.0, "step": 0.1}],
                "sampler_name": [["euler", "euler_ancestral", "dpmpp_2m"], {}],
                "scheduler": [["normal", "karras", "simple"], {}],
                "denoise": ["FLOAT", {"default": 1.0, "min": 0.0, "max": 1.0, "step": 0.01}]}},
                "input_order": {"required": ["model", "positive", "negative", "latent_image", "seed", "steps", "cfg", "sampler_name", "scheduler", "denoise"], "optional": []}},
            "VAEDecode": {"input": {"required": {"samples": ["LATENT", {}], "vae": ["VAE", {}]}},
                "input_order": {"required": ["samples", "vae"], "optional": []}},
            "SaveImage": {"input": {"required": {"images": ["IMAGE", {}], "filename_prefix": ["STRING", {"default": "ComfyUI"}]}},
                "input_order": {"required": ["images", "filename_prefix"], "optional": []}},
            "ReferenceMultiLatent": {"input": {
                "required": {"positive": ["CONDITIONING", {}],
                    "resolution": ["COMFY_DYNAMICCOMBO_V3", {"options": [
                        {"key": "keep resolution", "inputs": {"required": {}}},
                        {"key": "resize", "inputs": {"required": {"megapixels": ["FLOAT", {"default": 1.0}]}}}]}]},
                "optional": {"negative": ["CONDITIONING", {}], "vae": ["VAE", {}],
                    "images": ["COMFY_AUTOGROW_V3", {"template": {"input": {"required": {}, "optional": {"image": ["IMAGE", {}]}}}, "prefix": "image", "min": 1, "max": 100}]}},
                "input_order": {"required": ["positive", "resolution"], "optional": ["negative", "vae", "images"]}},
            "TextGenerate": {"input": {
                "required": {"prompt": ["STRING", {"multiline": true}], "clip": ["CLIP", {}]},
                "optional": {"image": ["IMAGE", {}], "video": ["IMAGE", {}], "audio": ["AUDIO", {}],
                    "system_prompt": ["STRING", {"multiline": true}], "max_tokens": ["INT", {"default": 4096}]}},
                "input_order": {"required": ["prompt", "clip"], "optional": ["image", "video", "audio", "system_prompt", "max_tokens"]}},
            "ComfySwitchNode": {"input": {"required": {}, "optional": {"on_false": ["*", {}], "on_true": ["*", {}], "switch": ["BOOLEAN", {"default": false}]}},
                "input_order": {"required": [], "optional": ["on_false", "on_true", "switch"]}},
            "Flux3LayoutPrompt": {"input": {"required": {"element": ["BBOXES", {}], "prompt": ["STRING", {"multiline": true}]}},
                "input_order": {"required": ["element", "prompt"], "optional": []}},
            "CreateBoundingBoxes": {"input": {"required": {"width": ["INT", {"default": 1024}], "height": ["INT", {"default": 1024}]},
                "optional": {"bboxes": ["BBOXES", {}]}},
                "input_order": {"required": ["width", "height"], "optional": ["bboxes"]}},
            "BatchImagesNode": {"input": {"required": {}, "optional": {}}, "input_order": {"required": [], "optional": []}},
            "PreviewAny": {"input": {"required": {"source": ["*", {}]}}, "input_order": {"required": ["source"], "optional": []}},
            "ImageCompare": {"input": {"required": {"compare_view": ["IMAGECOMPARE", {"socketless": true}]},
                "optional": {"image_a": ["IMAGE", {}], "image_b": ["IMAGE", {}]}},
                "input_order": {"required": ["compare_view"], "optional": ["image_a", "image_b"]}},
            "EmptyFlux2LatentImage": {"input": {"required": {
                "width": ["INT", {"default": 1024}], "height": ["INT", {"default": 1024}], "batch_size": ["INT", {"default": 1}]}},
                "input_order": {"required": ["width", "height", "batch_size"], "optional": []}},
        })
    }

    #[test]
    fn converts_the_flux3_ui_save() {
        let ui: Value = serde_json::from_str(include_str!("comfy_flux3_ui_fixture.json")).unwrap();
        let api = convert_ui_workflow(&ui, &object_info_fixture()).unwrap();
        // Notes and primitives are gone; the real nodes keep their ids and wiring.
        assert!(api.get("10").is_none() && api.get("26").is_none() && api.get("48").is_none());
        assert_eq!(api.pointer("/54/class_type"), Some(&json!("LoadImage")));
        assert_eq!(api.pointer("/54/inputs/image"), Some(&json!("fashion_portrait_blue_silk_gown_vis.png")));
        // The seed keeps its drawn value and drops the trailing "randomize"; the rest line up.
        assert_eq!(api.pointer("/7/inputs/seed"), Some(&json!(341_993_890_347_040_u64)));
        assert_eq!(api.pointer("/7/inputs/steps"), Some(&json!(50)));
        let cfg = api.pointer("/7/inputs/cfg").and_then(Value::as_f64).unwrap();
        assert!((cfg - 1.0).abs() < 1e-9, "{cfg}");
        assert_eq!(api.pointer("/7/inputs/model"), Some(&json!(["59", 0])));
        assert_eq!(api.pointer("/7/inputs/negative"), Some(&json!(["17", 1])));
        // The dynamic combo keeps its choice and its sub-input under the dotted name.
        assert_eq!(api.pointer("/17/inputs/resolution"), Some(&json!("resize")));
        let mp = api.pointer("/17/inputs/resolution.megapixels").and_then(Value::as_f64).unwrap();
        assert!((mp - 1.0).abs() < 1e-9, "{mp}");
        // Dotted autogrow inputs pass through verbatim.
        assert_eq!(api.pointer("/17/inputs/images.image0"), Some(&json!(["54", 0])));
        assert_eq!(api.pointer("/17/inputs/vae"), Some(&json!(["3", 0])));
        // The prompt flows through the switch node into the positive encode; the negative
        // encode's widget (unconnected) is used as-is.
        assert_eq!(api.pointer("/4/inputs/text"), Some(&json!(["52", 0])));
        assert_eq!(api.pointer("/4/inputs/clip"), Some(&json!(["2", 0])));
        assert_eq!(api.pointer("/58/inputs/text"), Some(&json!("")));
        // The primitive system prompt became a constant in TextGenerate's input.
        let sys = api.pointer("/47/inputs/system_prompt").and_then(Value::as_str).unwrap_or_default();
        assert!(sys.starts_with("You are the prompt upsampler"), "{sys}");
        assert!(api.pointer("/47/inputs/clip").is_some());
        assert_eq!(api.pointer("/9/inputs/filename_prefix"), Some(&json!("image_flux3_klein_distilled_image_edit")));
        assert_eq!(api.pointer("/59/inputs/unet_name"), Some(&json!("flux3-image-klein-base-int8-convrot.safetensors")));
    }

    #[test]
    fn socketless_widgets_convert_to_null_instead_of_failing() {
        let oi = object_info_fixture();
        // ComfyUI 0.44's ImageCompare: `compare_view` is a socketless frontend widget — a UI save
        // carries no value for it, and the old conversion refused the whole workflow over it.
        let ui = json!({"nodes": [
            {"id": 1, "type": "LoadImage", "widgets_values": ["photo.png", "image"], "inputs": []},
            {"id": 2, "type": "ImageCompare", "widgets_values": [], "inputs": [{"name": "image_a", "link": 3}]},
        ], "links": [[3, 1, 0, 2, 0, "IMAGE"]]});
        let api = convert_ui_workflow(&ui, &oi).unwrap();
        assert_eq!(api.pointer("/2/inputs/compare_view"), Some(&Value::Null));
        assert_eq!(api.pointer("/2/inputs/image_a"), Some(&json!(["1", 0])));
    }

    #[test]
    fn reroutes_are_followed_and_errors_name_the_node() {
        let oi = object_info_fixture();
        // A Reroute between the loader and the encode.
        let ui = json!({"nodes": [
            {"id": 1, "type": "CLIPLoader", "widgets_values": ["qwen3vl_4b_int8_convrot.safetensors", "flux3", "default"], "inputs": []},
            {"id": 2, "type": "Reroute", "inputs": [{"name": "", "link": 9}], "outputs": [{"name": "", "links": [8]}]},
            {"id": 3, "type": "CLIPTextEncode", "widgets_values": ["a cat"], "inputs": [{"name": "clip", "link": 8}]},
        ], "links": [[8, 2, 0, 3, 0, "CLIP"], [9, 1, 0, 2, 0, "CLIP"]]});
        let api = convert_ui_workflow(&ui, &oi).unwrap();
        assert_eq!(api.pointer("/3/inputs/clip"), Some(&json!(["1", 0])));
        assert_eq!(api.pointer("/3/inputs/text"), Some(&json!("a cat")));
        assert!(api.get("2").is_none());
        // A node the server does not have names the node in the error.
        let ui = json!({"nodes": [{"id": 5, "type": "MysteryNode", "widgets_values": [], "inputs": []}], "links": []});
        let e = convert_ui_workflow(&ui, &oi).unwrap_err();
        assert!(e.contains("MysteryNode") && e.contains("node 5"), "{e}");
        // A required connection-only input left open fails, naming node and input.
        let ui = json!({"nodes": [
            {"id": 1, "type": "VAEDecode", "widgets_values": [], "inputs": [{"name": "vae", "link": 4}]},
        ], "links": [[4, 9, 0, 1, 1, "VAE"]]});
        let e = convert_ui_workflow(&ui, &oi).unwrap_err();
        assert!(e.contains("samples") && e.contains("node 1"), "{e}");
        // Not a UI workflow at all.
        assert!(convert_ui_workflow(&json!({}), &oi).is_err());
        assert!(convert_ui_workflow(&json!({"nodes": "no"}), &oi).is_err());
    }

    #[test]
    fn inject_writes_prompt_seed_steps_and_image() {
        let mut wf: Value = serde_json::from_str(include_str!("comfy_flux3_klein.json")).unwrap();
        let injected = inject(&mut wf, "a red dress", "blurry", Some("photocraft-7.png"), 99, Some(12));
        // Node 4 feeds ReferenceMultiLatent.positive → prompt; node 58 feeds its negative.
        assert_eq!(injected.prompt_nodes, ["4"]);
        assert_eq!(injected.negative_nodes, ["58"]);
        assert_eq!(injected.image_nodes, ["54"]);
        assert_eq!(injected.sampler_nodes, ["7"]);
        assert_eq!(wf.pointer("/4/inputs/text"), Some(&json!("a red dress")));
        assert_eq!(wf.pointer("/58/inputs/text"), Some(&json!("blurry")));
        assert_eq!(wf.pointer("/54/inputs/image"), Some(&json!("photocraft-7.png")));
        assert_eq!(wf.pointer("/7/inputs/seed"), Some(&json!(99)));
        assert_eq!(wf.pointer("/7/inputs/steps"), Some(&json!(12)));
        // Without steps, the workflow's own value stays.
        let mut wf: Value = serde_json::from_str(include_str!("comfy_flux3_klein.json")).unwrap();
        inject(&mut wf, "p", "", None, 5, None);
        assert_eq!(wf.pointer("/7/inputs/steps"), Some(&json!(50)));
        // noise_seed samplers (KSamplerAdvanced) are written too.
        let mut wf = json!({"1": {"class_type": "KSamplerAdvanced", "inputs": {"noise_seed": 0, "negative": ["2", 0]}},
            "2": {"class_type": "CLIPTextEncode", "inputs": {"text": "", "clip": ["3", 0]}}});
        let injected = inject(&mut wf, "p", "n", None, 7, None);
        assert_eq!(wf.pointer("/1/inputs/noise_seed"), Some(&json!(7)));
        assert_eq!(wf.pointer("/2/inputs/text"), Some(&json!("n")));
        assert_eq!(injected.negative_nodes, ["2"]);
    }

    #[test]
    fn builtin_preset_parses_and_lists() {
        let builtins = builtin_workflows();
        assert_eq!(builtins.len(), 1);
        assert_eq!(builtins[0].0, "FLUX.3 Klein Image Edit");
        let mut s = Session::new();
        let r = s.execute("ai.comfyui.workflows.list", json!({})).unwrap();
        let list = r.get("workflows").and_then(Value::as_array).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["name"], json!("FLUX.3 Klein Image Edit"));
        assert_eq!(list[0]["builtin"], json!(true));
        assert_eq!(list[0]["analyze"]["promptNodes"], json!(["4"]));
        assert_eq!(list[0]["analyze"]["imageNodes"], json!(["54"]));
    }

    thread_local! {
        static CALLS: std::cell::RefCell<Vec<(&'static str, String)>> = const { std::cell::RefCell::new(Vec::new()) };
        static UPLOADS: std::cell::RefCell<Vec<Vec<u8>>> = const { std::cell::RefCell::new(Vec::new()) };
        static API_CALLS: std::cell::RefCell<Vec<Value>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    /// A transport that answers like ComfyUI, recording the upload name and queued workflow. The
    /// Image API arms record the call and answer like an OpenAI-compatible endpoint.
    fn mock_transport() -> ComfyFn {
        let png = test_png();
        std::sync::Arc::new(move |req: &ComfyRequest| match &req.call {
            ComfyCall::ObjectInfo => Ok(ComfyReply::Json(object_info_fixture())),
            ComfyCall::Upload { name, bytes } => {
                CALLS.with(|c| c.borrow_mut().push(("upload", name.clone())));
                UPLOADS.with(|u| u.borrow_mut().push(bytes.clone()));
                Ok(ComfyReply::Json(json!({"name": name, "subfolder": ""})))
            }
            ComfyCall::Queue { workflow, .. } => {
                CALLS.with(|c| c.borrow_mut().push(("queue", workflow.to_string())));
                Ok(ComfyReply::Json(json!({"prompt_id": "pid-9"})))
            }
            ComfyCall::History { .. } => Ok(ComfyReply::Json(json!({"pid-9": {"outputs": {
                "9": {"images": [{"filename": "out.png", "subfolder": "", "type": "output"}]}},
                "status": {"status_str": "success"}}}))),
            ComfyCall::Download { .. } => Ok(ComfyReply::Bytes(png.clone())),
            ComfyCall::ApiImage { base, key, model, prompt, image, .. } => {
                API_CALLS.with(|c| {
                    c.borrow_mut().push(json!({
                        "url": req.url, "base": base, "key": key, "model": model, "prompt": prompt,
                        "image": image.as_ref().map(|b| b.len()),
                    }))
                });
                Ok(ComfyReply::Bytes(png.clone()))
            }
            ComfyCall::ApiCheck { base, key } => {
                API_CALLS.with(|c| c.borrow_mut().push(json!({"check": true, "base": base, "key": key})));
                Ok(ComfyReply::Json(json!({"data": [{"id": "gpt-image-2"}, {"id": "other"}]})))
            }
            _ => Ok(ComfyReply::Json(Value::Null)),
        })
    }

    /// A 4×4 opaque PNG, as ComfyUI would return one.
    fn test_png() -> Vec<u8> {
        let fmt = PixelFormat::new(ColorMode::Rgb, SampleType::U8, false);
        let surface = Surface::from_interleaved(fmt, Rect::new(0, 0, 4, 4), &[255; 48]);
        let mut doc = Document::new("t", Size::new(4, 4), ColorMode::Rgb, SampleType::U8);
        doc.insert_above(None, Layer::new("l", LayerContent::Raster(surface)));
        photocraft_io::export(&doc, "t.png", &photocraft_io::ExportOptions::default()).unwrap().bytes
    }

    fn doc_with_selection() -> Session {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 64, "height": 64, "background": "white"})).unwrap();
        s.execute("select.rect", json!({"x": 8, "y": 12, "width": 40, "height": 30})).unwrap();
        s.comfy.transport = Some(mock_transport());
        s
    }

    fn last_queued() -> Value {
        CALLS.with(|c| c.borrow().iter().rev().find(|(k, _)| *k == "queue").map(|(_, v)| serde_json::from_str(v).unwrap()).unwrap())
    }

    fn last_upload() -> Option<Vec<u8>> {
        UPLOADS.with(|u| u.borrow().last().cloned())
    }

    #[test]
    fn inject_falls_back_to_prompt_inputs_without_text_encodes() {
        // An API-style graph (the Xin OpenAI node): no text encodes, the prompt lives in 提示词.
        let mut wf = json!({
            "360": {"class_type": "LoadImage", "inputs": {"image": "a.png"}},
            "374": {"class_type": "OpenAPIImage", "inputs": {"提示词": "文件里烧好的提示词", "随机种": 7}},
        });
        let injected = inject(&mut wf, "漫展室内展馆背景", "", None, 9, None);
        assert_eq!(injected.prompt_nodes, ["374"]);
        assert_eq!(wf.pointer("/374/inputs/提示词"), Some(&json!("漫展室内展馆背景")));
        // analyze lists it for the panel's "prompt → N" line.
        assert_eq!(analyze(&wf)["promptNodes"], json!(["374"]));
        // An empty prompt keeps the workflow's own value: running an imported graph as-is works.
        let injected = inject(&mut wf, "", "", None, 9, None);
        assert!(injected.prompt_nodes.is_empty());
        assert_eq!(wf.pointer("/374/inputs/提示词"), Some(&json!("漫展室内展馆背景")));
        // English `prompt` with `negative_prompt` on the same node; a wired `prompt` stays a link.
        let mut wf = json!({
            "1": {"class_type": "Node", "inputs": {"prompt": "p0", "negative_prompt": ""}},
            "2": {"class_type": "Node", "inputs": {"prompt": ["1", 0]}},
        });
        let injected = inject(&mut wf, "new", "blurry", None, 9, None);
        assert_eq!(injected.prompt_nodes, ["1"]);
        assert_eq!(injected.negative_nodes, ["1"]);
        assert_eq!(wf.pointer("/1/inputs/prompt"), Some(&json!("new")));
        assert_eq!(wf.pointer("/1/inputs/negative_prompt"), Some(&json!("blurry")));
        assert_eq!(wf.pointer("/2/inputs/prompt"), Some(&json!(["1", 0])));
    }

    #[test]
    fn fill_caps_oversized_uploads_and_keeps_small_ones() {
        // A tens-of-megapixel composite is what times out image-edit endpoints (HTTP 524 on the
        // relay): the upload is downscaled to 3072 px on the long edge first.
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 4500, "height": 3000, "background": "white"})).unwrap();
        s.comfy.transport = Some(mock_transport());
        s.execute("ai.comfyui.fill", json!({"workflow": "FLUX.3 Klein Image Edit", "prompt": "p", "place": "none"})).unwrap();
        let bytes = last_upload().expect("an upload happened");
        let sent = photocraft_io::import("upload.png", &bytes).unwrap().document.size;
        assert!((3071u32..=3073).contains(&sent.width), "{}x{}", sent.width, sent.height);
        assert!(sent.height <= 2049, "{}x{}", sent.width, sent.height);
        // A mid-size document (a 2K generation's input) stays inside the cap untouched.
        s.execute("file.new", json!({"width": 3000, "height": 2000, "background": "white"})).unwrap();
        s.execute("ai.comfyui.fill", json!({"workflow": "FLUX.3 Klein Image Edit", "prompt": "p", "place": "none"})).unwrap();
        let bytes = last_upload().expect("an upload happened");
        let sent = photocraft_io::import("upload.png", &bytes).unwrap().document.size;
        assert_eq!((sent.width, sent.height), (3000, 2000));
        // A small document is uploaded as-is.
        let fmt = PixelFormat::new(ColorMode::Rgb, SampleType::U8, false);
        let surface = Surface::from_interleaved(fmt, Rect::new(0, 0, 64, 64), &[255; 64 * 64 * 3]);
        let mut d = Document::new("t", Size::new(64, 64), ColorMode::Rgb, SampleType::U8);
        d.insert_above(None, Layer::new("l", LayerContent::Raster(surface)));
        let (_, w, h) = upload_png(&d, "photocraft.png").unwrap();
        assert_eq!((w, h), (64, 64));
    }

    #[test]
    fn fill_uploads_selection_queues_and_places_masked_layer() {
        let mut s = doc_with_selection();
        let r = s
            .execute("ai.comfyui.fill", json!({"workflow": "FLUX.3 Klein Image Edit", "prompt": "a red dress", "negative": "blurry", "seed": 42, "source": "selection", "place": "layer"}))
            .unwrap();
        // The queued graph carries prompt, negative, seed and the uploaded crop.
        let wf = last_queued();
        assert_eq!(wf.pointer("/4/inputs/text"), Some(&json!("a red dress")));
        assert_eq!(wf.pointer("/58/inputs/text"), Some(&json!("blurry")));
        assert_eq!(wf.pointer("/7/inputs/seed"), Some(&json!(42)));
        let uploaded = wf.pointer("/54/inputs/image").and_then(Value::as_str).unwrap().to_string();
        assert!(uploaded.starts_with("photocraft-"), "{uploaded}");
        CALLS.with(|c| assert!(c.borrow().iter().any(|(k, v)| *k == "upload" && v == &uploaded)));
        // The result was placed as a layer masked to the selection, and shelved.
        assert_eq!(r["seed"], json!(42));
        assert_eq!(r["place"], json!("layer"));
        assert_eq!(r["placed"][0]["mask"], json!(true));
        let d = s.active().unwrap();
        let id = photocraft_doc::LayerId(r["placed"][0]["layer"].as_u64().unwrap() as u64);
        let layer = d.doc.layer(id).unwrap();
        assert!(layer.mask.is_some());
        assert_eq!(d.doc.layers.len(), 2); // background + generated
        assert_eq!(s.comfy.results.len(), 1);
        assert_eq!(s.comfy.results[0].seed, 42);
        assert_eq!(s.journal.last().map(|(id, _)| id.as_str()), Some("ai.comfyui.fill"));
    }

    #[test]
    fn fill_canvas_and_layer_sources_need_what_they_need() {
        // No document: an actionable error.
        let mut s = Session::new();
        s.comfy.transport = Some(mock_transport());
        let e = s.execute("ai.comfyui.fill", json!({"workflow": "FLUX.3 Klein Image Edit"})).unwrap_err().to_string();
        assert!(e.contains("document"), "{e}");
        // Canvas source needs no selection; the upload is the whole composite.
        let mut s = doc_with_selection();
        s.execute("select.deselect", json!({})).unwrap();
        let r = s.execute("ai.comfyui.fill", json!({"workflow": "FLUX.3 Klein Image Edit", "prompt": "p", "source": "canvas", "place": "none"})).unwrap();
        assert_eq!(r["place"], json!("none"));
        assert_eq!(r["results"].as_array().map(Vec::len), Some(1));
        // Selection source without a selection: an actionable error.
        let e = s.execute("ai.comfyui.fill", json!({"workflow": "FLUX.3 Klein Image Edit", "source": "selection"})).unwrap_err().to_string();
        assert!(e.contains("selection"), "{e}");
        // Layer source sends the active layer alone.
        let r = s.execute("ai.comfyui.fill", json!({"workflow": "FLUX.3 Klein Image Edit", "prompt": "p", "source": "layer", "place": "none"})).unwrap();
        assert_eq!(r["place"], json!("none"));
        // A negative seed means random; unknown workflows and junk params error, never panic.
        for p in [
            json!({"workflow": "No Such Workflow"}),
            json!({"workflow": "FLUX.3 Klein Image Edit", "source": "video"}),
            json!({"workflow": "FLUX.3 Klein Image Edit", "place": "side"}),
            json!({"workflow": 42}),
            json!({}),
            json!({"workflow": "FLUX.3 Klein Image Edit", "prompt": 7}),
            json!({"workflow": "../evil"}),
        ] {
            assert!(s.execute("ai.comfyui.fill", p.clone()).is_err(), "{p}");
        }
        let r = s.execute("ai.comfyui.fill", json!({"workflow": "FLUX.3 Klein Image Edit", "seed": -1})).unwrap();
        assert!(r["seed"].as_u64().is_some());
    }

    #[test]
    fn place_and_save_shelf_results() {
        let mut s = doc_with_selection();
        s.execute("ai.comfyui.fill", json!({"workflow": "FLUX.3 Klein Image Edit", "prompt": "p", "place": "none"})).unwrap();
        // Place as a layer: masked to the generation-time selection snapshot even though the
        // selection has since changed.
        s.execute("select.rect", json!({"x": 0, "y": 0, "width": 5, "height": 5})).unwrap();
        let r = s.execute("ai.comfyui.place", json!({"result": 0, "as": "layer"})).unwrap();
        assert_eq!(r["mask"], json!(true));
        assert_eq!(s.active().unwrap().doc.layers.len(), 2);
        // Place as a document: the active one becomes the 4×4 generated image.
        let r = s.execute("ai.comfyui.place", json!({"result": 0, "as": "document"})).unwrap();
        assert_eq!(r["document"], json!(1));
        assert_eq!(s.active().map(|d| d.doc.size), Some(Size::new(4, 4)));
        // Save writes the bytes.
        let dir = std::env::temp_dir().join(format!("photocraft-comfy-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("result.png");
        let r = s.execute("ai.comfyui.save", json!({"result": 0, "path": out.display().to_string()})).unwrap();
        assert_eq!(r["bytes"], json!(s.comfy.results[0].bytes.len()));
        assert_eq!(std::fs::read(&out).unwrap().len(), s.comfy.results[0].bytes.len());
        // Junk is refused, not panicked on.
        for p in [json!({"result": 99}), json!({}), json!({"result": 0, "as": "sky"}), json!({"result": 0, "path": "x.jpg"}), json!({"result": 0, "path": ""})] {
            assert!(s.execute("ai.comfyui.place", p.clone()).is_err() || s.execute("ai.comfyui.save", p.clone()).is_err(), "{p}");
        }
        assert!(s.execute("ai.comfyui.place", json!({"result": 500})).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn shelf_stays_bounded() {
        let mut s = doc_with_selection();
        for i in 0..(RESULT_LIMIT + 4) {
            s.execute("ai.comfyui.fill", json!({"workflow": "FLUX.3 Klein Image Edit", "prompt": "p", "seed": i, "place": "none"})).unwrap();
        }
        assert_eq!(s.comfy.results.len(), RESULT_LIMIT);
        // And the newest results are the ones kept.
        assert_eq!(s.comfy.results.last().map(|r| r.seed), Some(RESULT_LIMIT as u64 + 3));
    }

    #[test]
    fn import_converts_a_ui_save_into_the_store() {
        let mut s = Session::new();
        s.comfy.transport = Some(mock_transport());
        // No folder yet (no shell injection, no preference): an actionable error.
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cmd/comfy_flux3_ui_fixture.json");
        let e = s.execute("ai.comfyui.workflows.import", json!({"path": fixture.display().to_string()})).unwrap_err().to_string();
        assert!(e.contains("workflow folder"), "{e}");
        // A file that is not there says so.
        let e = s.execute("ai.comfyui.workflows.import", json!({"path": "/no/such/file.json"})).unwrap_err().to_string();
        assert!(e.contains("No such file"), "{e}");
        // Import the fixture into a temp folder set through the preference.
        let dir = std::env::temp_dir().join(format!("photocraft-comfy-store-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        s.prefs.edit(|p| p.integrations.comfy_workflows_dir = dir.display().to_string());
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cmd/comfy_flux3_ui_fixture.json");
        let r = s.execute("ai.comfyui.workflows.import", json!({"path": src.display().to_string()})).unwrap();
        assert_eq!(r["name"], json!("comfy_flux3_ui_fixture"));
        assert_eq!(r["converted"], json!(true));
        // It now lists beside the built-in, loads, and is removable.
        let list = s.execute("ai.comfyui.workflows.list", json!({})).unwrap();
        let names: Vec<&str> = list["workflows"].as_array().unwrap().iter().map(|w| w["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"comfy_flux3_ui_fixture") && names.contains(&"FLUX.3 Klein Image Edit"));
        let r = s.execute("ai.comfyui.fill", json!({})).unwrap_err().to_string(); // no document
        assert!(r.contains("document"), "{r}");
        s.execute("file.new", json!({"width": 32, "height": 32, "background": "white"})).unwrap();
        s.execute("ai.comfyui.fill", json!({"workflow": "comfy_flux3_ui_fixture", "prompt": "p", "place": "none"})).unwrap();
        // The queued graph is the converted one (its SaveImage prefix survives).
        let wf = last_queued();
        assert_eq!(wf.pointer("/9/inputs/filename_prefix"), Some(&json!("image_flux3_klein_distilled_image_edit")));
        s.execute("ai.comfyui.workflows.remove", json!({"name": "comfy_flux3_ui_fixture"})).unwrap();
        let builtins_only = s.execute("ai.comfyui.workflows.list", json!({})).unwrap();
        assert_eq!(builtins_only["workflows"].as_array().map(Vec::len), Some(1));
        // The built-in cannot be removed; bad names are refused.
        assert!(s.execute("ai.comfyui.workflows.remove", json!({"name": "FLUX.3 Klein Image Edit"})).is_err());
        assert!(s.execute("ai.comfyui.workflows.remove", json!({"name": "../evil"})).is_err());
        assert!(s.execute("ai.comfyui.workflows.import", json!({"path": 7})).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fill_place_parses_native() {
        assert_eq!(FillPlace::parse(Some("native")).unwrap(), FillPlace::Native);
        assert_eq!(FillPlace::parse(None).unwrap(), FillPlace::None);
        assert_eq!(FillPlace::parse(Some("layer")).unwrap(), FillPlace::Layer);
        let e = FillPlace::parse(Some("big")).unwrap_err().to_string();
        assert!(e.contains("native"), "{e}");
    }

    /// A solid opaque PNG of any size, as `test_png` is of 4×4.
    fn solid_png(w: u32, h: u32) -> Vec<u8> {
        let fmt = PixelFormat::new(ColorMode::Rgb, SampleType::U8, false);
        let surface = Surface::from_interleaved(fmt, Rect::new(0, 0, w as i32, h as i32), &vec![255; (w * h * 3) as usize]);
        let mut doc = Document::new("t", Size::new(w, h), ColorMode::Rgb, SampleType::U8);
        doc.insert_above(None, Layer::new("l", LayerContent::Raster(surface)));
        photocraft_io::export(&doc, "t.png", &photocraft_io::ExportOptions::default()).unwrap().bytes
    }

    fn pngs_in(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("png")))
            .collect()
    }

    #[test]
    fn place_result_native_grows_the_canvas_to_fit() {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 64, "height": 64, "background": "white"})).unwrap();
        let r = place_result_native(&mut s, &solid_png(128, 96), None).unwrap();
        let d = s.active().unwrap();
        assert_eq!((d.doc.size.width, d.doc.size.height), (128, 96));
        assert_eq!(r["canvas"], json!([128, 96]));
        let layer = d.doc.layer(d.active_layer.unwrap()).unwrap();
        assert_eq!(layer.name, "ComfyUI Result");
        let LayerContent::Raster(px) = &layer.content else { panic!("not a raster layer") };
        assert_eq!(px.content_bounds(), Rect::new(0, 0, 128, 96));
    }

    #[test]
    fn place_result_native_keeps_a_larger_canvas() {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 64, "height": 64, "background": "white"})).unwrap();
        place_result_native(&mut s, &solid_png(32, 32), None).unwrap();
        let d = s.active().unwrap();
        assert_eq!((d.doc.size.width, d.doc.size.height), (64, 64));
        let layer = d.doc.layer(d.active_layer.unwrap()).unwrap();
        let LayerContent::Raster(px) = &layer.content else { panic!("not a raster layer") };
        assert_eq!(px.content_bounds(), Rect::new(16, 16, 48, 48)); // centred, 1:1
    }

    #[test]
    fn shelf_persists_restores_and_removes() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::new();
        s.comfy.generations_dir = Some(dir.path().to_path_buf());
        push_result(&mut s, StoredResult { bytes: b"one".to_vec(), seed: 7, prompt: "p1".into(), workflow: "w1".into(), selection: None, created_ms: 100, file: None });
        push_result(&mut s, StoredResult { bytes: b"two".to_vec(), seed: 8, prompt: "p2".into(), workflow: "w2".into(), selection: None, created_ms: 200, file: None });
        assert_eq!(s.comfy.results.len(), 2);
        assert_eq!(pngs_in(dir.path()).len(), 2);
        assert!(dir.path().join("index.json").exists());
        assert!(s.comfy.results.iter().all(|r| r.file.is_some()));
        // Removing the oldest drops its file and rewrites the index.
        let r = remove_results(&mut s, &json!({"result": 0})).unwrap();
        assert_eq!(r["removed"], json!(1));
        assert_eq!(pngs_in(dir.path()).len(), 1);
        let index: Value = serde_json::from_slice(&std::fs::read(dir.path().join("index.json")).unwrap()).unwrap();
        assert_eq!(index.as_array().map(Vec::len), Some(1));
        // A fresh session restores the survivor with its metadata (but no selection snapshot).
        let mut s2 = Session::new();
        s2.comfy.generations_dir = Some(dir.path().to_path_buf());
        s2.comfy.restore_results();
        assert_eq!(s2.comfy.results.len(), 1);
        assert_eq!(s2.comfy.results[0].bytes, b"two".to_vec());
        assert_eq!(s2.comfy.results[0].prompt, "p2");
        assert_eq!(s2.comfy.results[0].created_ms, 200);
        assert!(s2.comfy.results[0].selection.is_none());
        // Clearing the shelf empties the folder too.
        remove_results(&mut s2, &json!({"all": true})).unwrap();
        assert!(s2.comfy.results.is_empty());
        assert!(pngs_in(dir.path()).is_empty());
        let index: Value = serde_json::from_slice(&std::fs::read(dir.path().join("index.json")).unwrap()).unwrap();
        assert_eq!(index.as_array().map(Vec::len), Some(0));
    }

    #[test]
    fn generation_index_rejects_escape_names() {
        assert!(safe_generation_name("100-7.png"));
        assert!(safe_generation_name("100-7-1.png"));
        assert!(!safe_generation_name("../secret.png"));
        assert!(!safe_generation_name("notes.txt"));
        assert!(!safe_generation_name("7.png"));
        assert!(!safe_generation_name(""));
    }

    // ------------------------------------------------------------ Image API workflow

    fn api_session() -> Session {
        let mut s = doc_with_selection();
        s.comfy.image_api = true;
        s.prefs.edit(|p| {
            p.integrations.image_api_base = "https://api.example.com".into();
            p.integrations.image_api_key = "sk-test".into();
            p.integrations.image_api_model = "gpt-image-2".into();
        });
        s
    }

    #[test]
    fn image_api_workflow_lists_and_analyzes_only_with_the_api_transport() {
        let names = |s: &mut Session| {
            s.execute("ai.comfyui.workflows.list", json!({})).unwrap()["workflows"]
                .as_array()
                .map(|a| a.iter().map(|w| w["name"].as_str().unwrap_or("").to_string()).collect::<Vec<_>>())
                .unwrap_or_default()
        };
        let mut s = doc_with_selection();
        let plain = names(&mut s);
        assert!(plain.contains(&"FLUX.3 Klein Image Edit".to_string()));
        assert!(!plain.contains(&BUILTIN_IMAGE_API.to_string()), "no API transport, no API workflow");
        // The built-in is removable neither way.
        assert!(s.execute("ai.comfyui.workflows.remove", json!({"name": BUILTIN_IMAGE_API})).is_err());
        let mut s = api_session();
        assert!(names(&mut s).contains(&BUILTIN_IMAGE_API.to_string()));
        let listed = s.execute("ai.comfyui.workflows.list", json!({})).unwrap();
        let api = listed["workflows"].as_array().unwrap().iter().find(|w| w["name"] == BUILTIN_IMAGE_API).cloned().unwrap();
        assert_eq!(api["builtin"], json!(true));
        assert_eq!(api["analyze"]["api"], json!(true));
        assert_eq!(api["analyze"]["promptNodes"], json!(["1"]));
        // A plain ComfyUI graph keeps its plain analysis.
        let mut wf = builtin_workflows().into_iter().next().unwrap().1;
        assert_eq!(analyze(&wf)["api"], json!(false));
        wf = builtin_api_workflow();
        assert!(is_api_workflow(&wf) && comfy_cmds::valid_workflow(&wf));
    }

    #[test]
    fn fill_routes_the_image_api_workflow_to_one_direct_call() {
        let mut s = api_session();
        CALLS.with(|c| c.borrow_mut().clear());
        let r = s.execute("ai.comfyui.fill", json!({"workflow": BUILTIN_IMAGE_API, "prompt": "a red dress", "place": "none"})).unwrap();
        // One image straight onto the shelf, with the API's bookkeeping.
        assert_eq!(s.comfy.results.len(), 1);
        assert_eq!(s.comfy.results[0].prompt, "a red dress");
        assert_eq!(s.comfy.results[0].workflow, BUILTIN_IMAGE_API);
        assert!(r["promptId"].as_str().is_some_and(|id| id.starts_with("api-")), "{}", r);
        assert_eq!(r["injected"]["promptNodes"], json!([]));
        // Exactly one Image API call, and none of the ComfyUI protocol around it.
        let calls = API_CALLS.with(|c| c.borrow().clone());
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0]["base"], json!("https://api.example.com/v1"));
        assert_eq!(calls[0]["model"], json!("gpt-image-2"));
        assert_eq!(calls[0]["prompt"], json!("a red dress"));
        assert_eq!(calls[0]["url"], json!("https://api.example.com/v1"));
        assert!(calls[0]["image"].as_u64().is_some_and(|n| n > 0), "the source png went along");
        assert!(CALLS.with(|c| c.borrow().is_empty()), "no upload or queue happened");
        // The result places like any other (the same shelf).
        let p = s.execute("ai.comfyui.place", json!({"result": 0, "as": "layer"})).unwrap();
        assert!(p.get("layer").is_some());
    }

    #[test]
    fn fill_image_api_validates_settings_and_prompt_before_any_call() {
        let cases: Vec<(Value, &str)> = vec![
            (json!({"workflow": BUILTIN_IMAGE_API, "prompt": "p"}), "Image API Endpoint"),
            // A prompt is required: no workflow could carry a baked-in one.
            (json!({"workflow": BUILTIN_IMAGE_API}), "needs a prompt"),
        ];
        let mut s = api_session();
        s.prefs.edit(|p| p.integrations.image_api_base.clear());
        for (p, want) in &cases {
            let e = s.execute("ai.comfyui.fill", p.clone()).unwrap_err().to_string();
            assert!(e.contains(want), "{e}");
        }
        s.prefs.edit(|p| p.integrations.image_api_base = "https://api.example.com".into());
        let e = s.execute("ai.comfyui.fill", json!({"workflow": BUILTIN_IMAGE_API})).unwrap_err().to_string();
        assert!(e.contains("needs a prompt"), "{e}");
        s.prefs.edit(|p| p.integrations.image_api_model.clear());
        let e = s.execute("ai.comfyui.fill", json!({"workflow": BUILTIN_IMAGE_API, "prompt": "p"})).unwrap_err().to_string();
        assert!(e.contains("Image API Model"), "{e}");
        s.prefs.edit(|p| p.integrations.image_api_model = "gpt-image-2".into());
        s.prefs.edit(|p| p.integrations.image_api_base = "ftp://nope".into());
        let e = s.execute("ai.comfyui.fill", json!({"workflow": BUILTIN_IMAGE_API, "prompt": "p"})).unwrap_err().to_string();
        assert!(e.contains("not an http(s) address"), "{e}");
        // Without the flag the workflow is not loadable at all (the desktop has its own built-ins).
        let mut desktop = doc_with_selection();
        assert!(desktop.execute("ai.comfyui.fill", json!({"workflow": BUILTIN_IMAGE_API, "prompt": "p"})).is_err());
        assert!(API_CALLS.with(|c| c.borrow().is_empty()));
    }

    #[test]
    fn check_reports_the_image_api_and_whether_the_model_is_offered() {
        let mut s = api_session();
        let r = s.execute("ai.comfyui.check", json!({})).unwrap();
        assert_eq!(r["api"], json!(true));
        assert_eq!(r["model"], json!("gpt-image-2"));
        assert_eq!(r["modelFound"], json!(true), "the mock lists gpt-image-2");
        s.prefs.edit(|p| p.integrations.image_api_model = "not-offered".into());
        let r = s.execute("ai.comfyui.check", json!({})).unwrap();
        assert_eq!(r["modelFound"], json!(false));
        // An unset endpoint says what to set, and the ComfyUI branch is untouched without the flag.
        let mut s = doc_with_selection();
        assert_eq!(s.execute("ai.comfyui.check", json!({})).unwrap()["api"], json!(null));
    }
}
