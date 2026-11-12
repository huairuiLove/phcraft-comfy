//! The layer tray: stash layers from one open document and place them into another later.
//! On a touch screen only one document is visible at a time, so dragging a layer onto the other
//! document's tab is not an option; the tray is the two-step replacement. `layer.tray.put`
//! deep-copies layers into a session-level slot (the same scratch-document form the internal
//! clipboard holds), `layer.tray.place` lands them in the active document with the same colour,
//! linking and history treatment as `layer.copyToDocument`. A slot holds its own copies, so it
//! stays valid after its source document closes, and placing keeps the slot (like Paste, it can
//! land in several documents); `layer.tray.remove` and `layer.tray.clear` empty it.

use photocraft_doc::{DocId, Document, LayerId};
use serde_json::{Value, json};

use crate::commands::CommandSpec;
use crate::{EngineError, Result, Session};

/// How many slots the tray holds at most: each one keeps a scratch document (Arc tiles while the
/// source is open, its own pixels after it closes), and a tray with no end would hold them
/// forever. Putting into a full tray is an error, never a silent drop of the oldest slot.
pub const MAX_SLOTS: usize = 16;

/// One stashed set of layers.
pub struct TraySlot {
    /// The stashed layers, bottom to top at their source canvas positions, in a scratch document
    /// of the source's colour (like `Clip::layers`, so pixels convert on the way out).
    pub scratch: Box<Document>,
    /// The document the layers came from. Only a provenance label: it is never looked up, so a
    /// closed source changes nothing.
    pub source: DocId,
    /// That document's name when the layers were stashed.
    pub source_name: String,
    /// What a tray row says: the layer's name, or "3 layers".
    pub label: String,
}

fn bad(cmd: &'static str, msg: impl Into<String>) -> EngineError {
    EngineError::BadParams { cmd: cmd.into(), msg: msg.into() }
}

fn has_active_layer(s: &Session) -> std::result::Result<(), String> {
    let d = s.active().ok_or("no document open")?;
    d.active_layer.ok_or("no active layer".into()).map(|_| ())
}

fn tray_not_empty(s: &Session) -> std::result::Result<(), String> {
    if s.layer_tray.is_empty() { Err("the tray is empty".into()) } else { Ok(()) }
}

/// The layers a command's `layer`/`layers` params name, or the active document's selection when
/// neither is given.
fn named_layers(s: &Session, p: &Value) -> Result<Vec<LayerId>> {
    let explicit = match (crate::commands::explicit_layer(s, p)?, p.get("layers")) {
        (Some(id), None) => vec![id],
        (None, Some(v)) => v
            .as_array()
            .map(|a| a.iter().map(|id| id.as_u64().map(LayerId)).collect::<Option<Vec<_>>>())
            .and_then(|ids| ids)
            .ok_or_else(|| bad(PUT, "`layers` must be an array of layer ids"))?,
        (None, None) => s.active().ok_or(EngineError::NoDocument)?.selected_layers(),
        (Some(_), Some(_)) => return Err(bad(PUT, "pass `layer` or `layers`, not both")),
    };
    Ok(explicit)
}

const PUT: &str = "layer.tray.put";
const PLACE: &str = "layer.tray.place";
const REMOVE: &str = "layer.tray.remove";
const CLEAR: &str = "layer.tray.clear";

/// `layer.tray.put`: stash copies of the named (or selected) top-level layers of the active
/// document. The source document is untouched and no history step is recorded, like Copy.
fn put(s: &mut Session, p: &Value) -> Result<Value> {
    if s.layer_tray.len() >= MAX_SLOTS {
        return Err(bad(PUT, format!("the tray is full ({MAX_SLOTS} slots); remove one first")));
    }
    let st = s.active().ok_or(EngineError::NoDocument)?;
    let (sdoc, source, source_name) = (st.doc.clone(), st.doc.id, st.doc.name.clone());
    let ids = named_layers(s, p)?;
    if let Some(missing) = ids.iter().find(|id| sdoc.layer(**id).is_none()) {
        return Err(EngineError::NoLayer(*missing));
    }
    let ids = crate::layer_multi_cmds::top_level(&sdoc, &ids);
    if ids.is_empty() {
        return Err(bad(PUT, "no layers to stash"));
    }
    if !crate::layer_copy_cmds::layered(sdoc.mode) {
        return Err(EngineError::Other(format!("layers can't be stashed with a {:?} document", sdoc.mode)));
    }
    // The copies in a scratch document of the source's colour, so a later place converts them
    // the way a whole document does (`adapt_copies`).
    let mut scratch = Document::new("", sdoc.size, sdoc.mode, sdoc.depth);
    scratch.icc_profile = sdoc.icc_profile.clone();
    scratch.resolution_dpi = sdoc.resolution_dpi;
    scratch.patterns.clone_from(&sdoc.patterns);
    for id in &ids {
        let src_layer = sdoc.layer(*id).ok_or(EngineError::NoLayer(*id))?;
        let mut l = src_layer.duplicate();
        // A stashed Background lands as an ordinary layer, like one dragged to another document.
        if sdoc.is_background(*id) {
            l.name = sdoc.next_layer_name("Layer");
            l.locks = Default::default();
        }
        scratch.layers.push(l);
    }
    // Missing shape caches must exist before any later bounds are measured from the slot alone.
    crate::layer_copy_cmds::prepare_shapes(&mut scratch.layers, &sdoc, false);
    let label = match scratch.layers.as_slice() {
        [one] => one.name.clone(),
        many => format!("{} layers", many.len()),
    };
    let stashed: Vec<u64> = scratch.layers.iter().map(|l| l.id.0).collect();
    s.layer_tray.push(TraySlot { scratch: Box::new(scratch), source, source_name, label });
    let slot = s.layer_tray.len() - 1;
    Ok(json!({"slot": slot, "layers": stashed}))
}

/// The slot a command's `slot` param names, checked against the tray.
fn slot_index(s: &Session, p: &Value, cmd: &'static str) -> Result<usize> {
    let i = p
        .get("slot")
        .and_then(Value::as_u64)
        .and_then(|i| usize::try_from(i).ok())
        .ok_or_else(|| bad(cmd, "`slot` must be a tray index"))?;
    if i >= s.layer_tray.len() {
        return Err(bad(cmd, format!("no tray slot {i} (there are {})", s.layer_tray.len())));
    }
    Ok(i)
}

/// `layer.tray.place`: copy slot `slot`'s layers into the active document, above its active
/// layer, as one history step, and select the copies. The slot stays (the tray works like the
/// clipboard); `layer.tray.remove` empties it.
fn place(s: &mut Session, p: &Value) -> Result<Value> {
    let slot_i = slot_index(s, p, PLACE)?;
    let ddoc = s.active().ok_or(EngineError::NoDocument)?.doc.clone();
    if !crate::layer_copy_cmds::layered(ddoc.mode) {
        return Err(EngineError::Other(format!("layers can't be placed with a {:?} document", ddoc.mode)));
    }
    let slot = &s.layer_tray[slot_i];
    // Fresh copies every time: two placements of one slot must never share layer ids.
    let from = &slot.scratch;
    let mut copies = Document::new("", from.size, from.mode, from.depth);
    copies.icc_profile.clone_from(&from.icc_profile);
    copies.resolution_dpi = from.resolution_dpi;
    copies.patterns.clone_from(&from.patterns);
    for l in &from.layers {
        copies.layers.push(l.duplicate());
    }
    // A name other than the copy's own was typed.
    let mut typed = None;
    if let (Some(name), [l]) = (p.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()), copies.layers.as_mut_slice()) {
        if l.name != name {
            typed = Some(name.to_string());
        }
        l.name = name.to_string();
    }
    // Measure in the slot's space: the destination refresh may clip a shape before it is moved.
    crate::layer_copy_cmds::prepare_shapes(&mut copies.layers, from, false);
    let bounds = copies.layers.iter().filter_map(crate::layer_multi_cmds::layer_bounds).fold(photocraft_geom::Rect::EMPTY, |a, b| a.union(&b));
    let content = if bounds.is_empty() { from.bounds() } else { bounds };
    // Placement: centred on the destination's canvas, centred on a point, offset from the stash
    // place, or (default) the same canvas position when both canvases have the same pixel size
    // and centred otherwise — the rule a drop on another document's tab follows.
    let (dx, dy) = if p.get("center").and_then(Value::as_bool).unwrap_or(false) {
        let (c, d) = (crate::layer_copy_cmds::center(content), crate::layer_copy_cmds::center(ddoc.bounds()));
        (d[0] - c[0], d[1] - c[1])
    } else if let Some(at) = crate::layer_copy_cmds::point(p, "at")? {
        let c = crate::layer_copy_cmds::center(content);
        (at[0] - c[0], at[1] - c[1])
    } else if let Some(o) = crate::layer_copy_cmds::point(p, "offset")? {
        (o[0], o[1])
    } else if from.size == ddoc.size {
        (0.0, 0.0)
    } else {
        let (c, d) = (crate::layer_copy_cmds::center(content), crate::layer_copy_cmds::center(ddoc.bounds()));
        (d[0] - c[0], d[1] - c[1])
    };
    let (dx, dy) = (dx.round() as i32, dy.round() as i32);
    crate::layer_copy_cmds::adapt_copies(s, &mut copies, &ddoc)?;
    crate::layer_copy_cmds::translate_copies(&mut copies, &ddoc, dx, dy);
    let patterns: Vec<_> = copies.patterns.iter().filter(|pat| !ddoc.patterns.iter().any(|d| d.id == pat.id)).cloned().collect();
    let edited = s.edit("Place from Tray", |doc, active| {
        doc.patterns.extend(patterns);
        let mut above = *active;
        let mut new = Vec::with_capacity(copies.layers.len());
        // The first copy goes where a new layer would; the rest stack above it.
        let mut place_kind = crate::layer_place::Place::AutoEditable;
        for l in copies.layers {
            let id = crate::layer_place::insert_new(doc, above, l, place_kind);
            place_kind = crate::layer_place::Place::Above;
            above = Some(id);
            new.push(id);
        }
        *active = above;
        // Repeated placements of a deep group would otherwise nest without end.
        crate::layer_multi_cmds::check_group_depth(doc, "Place from Tray")?;
        Ok(new)
    });
    let new = edited?;
    if let (Some(name), [id]) = (typed, new.as_slice()) {
        crate::type_cmds::note_typed_name(s, *id, &name);
    }
    let last = new.last().copied();
    crate::layer_multi_cmds::reselect(s, new.clone(), last);
    Ok(json!({"layers": new.iter().map(|l| l.0).collect::<Vec<_>>(), "offset": [dx, dy]}))
}

/// `layer.tray.remove`: empty one slot.
fn remove(s: &mut Session, p: &Value) -> Result<Value> {
    let i = slot_index(s, p, REMOVE)?;
    s.layer_tray.remove(i);
    Ok(Value::Null)
}

/// `layer.tray.clear`: empty the whole tray.
fn clear(s: &mut Session, _p: &Value) -> Result<Value> {
    s.layer_tray.clear();
    Ok(Value::Null)
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: PUT,
            label: "Add to Tray",
            menu: &[],
            shortcut: None,
            params: r##"{"layer":id? / "layers":[id,…]? = the active document's selected layers (top-level ones of the selection)}"##,
            enabled: has_active_layer,
            run: put,
            journal: true,
        },
        CommandSpec {
            id: PLACE,
            label: "Place from Tray",
            menu: &[],
            shortcut: None,
            params: r##"{"slot":index,"at":[x,y]? (centre the copies on this point),"offset":[dx,dy]? (from their stashed place),"center":bool=false (centre on the destination's canvas),"name":str? (one layer: the copy's name)}"##,
            enabled: |s| s.active().map_or_else(|| Err("no document open".into()), |_| Ok(())),
            run: place,
            journal: true,
        },
        CommandSpec {
            id: REMOVE,
            label: "Remove from Tray",
            menu: &[],
            shortcut: None,
            params: r##"{"slot":index}"##,
            enabled: tray_not_empty,
            run: remove,
            journal: true,
        },
        CommandSpec {
            id: CLEAR,
            label: "Clear Tray",
            menu: &[],
            shortcut: None,
            params: r##"{}"##,
            enabled: tray_not_empty,
            run: clear,
            journal: true,
        },
    ]
}

#[cfg(test)]
mod tests;
