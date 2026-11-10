//! Touch Interface layout helpers: the points where a touch-first shell differs from the
//! pointer-first desktop one, decided from the preference and the screen at hand.

use crate::PhotocraftApp;

/// The desktop layout's smallest window (`apps/photocraft` native_options). The Touch Interface
/// scale shrinks towards 1 when the magnified UI would leave less than this to lay out in, where
/// dialogs and the dock stop fitting (# an 11-inch iPad's 834 pt portrait at ×1.4 would leave
/// 595).
pub const MIN_DESIGN_WIDTH: f32 = 760.0;
pub const MIN_DESIGN_HEIGHT: f32 = 480.0;

/// Touch Interface is on (`interface.touchUi`).
pub fn on(app: &PhotocraftApp) -> bool {
    app.session.prefs().interface.touch_ui
}

/// The screen is taller than wide (a portrait iPad). Decides the bottom chrome: the tools row
/// and the panel drawer, in place of the desktop's side toolbar and end dock. `PHOTOCRAFT_FORCE_
/// PORTRAIT` exercises the portrait chrome on a landscape screen (a testing aid; nothing in the
/// app sets it).
pub fn portrait(app: &PhotocraftApp, ctx: &egui::Context) -> bool {
    if std::env::var_os("PHOTOCRAFT_FORCE_PORTRAIT").is_some() {
        return on(app);
    }
    on(app) && {
        let screen = ctx.input(|i| i.content_rect()).size();
        screen.y > screen.x
    }
}
