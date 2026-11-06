//! File › Generate Image: local AI image generation through a ComfyUI server.
//!
//! [ComfyUI](https://www.comfy.org/) runs diffusion models on the user's own machine and serves a
//! small HTTP API on loopback. The engine never does network I/O (the same rule as the release
//! check, `updates_cmds`): the desktop shell injects a blocking HTTP transport ([`ComfyFn`]) and
//! these commands drive it — queue a workflow, wait for the server, then pin the generated image
//! onto the canvas as a new layer (a new document when none is open). Sandboxed WASM plug-ins
//! cannot do this by design (the sandbox has no network), so the bridge lives here as host
//! commands, drivable from the menu, shortcuts, CLI, control channel and MCP like any other.
//!
//! Which graph to build is decided from what the server actually has (`/object_info`): a classic
//! checkpoint (SD 1.5 / SD XL) when `CheckpointLoaderSimple` lists one, else a UNET-style
//! Qwen-Image or FLUX pipeline (the GGUF layouts ComfyUI Desktop installs on Apple silicon). The
//! built-in graphs cover text→image and image→image (`"mode": "image"` uploads the visible
//! document). `workflow` overrides everything with a complete graph in ComfyUI's API format
//! (ComfyUI › Workflow › Export (API)) for anything else; a custom graph's output images are
//! placed the same way.

use std::sync::Arc;

use photocraft_doc::Document;
use serde_json::{Value, json};

use super::params::bad;
use crate::commands::CommandSpec;
use crate::jobs::{self, JobCtx};
use crate::{EngineError, Result, Session};

/// Default ComfyUI address (Preferences ▸ Integrations ▸ ComfyUI Server URL overrides it).
pub const DEFAULT_URL: &str = "http://127.0.0.1:8188";

// ------------------------------------------------------------------ transport

/// One HTTP call the bridge makes; the shell fills it in (the engine owns no HTTP client).
pub enum ComfyCall {
    /// GET /system_stats — is the server up, and what is it running on.
    SystemStats,
    /// GET /object_info — every node with its model lists (the per-node endpoint returns `{}` on
    /// current servers, so the whole document is read at once).
    ObjectInfo,
    /// POST /prompt — queue a workflow (API format).
    Queue { workflow: Value, client_id: String },
    /// POST /interrupt — stop the running generation (best effort, on cancel).
    Interrupt,
    /// GET /history/&lt;prompt_id&gt; — the outputs once the prompt has run.
    History { prompt_id: String },
    /// GET /view — download one output image.
    Download { filename: String, subfolder: String, kind: String },
    /// POST /upload/image — an img2img source image.
    Upload { name: String, bytes: Vec<u8> },
}

/// A request: the base URL (Preferences ▸ Integrations) plus the call.
pub struct ComfyRequest {
    pub url: String,
    pub call: ComfyCall,
}

/// A call's reply: JSON, or raw image bytes.
pub enum ComfyReply {
    Json(Value),
    Bytes(Vec<u8>),
}

/// Blocking transport injected by the desktop app; `None` on the web and in headless sessions.
/// Called once per request from a worker thread, so a slow call never blocks the UI.
pub type ComfyFn = Arc<dyn Fn(&ComfyRequest) -> std::result::Result<ComfyReply, String> + Send + Sync>;

/// Session state for the bridge.
#[derive(Default)]
pub struct Comfy {
    /// Set by the desktop shell at startup (`Services::comfy_transport`).
    pub transport: Option<ComfyFn>,
    /// Where `ai.comfyui.workflows.*` keeps imported workflows: the platform config directory's
    /// `comfy_workflows` folder, set by the desktop shell (`None` on the web and headless).
    pub workflows_dir: Option<std::path::PathBuf>,
    /// The results of recent `ai.comfyui.fill` runs (the panel's gallery): each can be placed as
    /// a layer or saved to a file after the fact. Bounded by [`RESULT_LIMIT`]/[`RESULT_BYTES_MAX`].
    pub results: Vec<crate::cmd::comfy_workflow_cmds::StoredResult>,
}

// ------------------------------------------------------------------ parameters

/// The fixed choices the built-in workflows offer (samplers every ComfyUI ships).
pub const SAMPLERS: &[&str] = &["euler", "euler_ancestral", "dpmpp_2m", "dpmpp_2m_sde", "ddim", "uni_pc"];
pub const SCHEDULERS: &[&str] =
    &["normal", "karras", "exponential", "sgm_uniform", "simple", "beta", "ddim_uniform"];

/// Prompt length cap: diffusion prompts are a few hundred words; megabyte strings are hostile.
const PROMPT_MAX: usize = 8_000;
const NAME_MAX: usize = 500;
/// A `workflow` override must stay a small graph.
const WORKFLOW_MAX_BYTES: usize = 1_000_000;
const WORKFLOW_MAX_NODES: usize = 512;
/// One generated image at most 512 MiB, 4 GiB overall (a `batch`, or a graph with many outputs).
/// `u64`: the overall cap does not fit a wasm32 `usize`.
const IMAGE_MAX: u64 = 512 * 1024 * 1024;
const IMAGES_MAX: u64 = 4 * 1024 * 1024 * 1024;
/// How often to poll `/history` while waiting for the server.
const POLL_MS: u64 = 750;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Text→image: sample from an empty latent.
    Text,
    /// Image→image: upload the visible document and sample from it (`denoise` steers it).
    Image,
}

/// The validated parameters of `ai.comfyui.generate`. Absent numbers stay `None` so the workflow
/// can default them per model family (a 4-step FLUX schnell and a 20-step SD XL want different
/// values); explicit values always win.
#[derive(Clone, Debug, PartialEq)]
pub struct GenParams {
    pub prompt: String,
    pub negative: String,
    pub mode: Mode,
    /// A classic checkpoint, or a UNET (either list): empty picks automatically.
    pub checkpoint: String,
    /// The UNET-style model, when the server has no checkpoints (empty: automatic).
    pub unet: String,
    pub clip: String,
    pub vae: String,
    /// Rounded to a multiple of 8 here; model families with a 16×16 latent re-snap when the
    /// workflow is built.
    pub width: u32,
    pub height: u32,
    pub batch: u32,
    pub steps: Option<u32>,
    pub cfg: Option<f32>,
    pub denoise: f32,
    /// None: pick one at random.
    pub seed: Option<u64>,
    pub sampler: Option<String>,
    pub scheduler: Option<String>,
    /// The timestep shift of Qwen-Image-style samplers (ModelSamplingAuraFlow).
    pub shift: f32,
    /// Give up waiting after this many seconds.
    pub timeout_s: u64,
    /// A complete ComfyUI API-format graph, used verbatim instead of the built-in workflow.
    pub workflow: Option<Value>,
}

/// A JSON number that also accepts numeric strings (dialog fields send both); finite only.
fn num(p: &Value, key: &str) -> Option<f64> {
    p.get(key).and_then(|v| {
        v.as_f64()
            .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
            .filter(|f| f.is_finite())
    })
}

fn clamped(p: &Value, key: &str, lo: f64, hi: f64) -> Option<f64> {
    num(p, key).map(|v| v.clamp(lo, hi))
}

pub(crate) fn text<'a>(p: &'a Value, key: &str, cmd: &str) -> Result<&'a str> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(""),
        Some(v) => v.as_str().ok_or_else(|| bad(cmd, format!("`{key}` must be a string"))),
    }
}

pub(crate) fn name_param(p: &Value, key: &str, cmd: &str) -> Result<String> {
    let v = text(p, key, cmd)?.trim().to_string();
    if v.len() > NAME_MAX {
        return Err(bad(cmd, format!("`{key}` is longer than {NAME_MAX} characters")));
    }
    Ok(v)
}

/// A workflow override must be an API-format graph: an object of nodes with a `class_type`.
pub(crate) fn valid_workflow(v: &Value) -> bool {
    let Some(o) = v.as_object() else { return false };
    !o.is_empty()
        && o.len() <= WORKFLOW_MAX_NODES
        && o.values().all(|n| n.as_object().is_some_and(|n| n.get("class_type").and_then(Value::as_str).is_some_and(|t| !t.is_empty())))
}

impl GenParams {
    pub fn parse(cmd: &str, p: &Value) -> Result<Self> {
        let prompt = text(p, "prompt", cmd)?;
        if prompt.len() > PROMPT_MAX {
            return Err(bad(cmd, format!("`prompt` is longer than {PROMPT_MAX} characters")));
        }
        let negative = text(p, "negative", cmd)?;
        if negative.len() > PROMPT_MAX {
            return Err(bad(cmd, format!("`negative` is longer than {PROMPT_MAX} characters")));
        }
        let mode = match p.get("mode") {
            None | Some(Value::Null) => Mode::Text,
            Some(Value::String(s)) if s == "text" => Mode::Text,
            Some(Value::String(s)) if s == "image" => Mode::Image,
            Some(v) => return Err(bad(cmd, format!("`mode` must be \"text\" or \"image\", not {v}"))),
        };
        let seed = match num(p, "seed") {
            None => None,
            // Negative (the dialog's default -1) means "pick one at random"; huge values saturate.
            Some(s) if s < 0.0 => None,
            Some(s) => Some(s.round() as u64),
        };
        let sampler = text(p, "sampler", cmd)?;
        let sampler = if sampler.is_empty() { None } else { Some(sampler.to_string()) };
        if let Some(s) = &sampler
            && !SAMPLERS.contains(&s.as_str())
        {
            return Err(bad(cmd, format!("`sampler` must be one of {}", SAMPLERS.join(", "))));
        }
        let scheduler = text(p, "scheduler", cmd)?;
        let scheduler = if scheduler.is_empty() { None } else { Some(scheduler.to_string()) };
        if let Some(s) = &scheduler
            && !SCHEDULERS.contains(&s.as_str())
        {
            return Err(bad(cmd, format!("`scheduler` must be one of {}", SCHEDULERS.join(", "))));
        }
        let workflow = match p.get("workflow") {
            None | Some(Value::Null) => None,
            Some(v) if valid_workflow(v) => {
                if serde_json::to_vec(v).map_or(true, |b| b.len() > WORKFLOW_MAX_BYTES) {
                    return Err(bad(cmd, format!("`workflow` is larger than {WORKFLOW_MAX_BYTES} bytes")));
                }
                Some(v.clone())
            }
            Some(v) => {
                return Err(bad(cmd, format!("`workflow` must be a ComfyUI API-format graph (an object of nodes with a class_type), not {v}")));
            }
        };
        // Multiples of 8 the common latent grid needs (64..=8192).
        let snap = |v: f64| ((v.clamp(64.0, 8192.0) as u32) / 8) * 8;
        Ok(GenParams {
            prompt: prompt.to_string(),
            negative: negative.to_string(),
            mode,
            checkpoint: name_param(p, "checkpoint", cmd)?,
            unet: name_param(p, "unet", cmd)?,
            clip: name_param(p, "clip", cmd)?,
            vae: name_param(p, "vae", cmd)?,
            width: snap(clamped(p, "width", 64.0, 8192.0).unwrap_or(1024.0)),
            height: snap(clamped(p, "height", 64.0, 8192.0).unwrap_or(1024.0)),
            batch: clamped(p, "batch", 1.0, 4.0).unwrap_or(1.0) as u32,
            steps: clamped(p, "steps", 1.0, 150.0).map(|v| v as u32),
            cfg: clamped(p, "cfg", 0.0, 30.0).map(|v| v as f32),
            // Text→image always samples fully; image→image defaults to 0.55.
            denoise: clamped(p, "denoise", 0.01, 1.0).unwrap_or(if mode == Mode::Image { 0.55 } else { 1.0 }) as f32,
            seed,
            sampler,
            scheduler,
            shift: clamped(p, "shift", 0.0, 10.0).unwrap_or(3.1) as f32,
            timeout_s: clamped(p, "timeout", 10.0, 3600.0).unwrap_or(600.0) as u64,
            workflow,
        })
    }
}

// ------------------------------------------------------------------ server inventory

/// The models a server offers, from `/object_info` (the model lists of the loader nodes).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ServerModels {
    pub checkpoints: Vec<String>,
    /// `UNETLoader` (safetensors).
    pub unets: Vec<String>,
    /// `UnetLoaderGGUF` (quantized, lighter to load — preferred on Apple silicon).
    pub unets_gguf: Vec<String>,
    pub clips: Vec<String>,
    pub vaes: Vec<String>,
}

impl ServerModels {
    /// UNET-style models, GGUF first (the lighter load), then safetensors.
    pub fn all_unets(&self) -> Vec<String> {
        self.unets_gguf.iter().chain(self.unets.iter()).cloned().collect()
    }

    pub fn to_json(&self) -> Value {
        json!({
            "checkpoints": self.checkpoints,
            "unets": self.all_unets(),
            "clips": self.clips,
            "vaes": self.vaes,
            "counts": {"checkpoints": self.checkpoints.len(), "unets": self.unets.len() + self.unets_gguf.len(), "clips": self.clips.len(), "vaes": self.vaes.len()},
        })
    }
}

fn model_list(object_info: &Value, node: &str, input: &str) -> Vec<String> {
    object_info
        .pointer(&format!("/{node}/input/required/{input}/0"))
        .or_else(|| object_info.pointer(&format!("/{node}/input/optional/{input}/0")))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .collect()
}

/// The loader inventories out of a full `/object_info` reply.
pub fn parse_object_info(object_info: &Value) -> ServerModels {
    ServerModels {
        checkpoints: model_list(object_info, "CheckpointLoaderSimple", "ckpt_name"),
        unets: model_list(object_info, "UNETLoader", "unet_name"),
        unets_gguf: model_list(object_info, "UnetLoaderGGUF", "unet_name"),
        clips: model_list(object_info, "CLIPLoader", "clip_name"),
        vaes: model_list(object_info, "VAELoader", "vae_name"),
    }
}

// ------------------------------------------------------------------ workflow picking

/// The pipeline shape to build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    /// SD 1.5 / SD XL: one checkpoint carries model, CLIP and VAE.
    Classic,
    /// Qwen-Image: UNET + Qwen text encoder (CLIPLoader) + VAE, AuraFlow shift, 16×16 latent.
    Qwen,
    /// FLUX schnell/dev: UNET + dual CLIP (T5 + clip_l) + VAE, 16×16 latent.
    Flux,
}

impl Family {
    /// The latent grid the family's VAE works in (image sizes are snapped to it).
    pub fn latent_multiple(self) -> u32 {
        match self {
            Family::Classic => 8,
            Family::Qwen | Family::Flux => 16,
        }
    }

    /// Steps and CFG a first good result wants, when the params left them open.
    pub fn defaults(self) -> (u32, f32, &'static str, &'static str) {
        match self {
            Family::Classic => (20, 7.0, "euler", "normal"),
            Family::Qwen => (8, 2.2, "euler", "simple"),
            Family::Flux => (4, 1.0, "euler", "simple"),
        }
    }
}

/// Which named models a workflow runs on.
#[derive(Clone, Debug, PartialEq)]
pub struct Picked {
    pub family: Family,
    /// A checkpoint (Classic) or a UNET name (Qwen/Flux), as the server lists it.
    pub model: String,
    /// True when `model` came from the GGUF list (different loader node).
    pub gguf: bool,
    pub clip: String,
    /// The Qwen text encoder or the clip_l half of the FLUX dual loader.
    pub clip2: String,
    pub vae: String,
}

/// Choose the pipeline: explicit names win, else the first classic checkpoint, else the first
/// usable UNET layout. The error lists what the server has, so it is actionable.
pub fn pick(params: &GenParams, models: &ServerModels) -> Result<Picked> {
    let list = |names: &[String], what: &str| {
        if names.is_empty() {
            format!("no {what}")
        } else {
            format!("{what}: {}", names.iter().take(4).cloned().collect::<Vec<_>>().join(", "))
        }
    };
    let nothing = || {
        EngineError::Other(format!(
            "no usable model: {} — {} — {} — {}; install a checkpoint or UNET in ComfyUI, or pass `workflow` with your own graph",
            list(&models.checkpoints, "checkpoints"),
            list(&models.all_unets(), "UNET models"),
            list(&models.clips, "text encoders"),
            list(&models.vaes, "VAEs"),
        ))
    };
    // UNET families the built-in graphs know; anything else needs a custom `workflow`.
    let family_of = |model: &str| match model.to_lowercase() {
        m if m.contains("qwen") => Some(Family::Qwen),
        m if m.contains("flux") => Some(Family::Flux),
        _ => None,
    };
    // An explicit checkpoint names either list.
    if !params.checkpoint.is_empty() {
        if let Some(found) = models.checkpoints.iter().find(|n| *n == &params.checkpoint) {
            return Ok(Picked { family: Family::Classic, model: found.clone(), gguf: false, clip: String::new(), clip2: String::new(), vae: String::new() });
        }
        if let Some(found) = models.all_unets().iter().find(|n| *n == &params.checkpoint) {
            let gguf = models.unets_gguf.contains(found);
            let family = family_of(found).ok_or_else(|| unsupported(found))?;
            return finish_unet(params, models, found.clone(), gguf, family).ok_or_else(nothing);
        }
        return Err(nothing());
    }
    if !params.unet.is_empty() {
        if let Some(found) = models.all_unets().iter().find(|n| *n == &params.unet) {
            let gguf = models.unets_gguf.contains(found);
            let family = family_of(found).ok_or_else(|| unsupported(found))?;
            return finish_unet(params, models, found.clone(), gguf, family).ok_or_else(nothing);
        }
        return Err(nothing());
    }
    // Automatic: a classic checkpoint first, then Qwen, then FLUX (GGUF before safetensors).
    if let Some(found) = models.checkpoints.first() {
        return Ok(Picked { family: Family::Classic, model: found.clone(), gguf: false, clip: String::new(), clip2: String::new(), vae: String::new() });
    }
    // Image-edit models need a much more specific graph (masks, VAE Encode for Inpaint); the
    // built-in ones run the generation models only.
    let skip = ["edit", "inpaint", "i2v", "video", "wan", "hunyuan_video", "ltxv", "mochi", "cosmos"];
    for needle in ["qwen", "flux"] {
        for model in models.all_unets() {
            let lower = model.to_lowercase();
            if !lower.contains(needle) || skip.iter().any(|s| lower.contains(s)) {
                continue;
            }
            let gguf = models.unets_gguf.contains(&model);
            if let Some(family) = family_of(&model)
                && let Some(p) = finish_unet(params, models, model.clone(), gguf, family)
            {
                return Ok(p);
            }
        }
    }
    Err(nothing())
}

/// An explicit UNET-model family the built-in graphs cannot drive.
fn unsupported(model: &str) -> EngineError {
    EngineError::Other(format!(
        "`{model}` has no built-in workflow (only Qwen-Image and FLUX UNETs do); export your graph from ComfyUI (Workflow › Export (API)) and pass it as `workflow`"
    ))
}

/// Fill in the text encoder(s) and VAE a UNET-style model needs; `None` when they aren't there.
fn finish_unet(params: &GenParams, models: &ServerModels, model: String, gguf: bool, family: Family) -> Option<Picked> {
    match family {
        Family::Flux => {
            // FLUX needs the T5 half (files start with "t5" — umt5 belongs to other models);
            // clip_l doubles as its small half.
            let t5 = models.clips.iter().find(|n| n.to_lowercase().starts_with("t5"))?;
            let small = models.clips.iter().find(|n| n.to_lowercase().contains("clip_l")).unwrap_or(t5);
            let vae = models.vaes.iter().find(|n| n.to_lowercase().contains("ae")).or_else(|| models.vaes.first())?;
            Some(Picked { family, model, gguf, clip: small.clone(), clip2: t5.clone(), vae: vae.clone() })
        }
        Family::Qwen => {
            let clip = if params.clip.is_empty() {
                models.clips.iter().find(|n| n.to_lowercase().contains("qwen"))?.clone()
            } else {
                models.clips.iter().find(|n| *n == &params.clip)?.clone()
            };
            let vae = if params.vae.is_empty() {
                models.vaes.iter().find(|n| n.to_lowercase().contains("qwen")).or_else(|| models.vaes.first())?.clone()
            } else {
                models.vaes.iter().find(|n| *n == &params.vae)?.clone()
            };
            Some(Picked { family, model, gguf, clip, clip2: String::new(), vae })
        }
        Family::Classic => None,
    }
}

// ------------------------------------------------------------------ workflows

/// Where the sampler's latent comes from.
pub enum Latent {
    /// `EmptyLatentImage` / `EmptySD3LatentImage` of this size.
    Empty { width: u32, height: u32 },
    /// The uploaded image, scaled to this size and VAE-encoded.
    Uploaded { name: String, width: u32, height: u32 },
}

fn snap_to(value: u32, multiple: u32) -> u32 {
    value - value % multiple
}

/// The graph for `picked`: loaders by family, text encodes, latent source, KSampler, save. Steps,
/// CFG, sampler and scheduler default per family when the params left them open.
pub fn workflow(picked: &Picked, p: &GenParams, seed: u64, latent: Latent) -> Value {
    let (default_steps, default_cfg, default_sampler, default_scheduler) = picked.family.defaults();
    let steps = p.steps.unwrap_or(default_steps);
    let cfg = p.cfg.unwrap_or(default_cfg);
    let sampler = p.sampler.as_deref().unwrap_or(default_sampler);
    let scheduler = p.scheduler.as_deref().unwrap_or(default_scheduler);
    let multiple = picked.family.latent_multiple();
    let (latent_width, latent_height) = match &latent {
        Latent::Empty { width, height } => (snap_to(*width, multiple), snap_to(*height, multiple)),
        Latent::Uploaded { width, height, .. } => (snap_to(*width, multiple), snap_to(*height, multiple)),
    };
    let mut graph = serde_json::Map::new();
    // The model chain ("10" is always the sampler's MODEL).
    match picked.family {
        Family::Classic => {
            graph.insert("10".into(), json!({"class_type": "CheckpointLoaderSimple", "inputs": {"ckpt_name": picked.model}}));
        }
        Family::Qwen => {
            let loader = if picked.gguf {
                json!({"class_type": "UnetLoaderGGUF", "inputs": {"unet_name": picked.model}})
            } else {
                json!({"class_type": "UNETLoader", "inputs": {"unet_name": picked.model, "weight_dtype": "default"}})
            };
            graph.insert("11".into(), loader);
            graph.insert(
                "10".into(),
                json!({"class_type": "ModelSamplingAuraFlow", "inputs": {"model": ["11", 0], "shift": p.shift}}),
            );
        }
        Family::Flux => {
            let loader = if picked.gguf {
                json!({"class_type": "UnetLoaderGGUF", "inputs": {"unet_name": picked.model}})
            } else {
                json!({"class_type": "UNETLoader", "inputs": {"unet_name": picked.model, "weight_dtype": "default"}})
            };
            graph.insert("11".into(), loader);
            graph.insert("10".into(), json!({"class_type": "ModelSamplingFlux", "inputs": {
                "model": ["11", 0], "max_shift": 1.15, "base_shift": 0.5, "width": latent_width, "height": latent_height}}));
        }
    }
    // The conditioning chain ("20" positive, "30" negative; CLIP output slot 1 on Classic).
    let clip = match picked.family {
        Family::Classic => json!(["10", 1]),
        Family::Qwen => {
            graph.insert(
                "12".into(),
                json!({"class_type": "CLIPLoader", "inputs": {"clip_name": picked.clip, "type": "qwen_image", "device": "default"}}),
            );
            json!(["12", 0])
        }
        Family::Flux => {
            graph.insert(
                "12".into(),
                json!({"class_type": "DualCLIPLoader", "inputs": {"clip_name1": picked.clip, "clip_name2": picked.clip2, "type": "flux"}}),
            );
            json!(["12", 0])
        }
    };
    graph.insert("20".into(), json!({"class_type": "CLIPTextEncode", "inputs": {"text": p.prompt, "clip": clip}}));
    graph.insert("30".into(), json!({"class_type": "CLIPTextEncode", "inputs": {"text": p.negative, "clip": clip}}));
    // The VAE ("40" decodes; slot 2 on Classic).
    let vae = match picked.family {
        Family::Classic => json!(["10", 2]),
        _ => {
            graph.insert("13".into(), json!({"class_type": "VAELoader", "inputs": {"vae_name": picked.vae}}));
            json!(["13", 0])
        }
    };
    // The latent source ("50" feeds the sampler).
    match latent {
        Latent::Empty { .. } => {
            let class = match picked.family {
                Family::Classic => "EmptyLatentImage",
                Family::Qwen | Family::Flux => "EmptySD3LatentImage",
            };
            graph.insert("50".into(), json!({"class_type": class, "inputs": {"width": latent_width, "height": latent_height, "batch_size": p.batch}}));
        }
        Latent::Uploaded { name, .. } => {
            graph.insert("51".into(), json!({"class_type": "LoadImage", "inputs": {"image": name, "upload": "image"}}));
            graph.insert("52".into(), json!({"class_type": "ImageScale", "inputs": {
                "upscale_method": "bilinear", "width": latent_width, "height": latent_height, "crop": "disabled", "image": ["51", 0]}}));
            graph.insert("50".into(), json!({"class_type": "VAEEncode", "inputs": {"pixels": ["52", 0], "vae": vae}}));
        }
    }
    graph.insert("60".into(), json!({"class_type": "KSampler", "inputs": {
        "seed": seed, "steps": steps, "cfg": cfg, "sampler_name": sampler, "scheduler": scheduler,
        "denoise": if p.mode == Mode::Image { p.denoise } else { 1.0 },
        "model": ["10", 0], "positive": ["20", 0], "negative": ["30", 0], "latent_image": ["50", 0]}}));
    graph.insert("70".into(), json!({"class_type": "VAEDecode", "inputs": {"samples": ["60", 0], "vae": vae}}));
    graph.insert("80".into(), json!({"class_type": "SaveImage", "inputs": {"filename_prefix": "photocraft", "images": ["70", 0]}}));
    Value::Object(graph)
}

// ------------------------------------------------------------------ replies

/// `/upload/image`'s reply filed into the name LoadImage wants (`subfolder/name`).
pub(crate) fn uploaded_name(v: &Value) -> Option<String> {
    let name = v.get("name").and_then(Value::as_str)?;
    match v.get("subfolder").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        Some(sub) => Some(format!("{sub}/{name}")),
        None => Some(name.to_string()),
    }
}

/// The output images of a finished prompt from `/history/<id>`: every `SaveImage` output with
/// `type: "output"`. `None` while the prompt is still queued or running (no history entry).
pub fn history_outputs(history: &Value, prompt_id: &str) -> Option<Vec<Value>> {
    let outputs = history.get(prompt_id)?.get("outputs")?.as_object()?;
    let mut out = Vec::new();
    for node in outputs.values() {
        for image in node.get("images").and_then(Value::as_array).into_iter().flatten() {
            if image.get("type").and_then(Value::as_str) == Some("output") {
                out.push(image.clone());
            }
        }
    }
    Some(out)
}

/// Why a finished prompt produced nothing (`/history` entry with an error status).
pub fn history_error(history: &Value, prompt_id: &str) -> Option<String> {
    let entry = history.get(prompt_id)?;
    if entry.pointer("/status/status_str").and_then(Value::as_str) == Some("error") {
        return Some("ComfyUI reports the prompt failed (see ComfyUI's own window for which node errored)".into());
    }
    None
}

// ------------------------------------------------------------------ helpers

/// The transport, or why there is none (web, headless).
pub(crate) fn transport_of(s: &Session) -> Result<ComfyFn> {
    s.comfy.transport.clone().ok_or_else(|| {
        EngineError::Other("ComfyUI generation needs the desktop app: it talks to a local ComfyUI server over HTTP".into())
    })
}

/// The server base URL: the `url` param, else Preferences ▸ Integrations ▸ ComfyUI Server URL.
pub(crate) fn url_of(s: &Session, p: &Value) -> Result<String> {
    let url = match p.get("url") {
        Some(v) => v.as_str().ok_or_else(|| bad("ai.comfyui", "`url` must be a string"))?.trim().trim_end_matches('/').to_string(),
        None => s.prefs().integrations.comfy_url.trim().trim_end_matches('/').to_string(),
    };
    if url.is_empty() {
        Ok(DEFAULT_URL.to_string())
    } else if url.starts_with("http://") || url.starts_with("https://") {
        Ok(url)
    } else {
        Err(bad("ai.comfyui", format!("`{url}` is not an http(s) server address (like {DEFAULT_URL})")))
    }
}

/// Milliseconds since the epoch (0 if the clock is before it): upload names and client ids.
pub(crate) fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A varied seed from the clock (not crypto; it only has to differ between runs).
pub(crate) fn random_seed() -> u64 {
    let m = now_millis();
    m.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(17) ^ (m >> 32)
}

#[cfg(not(target_arch = "wasm32"))]
fn poll_sleep() {
    std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
}

// Unreachable in practice (the web has no transport), but the code compiles there too.
#[cfg(target_arch = "wasm32")]
fn poll_sleep() {}

/// Wrap a transport error with what we were doing, so the message stays actionable.
pub(crate) fn at(what: &str, e: String) -> EngineError {
    EngineError::Other(format!("{what}: {e}"))
}

/// Unwrap a reply's JSON, or explain why the bytes are not one.
pub(crate) fn json_of(r: ComfyReply) -> Result<Value> {
    match r {
        ComfyReply::Json(v) => Ok(v),
        ComfyReply::Bytes(b) => Err(EngineError::Other(format!("ComfyUI replied with {} bytes of an image where JSON was expected", b.len()))),
    }
}

/// Unwrap a reply's bytes, or explain why the JSON is not one.
pub(crate) fn bytes_of(r: ComfyReply) -> Result<Vec<u8>> {
    match r {
        ComfyReply::Bytes(b) => Ok(b),
        ComfyReply::Json(v) => Err(EngineError::Other(format!("ComfyUI replied with JSON where an image was expected: {v}"))),
    }
}

// ------------------------------------------------------------------ generate

/// What the worker thread hands back to the UI thread.
pub(crate) struct Generation {
    pub(crate) images: Vec<Vec<u8>>,
    pub(crate) model: String,
    pub(crate) family: &'static str,
    pub(crate) seed: u64,
    pub(crate) prompt_id: String,
    pub(crate) waited_ms: u64,
}

/// The generation, on the worker thread: read the server's models, build and queue the workflow,
/// wait for the server, download the outputs. `ctx` reports progress and carries cancellation (an
/// escape also asks the server to stop sampling).
fn run_generation(
    ctx: &JobCtx,
    transport: &ComfyFn,
    url: &str,
    p: &GenParams,
    doc: Option<&Document>,
    client_id: &str,
) -> Result<Generation> {
    let call = |c: ComfyCall| transport(&ComfyRequest { url: url.to_string(), call: c });
    ctx.progress(0.02, "Contacting ComfyUI…");
    let v = json_of(call(ComfyCall::ObjectInfo).map_err(|e| at("reading ComfyUI's models", e))?)?;
    let models = parse_object_info(&v);
    let seed = p.seed.unwrap_or_else(random_seed);
    let (workflow, model, family) = match &p.workflow {
        // A custom graph is the user's own; nothing of ours to substitute for it.
        Some(w) => (w.clone(), "custom workflow".to_string(), "custom"),
        None => {
            let picked = pick(p, &models).map_err(|e| at("choosing a model", e.to_string()))?;
            let multiple = picked.family.latent_multiple();
            let latent = match p.mode {
                Mode::Text => Latent::Empty { width: p.width, height: p.height },
                Mode::Image => {
                    let doc = doc.ok_or_else(|| bad("ai.comfyui.generate", "`mode` \"image\" needs an open document to send; use \"text\""))?;
                    let (w, h) = (snap_to(doc.size.width, multiple), snap_to(doc.size.height, multiple));
                    if w == 0 || h == 0 {
                        return Err(bad("ai.comfyui.generate", format!("the {} × {} document is smaller than one {multiple}×{multiple} latent tile", doc.size.width, doc.size.height)));
                    }
                    ctx.progress(0.08, "Uploading the document…");
                    let png = photocraft_io::export(doc, "photocraft.png", &photocraft_io::ExportOptions::default())
                        .map_err(|e| at("encoding the document as PNG", e.to_string()))?
                        .bytes;
                    let name = format!("photocraft-{}.png", now_millis());
                    let v = json_of(call(ComfyCall::Upload { name: name.clone(), bytes: png }).map_err(|e| at("uploading the source image", e))?)?;
                    let uploaded = uploaded_name(&v).unwrap_or(name);
                    Latent::Uploaded { name: uploaded, width: w, height: h }
                }
            };
            let model = picked.model.clone();
            let built = workflow(&picked, p, seed, latent);
            (built, model, family_name(picked.family))
        }
    };
    ctx.progress(0.05, &format!("Generating with {model}…"));
    queue_and_collect(ctx, &call, &workflow, client_id, &model, family, seed, p.timeout_s)
}

fn family_name(f: Family) -> &'static str {
    match f {
        Family::Classic => "checkpoint",
        Family::Qwen => "qwen",
        Family::Flux => "flux",
    }
}

/// Queue `workflow`, wait for it to finish (up to `timeout_s`), download the outputs.
pub(crate) fn queue_and_collect(
    ctx: &JobCtx,
    call: &impl Fn(ComfyCall) -> std::result::Result<ComfyReply, String>,
    workflow: &Value,
    client_id: &str,
    model: &str,
    family: &'static str,
    seed: u64,
    timeout_s: u64,
) -> Result<Generation> {
    let q = json_of(call(ComfyCall::Queue { workflow: workflow.clone(), client_id: client_id.to_string() }).map_err(|e| at("queueing the prompt", e))?)?;
    let prompt_id = q.get("prompt_id").and_then(Value::as_str);
    let prompt_id = prompt_id
        .filter(|id| !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
        .ok_or_else(|| EngineError::Other(format!("ComfyUI accepted no prompt (its reply was {q}")))?
        .to_string();
    // Wait for the server: poll history until the prompt has an entry with outputs.
    let limit_ms = timeout_s.saturating_mul(1000);
    let mut waited = 0u64;
    let mut total = 0u64;
    loop {
        if ctx.cancelled() {
            let _ = call(ComfyCall::Interrupt);
            return Err(EngineError::Cancelled);
        }
        let h = json_of(call(ComfyCall::History { prompt_id: prompt_id.clone() }).map_err(|e| at("checking ComfyUI's progress", e))?)?;
        if let Some(images) = history_outputs(&h, &prompt_id) {
            if images.is_empty() {
                return Err(match history_error(&h, &prompt_id) {
                    Some(msg) => EngineError::Other(msg),
                    None => EngineError::Other("ComfyUI finished the prompt without an output image".into()),
                });
            }
            ctx.progress(0.95, "Downloading…");
            let mut downloaded = Vec::with_capacity(images.len());
            for image in images {
                let get = |k: &str| image.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
                ctx.check()?;
                let bytes = bytes_of(call(ComfyCall::Download { filename: get("filename"), subfolder: get("subfolder"), kind: get("type") })
                    .map_err(|e| at("downloading the generated image", e))?)?;
                if bytes.is_empty() {
                    return Err(EngineError::Other("ComfyUI returned an empty image".into()));
                }
                if bytes.len() as u64 > IMAGE_MAX {
                    return Err(EngineError::Other(format!("the generated image is {} MiB, over the {} MiB limit", bytes.len() / 1_048_576, IMAGE_MAX / 1_048_576)));
                }
                total = total.saturating_add(bytes.len() as u64);
                if total > IMAGES_MAX {
                    return Err(EngineError::Other("the prompt produced more image data than the 4 GiB limit".into()));
                }
                downloaded.push(bytes);
            }
            ctx.progress(1.0, "Done");
            return Ok(Generation { images: downloaded, model: model.to_string(), family, seed, prompt_id, waited_ms: waited });
        }
        if waited >= limit_ms {
            let _ = call(ComfyCall::Interrupt);
            return Err(EngineError::Other(format!("ComfyUI did not finish within {} s (raise `timeout`, or check its window — one generation runs at a time)", limit_ms / 1000)));
        }
        poll_sleep();
        waited += POLL_MS;
        ctx.progress(0.05 + 0.9 * (waited as f32 / limit_ms.max(1) as f32).min(1.0), "ComfyUI is generating…");
    }
}

/// What the UI thread does with the images: pin each onto the canvas as a new layer, or make the
/// first a document of its own when none is open.
fn place_outputs(s: &mut Session, g: Generation) -> Result<Value> {
    let many = g.images.len() > 1;
    let mut placed = Vec::with_capacity(g.images.len());
    for (i, bytes) in g.images.into_iter().enumerate() {
        let name = if many { format!("ComfyUI Image {}.png", i + 1) } else { "ComfyUI Image.png".to_string() };
        let r = if s.active().is_none() {
            crate::cmd::file_cmds::open_bytes_as(s, &name, &bytes, None, None)?
        } else {
            crate::cmd::file_cmds::place_bytes(s, &name, bytes, None, &json!({"smartObject": false}))?
        };
        placed.push(r);
    }
    Ok(json!({
        "images": placed,
        "model": g.model,
        "family": g.family,
        "seed": g.seed,
        "promptId": g.prompt_id,
        "seconds": g.waited_ms / 1000,
    }))
}

fn generate(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "ai.comfyui.generate";
    let params = GenParams::parse(CMD, p)?;
    let transport = transport_of(s)?;
    let url = url_of(s, p)?;
    // img2img sends the visible document: clone the Arc now (cheap), the worker flattens it.
    let doc = match params.mode {
        Mode::Image => Some(s.active().ok_or_else(|| bad(CMD, "`mode` \"image\" needs an open document to send; use \"text\""))?.doc.clone()),
        Mode::Text => None,
    };
    let client_id = format!("photocraft-{:016x}", now_millis());
    jobs::run(
        s,
        "Generate Image",
        false,
        move |ctx| run_generation(ctx, &transport, &url, &params, doc.as_deref(), &client_id),
        place_outputs,
    )
}

// ------------------------------------------------------------------ queries

fn check(s: &mut Session, p: &Value) -> Result<Value> {
    if p.as_object().is_some_and(|o| !o.is_empty() && !o.contains_key("url")) {
        return Err(bad("ai.comfyui.check", "pass only {\"url\"}, or nothing"));
    }
    let transport = transport_of(s)?;
    let url = url_of(s, p)?;
    let v = json_of(transport(&ComfyRequest { url: url.clone(), call: ComfyCall::SystemStats })
        .map_err(|e| at(&format!("ComfyUI at {url} did not answer"), e))?)?;
    Ok(json!({"url": url, "server": v}))
}

fn models(s: &mut Session, p: &Value) -> Result<Value> {
    if p.as_object().is_some_and(|o| !o.is_empty() && !o.contains_key("url")) {
        return Err(bad("ai.comfyui.models", "pass only {\"url\"}, or nothing"));
    }
    let transport = transport_of(s)?;
    let url = url_of(s, p)?;
    let v = json_of(transport(&ComfyRequest { url: url.clone(), call: ComfyCall::ObjectInfo })
        .map_err(|e| at(&format!("asking {url} for its models"), e))?)?;
    let mut r = parse_object_info(&v).to_json();
    if let Some(o) = r.as_object_mut() {
        o.insert("url".into(), json!(url));
    }
    Ok(r)
}

pub(crate) fn transport_enabled(s: &Session) -> std::result::Result<(), String> {
    s.comfy.transport.as_ref().map(|_| ()).ok_or_else(|| "ComfyUI image generation needs the desktop app (it talks to a local ComfyUI server over HTTP)".into())
}

/// The ComfyUI bridge commands.
pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: "ai.comfyui.generate",
            label: "Generate Image (ComfyUI)…",
            menu: &["File"],
            shortcut: None,
            params: r#"{"prompt":text,"negative":text,"mode":"text|image"="text","checkpoint":text,"unet":text,"clip":text,"vae":text,"width":64..8192=1024,"height":64..8192=1024,"batch":1..4=1,"steps":1..150,"cfg":0..30,"denoise":0.01..1=0.55,"seed":int=-1,"sampler":"euler|euler_ancestral|dpmpp_2m|dpmpp_2m_sde|ddim|uni_pc","scheduler":"normal|karras|exponential|sgm_uniform|simple|beta|ddim_uniform","shift":0..10=3.1,"timeout":10..3600=600,"url":text,"workflow":json} (a local ComfyUI server generates the image; the pipeline is chosen from its models — a classic checkpoint, or a Qwen-Image or FLUX UNET layout; mode "image" uploads the visible document for img2img; empty model names pick automatically, ai.comfyui.models lists them; seed -1 is random; steps/cfg/sampler default per model family when left empty; workflow is a complete ComfyUI API-format graph overriding the built-in ones; the image lands as a new layer, or a new document when none is open)"#,
            enabled: transport_enabled,
            run: generate,
            journal: true,
        },
        CommandSpec {
            id: "ai.comfyui.check",
            label: "Check ComfyUI Connection",
            menu: &[],
            shortcut: None,
            params: r#"{"url":text} (empty: the preference)"#,
            enabled: transport_enabled,
            run: check,
            journal: false,
        },
        CommandSpec {
            id: "ai.comfyui.models",
            label: "List ComfyUI Models",
            menu: &[],
            shortcut: None,
            params: r#"{"url":text} (empty: the preference)"#,
            enabled: transport_enabled,
            run: models,
            journal: false,
        },
    ]
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;
    use photocraft_color::{ColorMode, PixelFormat, SampleType};
    use photocraft_doc::{Layer, LayerContent, Size};
    use photocraft_geom::Rect;
    use photocraft_raster::Surface;

    fn params(p: Value) -> Result<GenParams> {
        GenParams::parse("ai.comfyui.generate", &p)
    }

    /// This machine's layout: no checkpoints, GGUF + safetensors UNETs, Qwen encoders and VAEs.
    fn gguf_models() -> ServerModels {
        ServerModels {
            checkpoints: Vec::new(),
            unets: vec!["qwen_image_2.1_turbo_bf16.safetensors".into()],
            unets_gguf: vec![
                "Qwen-Image-Edit-2509-Q4_K_M.gguf".into(),
                "flux1-schnell-Q8_0.gguf".into(),
                "qwen-image-2.1-turbo-Q8_0.gguf".into(),
                "wan2.1-i2v-14b-480p-Q5_K_M.gguf".into(),
            ],
            clips: vec![
                "clip_l.safetensors".into(),
                "qwen_2.5_vl_7b_fp8_scaled.safetensors".into(),
                "umt5_xxl_fp8_e4m3fn_scaled.safetensors".into(),
            ],
            vaes: vec![
                "ae.safetensors".into(),
                "qwen_image_vae.safetensors".into(),
            ],
        }
    }

    fn classic_models() -> ServerModels {
        ServerModels {
            checkpoints: vec!["sd_xl_refiner.safetensors".into(), "a.safetensors".into()],
            ..ServerModels::default()
        }
    }

    #[test]
    fn defaults_clamp_and_snap() {
        let d = params(json!({})).unwrap();
        assert_eq!((d.mode, d.width, d.height, d.batch, d.seed, d.shift), (Mode::Text, 1024, 1024, 1, None, 3.1));
        assert_eq!((d.steps, d.cfg, d.sampler.as_deref(), d.scheduler.as_deref(), d.timeout_s), (None, None, None, None, 600));
        // Width snaps to the latent grid and clamps into 64..=8192.
        let d = params(json!({"width": 1000.6, "height": 9999, "steps": 500, "batch": 99, "cfg": 99.0})).unwrap();
        assert_eq!((d.width, d.height, d.batch, d.steps, d.cfg), (1000, 8192, 4, Some(150), Some(30.0)));
        // Numeric strings (dialog text fields) and floats both parse; junk stays at defaults.
        let d = params(json!({"width": "512", "seed": "42"})).unwrap();
        assert_eq!((d.width, d.seed), (512, Some(42)));
        // A negative seed (the dialog default -1) means random; a huge one saturates, not wraps.
        assert_eq!(params(json!({"seed": -1})).unwrap().seed, None);
        assert_eq!(params(json!({"seed": 1e300})).unwrap().seed, Some(u64::MAX));
    }

    #[test]
    fn hostile_params_error_never_panic() {
        for p in [
            json!({"prompt": 42}),
            json!({"prompt": "x".repeat(9_000)}),
            json!({"mode": "video"}),
            json!({"mode": []}),
            json!({"sampler": "evil"}),
            json!({"scheduler": "evil"}),
            json!({"checkpoint": "m".repeat(600)}),
            json!({"workflow": [1, 2]}),
            json!({"workflow": {"a": {"no_class": 1}}}),
            json!({"workflow": {}}),
            json!({"workflow": {"n": {"class_type": ""}}}),
            json!({"negative": 17}),
            json!({"unet": {"x": 1}}),
        ] {
            assert!(params(p).is_err());
        }
        // Clamped, not an error:
        let d = params(json!({"width": "not a number", "steps": 1e9})).unwrap();
        assert_eq!((d.width, d.steps), (1024, Some(150)));
    }

    #[test]
    fn picks_the_pipeline_from_what_the_server_has() {
        // Classic first when a checkpoint exists.
        let p = pick(&params(json!({})).unwrap(), &classic_models()).unwrap();
        assert_eq!((p.family, p.model.as_str()), (Family::Classic, "sd_xl_refiner.safetensors"));
        // A GGUF layout with no checkpoints: the Qwen model (not the edit/video ones), its own
        // encoder and VAE, GGUF preferred over the bf16 file.
        let p = pick(&params(json!({})).unwrap(), &gguf_models()).unwrap();
        assert_eq!((p.family, p.model.as_str(), p.gguf), (Family::Qwen, "qwen-image-2.1-turbo-Q8_0.gguf", true));
        assert_eq!(p.clip.as_str(), "qwen_2.5_vl_7b_fp8_scaled.safetensors");
        assert_eq!(p.vae.as_str(), "qwen_image_vae.safetensors");
        // Explicit names win and are validated against the lists.
        let p = pick(&params(json!({"checkpoint": "a.safetensors"})).unwrap(), &classic_models()).unwrap();
        assert_eq!(p.model.as_str(), "a.safetensors");
        // FLUX wants the T5 encoder; this server has none, so it falls over to nothing-found.
        assert!(pick(&params(json!({"unet": "flux1-schnell-Q8_0.gguf"})).unwrap(), &gguf_models()).is_err());
        let mut with_t5 = gguf_models();
        with_t5.clips.push("t5_xxl_fp8.safetensors".into());
        let p = pick(&params(json!({"unet": "flux1-schnell-Q8_0.gguf"})).unwrap(), &with_t5).unwrap();
        assert_eq!((p.family, p.clip2.as_str(), p.vae.as_str()), (Family::Flux, "t5_xxl_fp8.safetensors", "ae.safetensors"));
        // Unknown names error with the inventory.
        assert!(pick(&params(json!({"checkpoint": "ghost.safetensors"})).unwrap(), &classic_models()).is_err());
        // Nothing usable at all.
        assert!(pick(&params(json!({})).unwrap(), &ServerModels::default()).is_err());
        // Edit/video-only UNETs are skipped automatically.
        let mut edit_only = ServerModels::default();
        edit_only.unets_gguf = vec!["Qwen-Image-Edit-2509-Q4_K_M.gguf".into(), "wan2.1-i2v-14b-480p-Q5_K_M.gguf".into()];
        edit_only.clips = vec!["qwen_2.5_vl_7b_fp8_scaled.safetensors".into()];
        edit_only.vaes = vec!["qwen_image_vae.safetensors".into()];
        assert!(pick(&params(json!({})).unwrap(), &edit_only).is_err());
    }

    #[test]
    fn object_info_parses_into_inventories() {
        let v = json!({
            "CheckpointLoaderSimple": {"input": {"required": {"ckpt_name": [["a.safetensors", 1, "b.safetensors"], {}]}}},
            "UNETLoader": {"input": {"required": {"unet_name": [["u.safetensors"], {}]}}},
            "UnetLoaderGGUF": {"input": {"required": {"unet_name": [["g.gguf"], {}]}}},
            "VAELoader": {"input": {"required": {"vae_name": [["v.safetensors"], {}]}}},
            "KSampler": {"input": {"required": {"seed": ["INT", {"default": 0}]}}},
        });
        let m = parse_object_info(&v);
        assert_eq!(m.checkpoints, ["a.safetensors", "b.safetensors"]);
        assert_eq!(m.all_unets(), ["g.gguf", "u.safetensors"]);
        assert_eq!(m.vaes, ["v.safetensors"]);
        assert_eq!(m.clips, Vec::<String>::new());
    }

    fn built(p: Value, models: &ServerModels, latent: Latent) -> Value {
        let gp = params(p).unwrap();
        let picked = pick(&gp, models).unwrap();
        let seed = gp.seed.unwrap_or(1);
        workflow(&picked, &gp, seed, latent)
    }

    #[test]
    fn classic_workflow_wired_by_node_id() {
        let w = built(json!({"prompt": "a red boat", "negative": "blurry", "steps": 30, "seed": 77}), &classic_models(), Latent::Empty { width: 1024, height: 1024 });
        assert_eq!(w.pointer("/10/inputs/ckpt_name"), Some(&json!("sd_xl_refiner.safetensors")));
        assert_eq!(w.pointer("/20/inputs/text"), Some(&json!("a red boat")));
        assert_eq!(w.pointer("/30/inputs/clip"), Some(&json!(["10", 1])));
        assert_eq!(w.pointer("/50/class_type"), Some(&json!("EmptyLatentImage")));
        assert_eq!(w.pointer("/50/inputs/width"), Some(&json!(1024)));
        assert_eq!(w.pointer("/60/inputs/seed"), Some(&json!(77)));
        assert_eq!(w.pointer("/60/inputs/steps"), Some(&json!(30)));
        assert_eq!(w.pointer("/60/inputs/cfg"), Some(&json!(7.0)));
        assert_eq!(w.pointer("/60/inputs/denoise"), Some(&json!(1.0)));
        assert_eq!(w.pointer("/70/inputs/vae"), Some(&json!(["10", 2])));
        assert_eq!(w.pointer("/80/class_type"), Some(&json!("SaveImage")));
    }

    #[test]
    fn qwen_workflow_uses_its_own_loaders_and_grid() {
        // Defaults per family: 8 steps, CFG 2.2, euler/simple, SD3 latent snapped to 16.
        let w = built(json!({"prompt": "a boat", "seed": 5}), &gguf_models(), Latent::Empty { width: 1000, height: 1024 });
        assert_eq!(w.pointer("/11/class_type"), Some(&json!("UnetLoaderGGUF")));
        assert_eq!(w.pointer("/11/inputs/unet_name"), Some(&json!("qwen-image-2.1-turbo-Q8_0.gguf")));
        assert_eq!(w.pointer("/10/class_type"), Some(&json!("ModelSamplingAuraFlow")));
        assert_eq!(w.pointer("/12/inputs/type"), Some(&json!("qwen_image")));
        assert_eq!(w.pointer("/12/inputs/clip_name"), Some(&json!("qwen_2.5_vl_7b_fp8_scaled.safetensors")));
        assert_eq!(w.pointer("/50/class_type"), Some(&json!("EmptySD3LatentImage")));
        assert_eq!(w.pointer("/50/inputs/width"), Some(&json!(992)));
        assert_eq!(w.pointer("/60/inputs/steps"), Some(&json!(8)));
        let cfg = w.pointer("/60/inputs/cfg").and_then(Value::as_f64).unwrap();
        assert!((cfg - 2.2).abs() < 1e-6, "{cfg}");
        assert_eq!(w.pointer("/60/inputs/sampler_name"), Some(&json!("euler")));
        assert_eq!(w.pointer("/60/inputs/scheduler"), Some(&json!("simple")));
        assert_eq!(w.pointer("/60/inputs/model"), Some(&json!(["10", 0])));
        assert_eq!(w.pointer("/70/inputs/vae"), Some(&json!(["13", 0])));
        // Explicit values override the family defaults; img2img wires the uploaded image and
        // the asked denoise.
        let w = built(json!({"mode": "image", "denoise": 0.5, "steps": 20}), &gguf_models(), Latent::Uploaded { name: "photocraft-1.png".into(), width: 1000, height: 520 });
        assert_eq!(w.pointer("/51/class_type"), Some(&json!("LoadImage")));
        assert_eq!(w.pointer("/51/inputs/image"), Some(&json!("photocraft-1.png")));
        assert_eq!(w.pointer("/52/inputs/width"), Some(&json!(992)));
        assert_eq!(w.pointer("/52/inputs/height"), Some(&json!(512)));
        assert_eq!(w.pointer("/50/class_type"), Some(&json!("VAEEncode")));
        assert_eq!(w.pointer("/60/inputs/steps"), Some(&json!(20)));
        assert_eq!(w.pointer("/60/inputs/denoise"), Some(&json!(0.5)));
    }

    #[test]
    fn history_and_upload_replies_parse() {
        assert_eq!(history_outputs(&json!({}), "p1"), None);
        let h = json!({"p1": {"outputs": {
            "9": {"images": [
                {"filename": "a.png", "subfolder": "", "type": "output"},
                {"filename": "temp.png", "subfolder": "", "type": "temp"},
                {"filename": "b.png", "subfolder": "s", "type": "output"}]},
            "bad": {"not_images": true}}, "status": {"status_str": "success"}}});
        let out = history_outputs(&h, "p1").unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].get("subfolder"), Some(&json!("s")));
        assert_eq!(history_error(&h, "p1"), None);
        let e = json!({"p1": {"outputs": {}, "status": {"status_str": "error"}}});
        assert_eq!(history_outputs(&e, "p1"), Some(Vec::new()));
        assert!(history_error(&e, "p1").is_some());
        assert_eq!(uploaded_name(&json!({"name": "n.png", "subfolder": ""})), Some("n.png".into()));
        assert_eq!(uploaded_name(&json!({"name": "n.png", "subfolder": "in"})), Some("in/n.png".into()));
        assert_eq!(uploaded_name(&json!({})), None);
    }

    /// A 2×2 PNG, as ComfyUI would return one.
    fn png() -> Vec<u8> {
        let fmt = PixelFormat::new(ColorMode::Rgb, SampleType::U8, false);
        let surface = Surface::from_interleaved(fmt, Rect::new(0, 0, 2, 2), &[255; 12]);
        let mut doc = Document::new("t", Size::new(2, 2), ColorMode::Rgb, SampleType::U8);
        doc.insert_above(None, Layer::new("l", LayerContent::Raster(surface)));
        photocraft_io::export(&doc, "t.png", &photocraft_io::ExportOptions::default()).unwrap().bytes
    }

    thread_local! {
        /// Workflows the mock server was asked to run (tests read it back on the same thread:
        /// `Session::execute` runs jobs inline).
        static QUEUED: std::cell::RefCell<Vec<Value>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    /// A transport that answers like a ComfyUI with this inventory, recording queued workflows.
    fn mock_transport(models: ServerModels) -> ComfyFn {
        Arc::new(move |req: &ComfyRequest| match &req.call {
            ComfyCall::SystemStats => Ok(ComfyReply::Json(json!({"system": {"comfyui_version": "1.0"}}))),
            ComfyCall::ObjectInfo => Ok(ComfyReply::Json(json!({
                "CheckpointLoaderSimple": {"input": {"required": {"ckpt_name": [models.checkpoints, {}]}}},
                "UNETLoader": {"input": {"required": {"unet_name": [models.unets, {}]}}},
                "UnetLoaderGGUF": {"input": {"required": {"unet_name": [models.unets_gguf, {}]}}},
                "CLIPLoader": {"input": {"required": {"clip_name": [models.clips, {}]}}},
                "VAELoader": {"input": {"required": {"vae_name": [models.vaes, {}]}}},
            }))),
            ComfyCall::Queue { workflow, .. } => {
                QUEUED.with(|q| q.borrow_mut().push(workflow.clone()));
                Ok(ComfyReply::Json(json!({"prompt_id": "pid-1"})))
            }
            ComfyCall::History { .. } => Ok(ComfyReply::Json(json!({"pid-1": {"outputs": {
                "70": {"images": [{"filename": "out.png", "subfolder": "", "type": "output"}]}},
                "status": {"status_str": "success"}}}))),
            ComfyCall::Download { .. } => Ok(ComfyReply::Bytes(png())),
            _ => Ok(ComfyReply::Json(Value::Null)),
        })
    }

    #[test]
    fn generates_places_layers_and_opens_documents() {
        let mut s = Session::new();
        // Without a transport every command answers a graceful error (web, headless).
        for id in ["ai.comfyui.generate", "ai.comfyui.check", "ai.comfyui.models"] {
            assert!(s.execute(id, json!({})).is_err(), "{id}");
        }
        s.comfy.transport = Some(mock_transport(gguf_models()));
        // No document open: the generated image becomes one (auto-picked Qwen model).
        let r = s.execute("ai.comfyui.generate", json!({"prompt": "a cat", "seed": 7})).unwrap();
        assert_eq!(r["model"], "qwen-image-2.1-turbo-Q8_0.gguf");
        assert_eq!(r["family"], "qwen");
        assert_eq!(r["seed"], 7);
        assert_eq!(s.active().unwrap().doc.size, Size::new(2, 2));
        // With a document open: the image pins onto it as a layer, and History records the step.
        let before = s.active().unwrap().doc.layers.len();
        s.execute("ai.comfyui.generate", json!({"prompt": "a dog"})).unwrap();
        assert_eq!(s.active().unwrap().doc.layers.len(), before + 1);
        assert_eq!(s.journal.last().map(|(id, _)| id.as_str()), Some("ai.comfyui.generate"));
        // A custom workflow graph is used verbatim.
        let wf = json!({"1": {"class_type": "SaveImage", "inputs": {"images": ["2", 0]}}, "2": {"class_type": "EmptyLatentImage", "inputs": {"width": 512, "height": 512, "batch_size": 1}}});
        s.execute("ai.comfyui.generate", json!({"workflow": wf})).unwrap();
        assert_eq!(QUEUED.with(|q| q.borrow().last().cloned()), Some(wf));
        // Queries against the same mock.
        assert_eq!(s.execute("ai.comfyui.check", json!({})).unwrap()["url"], DEFAULT_URL);
        let v = s.execute("ai.comfyui.models", json!({})).unwrap();
        assert_eq!(v["counts"]["unets"], json!(5));
        assert_eq!(v["checkpoints"], json!([]));
        // Non-http URLs and junk params are rejected without a panic.
        assert!(s.execute("ai.comfyui.check", json!({"url": "ftp://nope"})).is_err());
        assert!(s.execute("ai.comfyui.check", json!({"x": 1})).is_err());
        assert!(s.execute("ai.comfyui.generate", json!({"prompt": 42})).is_err());
        // A server with nothing usable gives the actionable inventory error.
        s.comfy.transport = Some(mock_transport(ServerModels::default()));
        let e = s.execute("ai.comfyui.generate", json!({})).unwrap_err().to_string();
        assert!(e.contains("no usable model") && e.contains("UNET models"), "{e}");
    }

    #[test]
    fn img2img_queues_the_uploaded_document() {
        let mut s = Session::new();
        s.comfy.transport = Some(mock_transport(gguf_models()));
        // Image mode needs a document to send.
        assert!(s.execute("ai.comfyui.generate", json!({"mode": "image"})).is_err());
        s.execute("file.new", json!({"width": 1003, "height": 521, "background": "white"})).unwrap();
        s.execute("ai.comfyui.generate", json!({"mode": "image", "denoise": 0.5})).unwrap();
        // The queued graph is the img2img one, scaled to Qwen's 16×16 latent grid.
        let w = QUEUED.with(|q| q.borrow().last().cloned()).unwrap();
        assert_eq!(w.pointer("/51/class_type"), Some(&json!("LoadImage")));
        assert_eq!(w.pointer("/52/inputs/width"), Some(&json!(992)));
        assert_eq!(w.pointer("/52/inputs/height"), Some(&json!(512)));
        assert_eq!(w.pointer("/60/inputs/denoise"), Some(&json!(0.5)));
    }
}
