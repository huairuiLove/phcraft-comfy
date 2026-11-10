use super::history::history;
use super::info::info_panel;
use super::layers::{channels, layers};
use super::navigator::navigator;
use super::properties::{adjustments_grid, properties_body};
use super::{PhotocraftApp, Stroke, Tokens, icons, pos2, vec2};

// ----------------------------------------------------------------------------- dock

/// The panels and the canvas in the space left by the menus, bars and toolbar: craft-ui's
/// docking layout, with the icon rail at the window's end edge in the Studio themes. Portrait
/// Touch Interface replaces it with the canvas alone and the group drawer over the tool strip
/// (craft-ui docks only to a screen's side edges, and a portrait iPad has no side to give).
pub fn dock_area(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    let t = Tokens::get(ui.ctx());
    if crate::touch::portrait(app, ui.ctx()) {
        // All side panels before the central one (egui's panel order), the drawer first so the
        // tool strip keeps the screen's bottom edge.
        touch_drawer(app, ui);
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(crate::prefs_ui::pasteboard_color(app).unwrap_or(t.canvas)))
            .show(ui, |ui| crate::canvas::document_area(app, ui));
        return;
    }
    if app.ui.panels.rail && !t.pro {
        rail(app, ui, &t);
    }
    egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| crate::panel_docking::show(app, ui));
    crate::dock::persist(app, ui.ctx());
    crate::color_panel_ui::persist(app);
}

/// The panel drawer above the portrait tool strip: one button per panel group (the side rail's
/// icons), the picked group's tabs and body over the canvas, closed by tapping it again. The
/// bodies are the dock's own (`crate::panels::dock_body`), and the selected tab is remembered
/// per group like the desktop dock's.
fn touch_drawer(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    use crate::dock::Group;
    let t = Tokens::get(ui.ctx());
    let id = egui::Id::unique("touch-drawer");
    let open: Option<Group> = ui.ctx().data(|d| d.get_temp(id));
    let pro = t.pro;
    let entries: [(Group, &str, &str); 8] = [
        (Group::Layers, "layers", tl!("Layers")),
        (Group::Properties, "sliders-horizontal", tl!("Properties")),
        (Group::Color, "palette", tl!("Color & Swatches")),
        (Group::Navigator, "navigation", tl!("Navigator")),
        (Group::History, "clock", tl!("History")),
        (Group::Brushes, "brush", tl!("Brushes")),
        (Group::Character, "type", tl!("Character")),
        (Group::Comfy, "sparkles", tl!("Generate")),
    ];
    let bar = 44.0;
    let body = if open.is_some() { (ui.ctx().input(|i| i.content_rect()).height() * 0.4).clamp(180.0, 320.0) } else { 0.0 };
    egui::Panel::bottom("touch_panels")
        .resizable(false)
        .exact_size(bar + body)
        .frame(egui::Frame::NONE.fill(t.chrome).inner_margin(egui::Margin::symmetric(6, 4)))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing = vec2(3.0, 2.0);
                for (group, icon, name) in entries {
                    if icons::rail_button(ui, icon, 32.0, open == Some(group), name).clicked() {
                        let next = (open != Some(group)).then_some(group);
                        ui.ctx().data_mut(|d| d.insert_temp(id, next));
                    }
                }
            });
            let Some(group) = open else { return };
            let tabs = group.tabs(pro);
            let tab = *group.tab_mut(&mut app.ui.dock_tabs);
            ui.add_space(2.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(4.0, 2.0);
                for (index, label) in tabs.iter().enumerate() {
                    if ui
                        .selectable_label(index == tab, egui::RichText::new(tl!(label)).text_style(egui::TextStyle::Small))
                        .clicked()
                    {
                        *group.tab_mut(&mut app.ui.dock_tabs) = index;
                    }
                }
            });
            ui.add_space(2.0);
            let tab = *group.tab_mut(&mut app.ui.dock_tabs);
            egui::ScrollArea::vertical()
                .id_salt(("touch-drawer-body", group.key()))
                .auto_shrink([false, false])
                .max_height(body - bar - 4.0)
                .show(ui, |ui| crate::panels::dock_body(app, ui, group, tab));
        });
}

/// The docking layout's protected canvas panel: the document area, with the icon rail on its end
/// side, beside the dock, in the Pro themes.
pub(crate) fn canvas_panel(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    let t = Tokens::get(ui.ctx());
    let backdrop = if app.ui.view.hides_chrome() { egui::Color32::BLACK } else { crate::prefs_ui::pasteboard_color(app).unwrap_or(t.canvas) };
    if app.ui.panels.rail && t.pro {
        rail(app, ui, &t);
    }
    egui::CentralPanel::default().frame(egui::Frame::NONE.fill(backdrop)).show(ui, |ui| crate::canvas::document_area(app, ui));
}

/// Narrow icon rail, unless an embedding app hides it: shows, expands or collapses panel groups.
fn rail(app: &mut PhotocraftApp, ui: &mut egui::Ui, t: &Tokens) {
    let customized = app.ui.docking.is_some();
    let p = app.ui.panels.clone();
    let (rw, rb) = if t.pro { (36.0, 28.0) } else { (44.0, 32.0) };
    crate::direction::end_panel("rail")
        .resizable(false)
        .exact_size(rw)
        .frame(egui::Frame::NONE.fill(t.chrome).inner_margin(egui::Margin::symmetric(4, 8)))
        .show(ui, |ui| {
            // The separator is on the canvas side.
            let r = ui.max_rect();
            let x = if crate::direction::is_rtl() { r.right() + 6.0 } else { r.left() - 6.0 };
            ui.painter().line_segment([pos2(x, r.top() - 8.0), pos2(x, r.bottom() + 8.0)], Stroke::new(1.0, t.separator));
            ui.spacing_mut().item_spacing.y = 4.0;
            use crate::dock::Group;
            let entries: [(&str, &str, Group); 6] = [
                ("sliders-horizontal", tl!("Properties"), Group::Properties),
                ("navigation", tl!("Navigator"), Group::Navigator),
                ("palette", tl!("Color & Swatches"), Group::Color),
                ("layers", tl!("Layers"), Group::Layers),
                ("clock", tl!("History"), Group::History),
                ("brush", tl!("Brushes"), Group::Brushes),
            ];
            for (icon, name, g) in entries {
                // Studio floats Properties outside the dock.
                let docked = t.pro || g != Group::Properties;
                let on = if customized { crate::panel_docking::rail_on(app, g) } else { g.shown(&p) && !(docked && app.ui.dock.is_collapsed(g)) };
                if icons::rail_button(ui, icon, rb, on, name).clicked() {
                    if customized {
                        crate::panel_docking::rail_click(app, g);
                    } else {
                        crate::dock::rail_click(app, g, docked);
                    }
                }
            }
        });
}

/// The dock's narrowest width (points). It is the right dock, on the left in a right-to-left UI.
pub(crate) const DOCK_MIN_WIDTH: f32 = 250.0;

/// What the dock leaves the toolbar and the canvas when it is widest (points).
const DOCK_LEAVES: f32 = 400.0;

/// The dock's width range (points) in a window `window_width` wide: from [`DOCK_MIN_WIDTH`] to
/// everything but [`DOCK_LEAVES`], and never capped below the old 520, so two-column layouts can
/// be as wide as the window allows (#7896).
pub(crate) fn dock_width_range(window_width: f32) -> std::ops::RangeInclusive<f32> {
    let max = if window_width.is_finite() { (window_width - DOCK_LEAVES).max(520.0) } else { 520.0 };
    DOCK_MIN_WIDTH..=max
}

pub(crate) fn dock_width_id() -> egui::Id {
    egui::Id::unique("dock-width-request")
}

/// Set the right dock's width on the next frame (`ui.set {dockWidth}`); clamped to its range.
pub fn request_dock_width(ctx: &egui::Context, w: f32) {
    let range = dock_width_range(ctx.content_rect().width());
    let w = if w.is_finite() { w.clamp(*range.start(), *range.end()) } else { *range.start() };
    ctx.data_mut(|d| d.insert_temp(dock_width_id(), w));
}

/// One dock group's tab content; `panel_docking` bounds it and scrolls it when it's taller.
pub(crate) fn dock_body(app: &mut PhotocraftApp, ui: &mut egui::Ui, group: crate::dock::Group, tab: usize) {
    use crate::dock::Group;
    let pro = Tokens::get(ui.ctx()).pro;
    match (group, tab) {
        (Group::Color, 2) => crate::preset_panels::gradients_panel(app, ui),
        (Group::Color, 3) => crate::preset_panels::patterns_panel(app, ui),
        // The Color tab is the same in every theme, in the mode picked from its panel menu
        // (#6000); only the tab order differs between themes.
        (Group::Color, tab) if group.tabs(pro).get(tab) == Some(&"Color") => crate::color_panel_ui::panel(app, ui),
        (Group::Color, _) => crate::swatches_ui::panel(app, ui),
        (Group::Properties, 0) => properties_body(app, ui),
        (Group::Properties, _) => adjustments_grid(app, ui),
        (Group::Character, tab) => crate::type_tool::character_panel(app, ui, tab == 1),
        (Group::Navigator, 0) => navigator(app, ui),
        (Group::Navigator, 1) => crate::histogram_panel::show(app, ui),
        (Group::Navigator, _) => info_panel(app, ui),
        (Group::History, 0) => history(app, ui),
        (Group::History, 1) => crate::actions::panel(app, ui),
        (Group::History, _) => crate::comps_ui::panel(app, ui),
        (Group::Layers, 0) => layers(app, ui),
        (Group::Layers, 1) => channels(app, ui),
        (Group::Layers, _) => crate::vector_ui::paths_panel(app, ui),
        (Group::Brushes, _) => crate::brushes_tab::show(app, ui),
        (Group::Comfy, _) => crate::comfy_panel::show(app, ui),
    }
}
