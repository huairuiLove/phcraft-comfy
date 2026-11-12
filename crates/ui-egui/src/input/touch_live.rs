//! Live touches: how many fingers are genuinely on the screen right now. egui ends a touch
//! only when its End or Cancel event arrives; a lift that never arrives (a window that lost
//! focus mid-gesture, an input pipeline that wedged, a lift swallowed by a shell filter) stays
//! "down" in egui forever, and every later one-finger drag then reads as multi-touch — the
//! canvas zooms and pans where it should paint. [`live_touches`] counts the touches that
//! reported something recently, so a gesture needs fingers that are still there.

use std::collections::BTreeMap;

use egui::{Context, Event, TouchId, TouchPhase};

/// How long (seconds) a touch that reported nothing still counts as down. A pinch streams
/// moves, and Android's move batches carry every down pointer, so resting fingers stay well
/// inside this; a touch whose end was lost never reports again, and stops counting as a finger
/// long before the user tries to paint next. A one-finger-parked pivot pinch held still for
/// longer than this pauses its zoom until the parked finger moves again — the price of never
/// zooming on one live finger.
const STILL_LIVE: f64 = 3.0;

fn watch_id() -> egui::Id {
    egui::Id::unique("pc-touch-live")
}

/// This frame's live touch count. The touches egui counts as down, minus those that have
/// reported nothing for [`STILL_LIVE`] seconds. Cached per frame: the first call of a frame
/// scans its events, the rest read the cache.
pub fn live_touches(ctx: &Context) -> usize {
    let frame = ctx.cumulative_pass_nr();
    if let Some(state) = ctx.data(|d| d.get_temp::<LiveTouches>(watch_id())).filter(|s| s.frame == frame) {
        return state.live;
    }
    let now = ctx.input(|i| i.time);
    let mut touches: BTreeMap<TouchId, f64> = ctx.data(|d| d.get_temp::<LiveTouches>(watch_id())).map(|s| s.touches).unwrap_or_default();
    ctx.input(|i| {
        for event in &i.events {
            if let Event::Touch { id, phase, .. } = event {
                match phase {
                    TouchPhase::Start | TouchPhase::Move => {
                        touches.insert(*id, now);
                    }
                    TouchPhase::End | TouchPhase::Cancel => {
                        touches.remove(id);
                    }
                }
            }
        }
    });
    let live = touches.values().filter(|seen| now - **seen <= STILL_LIVE).count();
    ctx.data_mut(|d| d.insert_temp(watch_id(), LiveTouches { frame, touches, live }));
    live
}

/// The watch itself, kept in `Context` data across frames.
#[derive(Clone, Default)]
struct LiveTouches {
    /// The frame [`live_touches`] last ran for.
    frame: u64,
    /// Every touch egui still counts as down, with when it last reported.
    touches: BTreeMap<TouchId, f64>,
    /// That frame's live count.
    live: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{pos2, RawInput};

    fn touch(id: u64, phase: TouchPhase) -> Event {
        Event::Touch { device_id: egui::TouchDeviceId(0), id: TouchId(id), phase, pos: pos2(10.0, 10.0), force: None }
    }

    /// A frame of `events` at time `time` through a real egui context, then the live count.
    /// The texture delta is cleared as any egui test context must (the next pass asserts it).
    fn run(ctx: &Context, events: Vec<Event>, time: f64) -> usize {
        let raw = RawInput { time: Some(time), events, ..Default::default() };
        ctx.run_ui(raw, |_| {}).textures_delta.clear();
        live_touches(ctx)
    }

    #[test]
    fn two_moving_fingers_are_live_a_silent_one_is_not() {
        let ctx = Context::default();
        let one = run(&ctx, vec![touch(1, TouchPhase::Start)], 1.0);
        assert_eq!(one, 1, "one finger down");
        let two = run(&ctx, vec![touch(2, TouchPhase::Start)], 1.1);
        assert_eq!(two, 2, "a second finger joins");
        // Finger 2 keeps reporting; finger 1 goes silent past STILL_LIVE.
        let mut still_two = two;
        let mut t = 2.0;
        while t < 3.9 {
            still_two = run(&ctx, vec![touch(2, TouchPhase::Move)], t);
            t += 0.1;
        }
        assert_eq!(still_two, 2, "while both report, both are live");
        let one_live = run(&ctx, vec![touch(2, TouchPhase::Move)], 5.0);
        assert_eq!(one_live, 1, "the finger silent for over STILL_LIVE seconds no longer counts");
    }

    #[test]
    fn a_lifted_touch_stops_counting_at_once() {
        let ctx = Context::default();
        assert_eq!(run(&ctx, vec![touch(1, TouchPhase::Start)], 1.0), 1);
        assert_eq!(run(&ctx, vec![touch(1, TouchPhase::End)], 1.1), 0, "the lift ends it");
        // And a fresh finger after that is alone, not a pair with a ghost.
        assert_eq!(run(&ctx, vec![touch(2, TouchPhase::Start)], 1.2), 1);
    }
}
