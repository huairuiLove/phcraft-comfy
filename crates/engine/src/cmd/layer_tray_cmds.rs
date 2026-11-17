//! The layer tray: stash layers in a session-level box, switch documents, take them out — the
//! touch answer to `layer.copyToDocument` (#589). A touchscreen shows one document at a time, so
//! the desktop's drag-a-layer-onto-the-other-document's-tab has nothing to aim at; the tray is the
//! two-step replacement (put here, place there). Photoshop has no counterpart; this is the
//! PhotoCraft mobile affordance, driven by the Layers panel's footer button and the layer context
//! menu, and callable by agents.
//!
//! A slot holds a deep copy of the stashed top-level layers in a scratch `Document` of the
//! source's colour (mode, depth, profile, resolution, patterns), so it outlives its source
//! document closing. `layer.tray.place` lands the copies through `layer.copyToDocument`'s
//! machinery (`adapt_copies`, `translate_copies`, `layer_place::insert_new`): one history step in
//! the destination, converted to its profile and depth, selected afterwards. Default placement
//! keeps the stashed canvas position when both canvases are the same size and centres otherwise
//! (the tab-drop rule). Placing keeps the slot — Paste semantics — with fresh layer ids every
//! time; `layer.tray.remove` and `layer.tray.clear` empty it.

use photocraft_doc::{Document, LayerId};
use serde_json::{Value, json};

use crate::commands::CommandSpec;
use crate::{EngineError, Result, Session};

/// The tray never grows past this: a full tray is an error, never silent eviction (a dropped
/// slot would lose the user's stashed layers without a trace).
pub const TRAY_SLOTS: usize = 16;

/// One stashed set of layers.
#[derive(Clone, Debug)]
pub struct TraySlot {
    /// The copies, bottom to top, in a document of the source's colour (mode, depth, profile,
    /// resolution) holding the patterns they use.
    pub doc: Document,
    /// The source document's name when the layers were stashed (the popup's "from …" line).
    pub source: String,
}

fn bad(cmd: &str, msg: impl Into<String>) -> EngineError {
    EngineError::BadParams { cmd: cmd.into(), msg: msg.into() }
}

/// The slot index param, checked against the tray.
fn slot_index(s: &Session, p: &Value, key: &str) -> Result<usize> {
    let i = p.get(key).and_then(Value::as_u64).and_then(|i| usize::try_from(i).ok()).ok_or_else(|| bad("layer.tray.place", format!("`{key}` must be a tray slot index")))?;
    if i >= s.layer_tray.len() {
        return Err(bad("layer.tray.place", format!("no tray slot {i} (there are {})", s.layer_tray.len())));
    }
    Ok(i)
}

/// The layers to stash, as a scratch document of the source's colour: the explicit ids, or the
/// selection's top-level layers. The ids are top-level only; a group carries its children.
fn stashed(s: &Session, p: &Value) -> std::result::Result<Document, String> {
    let st = s.active().ok_or("no document open")?;
    let sdoc = &st.doc;
    let ids: Vec<LayerId> = match p.get("layers") {
        None => st.selected_layers(),
        Some(v) => v
            .as_array()
            .map(|a| a.iter().map(|id| id.as_u64().map(LayerId)).collect::<Option<Vec<_>>>())
            .and_then(|ids| ids)
            .ok_or("`layers` must be an array of layer ids")?,
    };
    if let Some(missing) = ids.iter().find(|id| sdoc.layer(**id).is_none()) {
        return Err(format!("no layer {}", missing.0));
    }
    let ids = crate::layer_multi_cmds::top_level(sdoc, &ids);
    if ids.is_empty() {
        return Err("no layers to stash".into());
    }
    if !crate::layer_copy_cmds::layered(sdoc.mode) {
        return Err(format!("layers can't be stashed with a {:?} document", sdoc.mode));
    }
    let mut doc = Document::new("", sdoc.size, sdoc.mode, sdoc.depth);
    doc.icc_profile = sdoc.icc_profile.clone();
    // Type re-lays out at the source's resolution, so it keeps its size in pixels.
    doc.resolution_dpi = sdoc.resolution_dpi;
    doc.patterns = sdoc.patterns.clone();
    for id in &ids {
        let src = sdoc.layer(*id).ok_or_else(|| format!("no layer {}", id.0))?;
        let mut l = src.duplicate();
        // A stashed Background arrives as an ordinary layer: the destination keeps its own.
        if sdoc.is_background(*id) {
            l.name = sdoc.next_layer_name("Layer");
            l.locks = Default::default();
        }
        doc.layers.push(l);
    }
    // Missing shape caches must participate in the placement bounds measured at place time.
    crate::layer_copy_cmds::prepare_shapes(&mut doc.layers, sdoc, false);
    Ok(doc)
}

/// `layer.tray.put`.
fn put(s: &mut Session, p: &Value) -> Result<Value> {
    if s.layer_tray.len() >= TRAY_SLOTS {
        return Err(bad("layer.tray.put", format!("the tray is full ({TRAY_SLOTS} slots)")));
    }
    let copies = stashed(s, p).map_err(|msg| bad("layer.tray.put", msg))?;
    let count = copies.layers.len();
    let names: Vec<String> = copies.layers.iter().map(|l| l.name.clone()).collect();
    let source = s.active().map_or_else(String::new, |st| st.doc.name.clone());
    s.layer_tray.push(TraySlot { doc: copies, source });
    let slot = s.layer_tray.len() - 1;
    Ok(json!({"slot": slot, "count": count, "names": names}))
}

/// `layer.tray.place`: land a slot's copies in the active document (see the module docs).
fn place(s: &mut Session, p: &Value) -> Result<Value> {
    let slot = match p.get("slot") {
        None => s.layer_tray.len().checked_sub(1).ok_or_else(|| bad("layer.tray.place", "the tray is empty"))?,
        Some(_) => slot_index(s, p, "slot")?,
    };
    let st = s.active().ok_or(EngineError::NoDocument)?;
    let dest = st.doc.clone();
    if !crate::layer_copy_cmds::layered(dest.mode) {
        return Err(EngineError::Other(format!("layers can't be placed with a {:?} document", dest.mode)));
    }
    // Fresh ids every time: the slot keeps its copies, and two placements must not share ids.
    let stashed = &s.layer_tray.get(slot).ok_or_else(|| bad("layer.tray.place", format!("no tray slot {slot}")))?.doc;
    let mut copies = Document::new("", stashed.size, stashed.mode, stashed.depth);
    copies.icc_profile = stashed.icc_profile.clone();
    copies.resolution_dpi = stashed.resolution_dpi;
    let patterns: Vec<_> = stashed.patterns.iter().filter(|pat| !dest.patterns.iter().any(|d| d.id == pat.id)).cloned().collect();
    for l in &stashed.layers {
        copies.layers.push(l.duplicate());
    }
    // Where the copies land: centred on a point, forced to the canvas centre, or (default) the
    // stashed canvas position when both canvases are the same size and centred otherwise — the
    // tab-drop rule. Bounds are measured in the slot's own space, before any destination refresh.
    let same_size = stashed.size == dest.size;
    let center = p.get("center").and_then(Value::as_bool);
    let content = |doc: &Document| {
        doc.layers
            .iter()
            .filter_map(crate::layer_multi_cmds::layer_bounds)
            .fold(photocraft_geom::Rect::EMPTY, |a, b| a.union(&b))
    };
    let bounds = content(&copies);
    let (dx, dy) = if let Some(at) = crate::layer_copy_cmds::point(p, "at")? {
        let c = crate::layer_copy_cmds::center(bounds);
        (at[0] - c[0], at[1] - c[1])
    } else {
        match center.unwrap_or(!same_size) {
            true => {
                let (c, d) = (crate::layer_copy_cmds::center(bounds), crate::layer_copy_cmds::center(dest.bounds()));
                (d[0] - c[0], d[1] - c[1])
            }
            false => (0.0, 0.0),
        }
    };
    let (dx, dy) = (dx.round() as i32, dy.round() as i32);
    crate::layer_copy_cmds::adapt_copies(s, &mut copies, &dest)?;
    crate::layer_copy_cmds::translate_copies(&mut copies, &dest, dx, dy);
    let label = if copies.layers.len() == 1 { "Place Layer" } else { "Place Layers" };
    let new = s.edit(label, |doc, active| {
        doc.patterns.extend(patterns);
        let mut above = *active;
        let mut new = Vec::with_capacity(copies.layers.len());
        // The first copy goes where a new layer would; the rest stack above it.
        let mut place = crate::layer_place::Place::AutoEditable;
        for l in copies.layers {
            let id = crate::layer_place::insert_new(doc, above, l, place);
            place = crate::layer_place::Place::Above;
            above = Some(id);
            new.push(id);
        }
        // Copied groups keep their own depth: repeated placements would otherwise nest without end.
        crate::layer_multi_cmds::check_group_depth(doc, "Place Layers")?;
        *active = above;
        Ok(new)
    })?;
    let last = new.last().copied();
    crate::layer_multi_cmds::reselect(s, new.clone(), last);
    Ok(json!({"slot": slot, "layers": new.iter().map(|l| l.0).collect::<Vec<_>>(), "offset": [dx, dy]}))
}

/// `layer.tray.remove`.
fn remove(s: &mut Session, p: &Value) -> Result<Value> {
    let slot = slot_index(s, p, "slot")?;
    let removed = s.layer_tray.remove(slot);
    Ok(json!({"slot": slot, "names": removed.doc.layers.iter().map(|l| l.name.clone()).collect::<Vec<_>>()}))
}

/// `layer.tray.clear`.
fn clear(s: &mut Session, _p: &Value) -> Result<Value> {
    let removed = s.layer_tray.len();
    s.layer_tray.clear();
    Ok(json!({"removed": removed}))
}

pub fn specs() -> Vec<CommandSpec> {
    fn put_enabled(s: &Session) -> std::result::Result<(), String> {
        s.active().ok_or("no document open")?.active_layer.ok_or("no active layer")?;
        if s.layer_tray.len() >= TRAY_SLOTS {
            Err(format!("the tray is full ({TRAY_SLOTS} slots)"))
        } else {
            Ok(())
        }
    }
    fn place_enabled(s: &Session) -> std::result::Result<(), String> {
        (!s.layer_tray.is_empty()).then_some(()).ok_or_else(|| "the tray is empty".to_string())?;
        s.active().ok_or("no document open")?;
        Ok(())
    }
    fn has_slots(s: &Session) -> std::result::Result<(), String> {
        (!s.layer_tray.is_empty()).then_some(()).ok_or_else(|| "the tray is empty".to_string())
    }
    vec![
        CommandSpec {
            id: "layer.tray.put",
            label: "Add Layers to Tray",
            menu: &[],
            shortcut: None,
            params: r##"{"layers":[id,…]?=the document's selected top-level layers} → {slot,count,names}"##,
            enabled: put_enabled,
            run: put,
            journal: false,
        },
        CommandSpec {
            id: "layer.tray.place",
            label: "Place Layers from Tray",
            menu: &[],
            shortcut: None,
            params: r##"{"slot":index?=the newest slot,"at":[x,y]? (centre the copies on this point),"center":bool?=same-size canvases keep the stashed position (default) or centre} → {slot,layers,offset}"##,
            enabled: place_enabled,
            run: place,
            journal: true,
        },
        CommandSpec {
            id: "layer.tray.remove",
            label: "Remove Tray Slot",
            menu: &[],
            shortcut: None,
            params: r##"{"slot":index} → {slot,names}"##,
            enabled: has_slots,
            run: remove,
            journal: false,
        },
        CommandSpec {
            id: "layer.tray.clear",
            label: "Clear Layer Tray",
            menu: &[],
            shortcut: None,
            params: r##"{} → {removed}"##,
            enabled: has_slots,
            run: clear,
            journal: false,
        },
    ]
}

#[cfg(test)]
mod tests;
