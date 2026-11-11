//! Long-press → context menu, the touch answer to the right-click (an iPad has no pointer
//! button). iPadOS itself maps long-press to context menus for UIKit controls; for the egui UI
//! the shell synthesizes one: when a finger holds still for half a second, a Secondary
//! press+release pair is injected at the touch position and every existing right-click menu in
//! the app opens unchanged. The finger's own events are swallowed from then until it lifts, so
//! its release cannot click through the menu that just opened.

use std::time::Duration;

use egui::{Event, Pos2, PointerButton, TouchId, TouchPhase};

/// How long the finger must hold still before the synthetic right-click fires (iPadOS' own
/// menus use ~0.5 s).
const HOLD: Duration = Duration::from_millis(500);
/// How far the finger may drift and still count as holding still (a drag cancels the press).
const SLOP: f32 = 18.0;

/// The state machine. Feed it the frame's events (in order) and the frame time; it rewrites
/// them in place: a touch that already fired has its events (and the pointer events egui
/// synthesized from it) removed, and when a hold elapses the synthetic right-click is appended.
#[derive(Default)]
pub struct LongPress {
    /// The touch being watched for a hold: id, where it started, when it started.
    held: Option<(TouchId, Pos2, f64)>,
    /// The touch whose events are swallowed until it ends (it already fired).
    swallow: Option<TouchId>,
    /// Touches currently down; two at once is a gesture, never a hold.
    down: u8,
}

impl LongPress {
    /// Rewrite one frame of events. `now` is `RawInput::time` (seconds); without a clock
    /// nothing can hold, so a `None` time only passes events through.
    pub fn frame(&mut self, events: &mut Vec<Event>, now: Option<f64>) {
        let mut fired: Option<Pos2> = None;
        // Cleared after the retain, not inside it: the finger's End event precedes the pointer
        // events egui synthesized from it in the same batch, and clearing early would let them
        // through.
        let mut lifted: Option<TouchId> = None;
        events.retain(|event| {
            match event {
                Event::Touch { id, phase, pos, .. } => match phase {
                    TouchPhase::Start => {
                        self.down = self.down.saturating_add(1);
                        // Exactly one finger down and no menu open: watch it. Anything else
                        // (a second finger joining) makes this a gesture, not a hold.
                        if self.down == 1 && self.held.is_none() && self.swallow.is_none() {
                            self.held = Some((*id, *pos, now.unwrap_or(f64::NAN)));
                        } else {
                            self.held = None;
                        }
                        self.swallow != Some(*id)
                    }
                    TouchPhase::Move => {
                        if let Some((hid, start, _)) = self.held
                            && hid == *id
                            && start.distance(*pos) > SLOP
                        {
                            self.held = None; // it became a drag or a pan
                        }
                        self.swallow != Some(*id)
                    }
                    TouchPhase::End | TouchPhase::Cancel => {
                        self.down = self.down.saturating_sub(1);
                        if self.swallow == Some(*id) {
                            lifted = Some(*id); // the finger behind the menu has lifted
                        }
                        if matches!(self.held, Some((hid, _, _)) if hid == *id) {
                            self.held = None; // lifted too soon
                        }
                        self.swallow != Some(*id)
                    }
                },
                // The pointer events egui synthesized from the swallowed finger (moves, and the
                // release) would click through the menu that just opened under it.
                Event::PointerButton { .. } | Event::PointerMoved(_) | Event::PointerGone => self.swallow.is_none(),
                _ => true,
            }
        });

        if lifted.is_some() {
            self.swallow = None;
        }

        let Some(now) = now else {
            self.held = None;
            return;
        };
        if let Some((id, start, t0)) = self.held
            && self.down == 1
            && t0.is_finite()
            && now - t0 >= HOLD.as_secs_f64()
        {
            self.held = None;
            self.swallow = Some(id);
            fired = Some(start);
        }

        if let Some(pos) = fired {
            // A complete right-click, and a primary-pointer reset first: the finger's
            // synthesized press is still down from the touch that fired, and a stuck-down
            // primary would make the menu's first tap drag instead of click.
            events.push(Event::PointerButton { pos, button: PointerButton::Primary, pressed: false, modifiers: Default::default() });
            events.push(Event::PointerGone);
            events.push(Event::PointerButton { pos, button: PointerButton::Secondary, pressed: true, modifiers: Default::default() });
            events.push(Event::PointerButton { pos, button: PointerButton::Secondary, pressed: false, modifiers: Default::default() });
        }
    }

    /// How much longer until the held touch fires, so the shell can schedule the repaint that
    /// wakes a frame at the deadline (a still finger generates no events on its own).
    pub fn fires_within(&self, now: Option<f64>) -> Option<Duration> {
        let (_, _, t0) = self.held?;
        let now = now?;
        if !t0.is_finite() || !now.is_finite() {
            return None;
        }
        let elapsed = (now - t0).max(0.0);
        HOLD.checked_sub(Duration::from_secs_f32(elapsed as f32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::Modifiers;

    fn touch(id: u64, phase: TouchPhase, pos: Pos2) -> Event {
        Event::Touch { device_id: egui::TouchDeviceId(1), id: TouchId(id), phase, pos, force: None }
    }
    fn primary(pos: Pos2, pressed: bool) -> Event {
        Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE }
    }
    fn secondary_fired(events: &[Event]) -> usize {
        events.iter().filter(|e| matches!(e, Event::PointerButton { button: PointerButton::Secondary, pressed: true, .. })).count()
    }
    fn close_enough(a: Option<Duration>, b: Option<Duration>) -> bool {
        match (a, b) {
            (Some(a), Some(b)) => a.abs_diff(b) < Duration::from_millis(2),
            (None, None) => true,
            _ => false,
        }
    }

    #[test]
    fn a_still_half_second_hold_fires_exactly_one_right_click() {
        let mut lp = LongPress::default();
        let at = Pos2::new(300.0, 200.0);
        let mut events = vec![touch(7, TouchPhase::Start, at), primary(at, true)];
        lp.frame(&mut events, Some(1.0));
        assert_eq!(secondary_fired(&events), 0, "not yet");

        // A drift within the slop is still a hold.
        let mut events = vec![touch(7, TouchPhase::Move, at + egui::vec2(2.0, 1.0)), Event::PointerMoved(at + egui::vec2(2.0, 1.0))];
        lp.frame(&mut events, Some(1.5));
        assert_eq!(secondary_fired(&events), 1, "fired at the deadline");

        // And it does not fire again while the finger stays down.
        let mut events = vec![touch(7, TouchPhase::Move, at)];
        lp.frame(&mut events, Some(2.0));
        assert_eq!(secondary_fired(&events), 0);
    }

    #[test]
    fn a_drag_cancels_the_hold() {
        let mut lp = LongPress::default();
        let at = Pos2::new(100.0, 100.0);
        let mut events = vec![touch(7, TouchPhase::Start, at)];
        lp.frame(&mut events, Some(1.0));

        let mut events = vec![touch(7, TouchPhase::Move, at + egui::vec2(40.0, 0.0))];
        lp.frame(&mut events, Some(1.2));
        let mut events = Vec::new();
        lp.frame(&mut events, Some(1.6));
        assert!(events.is_empty(), "a pan is not a long-press");
    }

    #[test]
    fn an_early_lift_cancels_the_hold() {
        let mut lp = LongPress::default();
        let at = Pos2::new(50.0, 50.0);
        let mut events = vec![touch(7, TouchPhase::Start, at)];
        lp.frame(&mut events, Some(1.0));
        let mut events = vec![touch(7, TouchPhase::End, at), primary(at, false), Event::PointerGone];
        lp.frame(&mut events, Some(1.2));
        let mut events = Vec::new();
        lp.frame(&mut events, Some(2.0));
        assert!(events.is_empty());
    }

    #[test]
    fn after_firing_the_fingers_events_are_swallowed_until_it_lifts() {
        let mut lp = LongPress::default();
        let at = Pos2::new(10.0, 10.0);
        let mut events = vec![touch(7, TouchPhase::Start, at)];
        lp.frame(&mut events, Some(1.0));
        let mut events = Vec::new();
        lp.frame(&mut events, Some(1.6)); // fires
        assert_eq!(secondary_fired(&events), 1);

        // The finger moves over the menu and lifts: none of it may click through.
        let mut events = vec![touch(7, TouchPhase::Move, at + egui::vec2(3.0, 0.0)), Event::PointerMoved(at + egui::vec2(3.0, 0.0)), touch(7, TouchPhase::End, at), primary(at, false), Event::PointerGone];
        lp.frame(&mut events, Some(1.8));
        assert!(events.is_empty(), "no click-through from the release");

        // A fresh touch afterwards is untouched.
        let mut events = vec![touch(8, TouchPhase::Start, at), primary(at, true)];
        lp.frame(&mut events, Some(2.0));
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn a_second_finger_does_not_start_a_hold() {
        let mut lp = LongPress::default();
        let a = Pos2::new(10.0, 10.0);
        let b = Pos2::new(500.0, 500.0);
        // Both down in the same frame.
        let mut events = vec![touch(7, TouchPhase::Start, a), touch(8, TouchPhase::Start, b)];
        lp.frame(&mut events, Some(1.0));
        let mut events = Vec::new();
        lp.frame(&mut events, Some(1.6));
        assert!(events.is_empty(), "two fingers are a gesture, not a hold");

        // And a finger joining later cancels an existing watch.
        let mut lp = LongPress::default();
        let mut events = vec![touch(7, TouchPhase::Start, a)];
        lp.frame(&mut events, Some(1.0));
        let mut events = vec![touch(8, TouchPhase::Start, b)];
        lp.frame(&mut events, Some(1.2));
        let mut events = Vec::new();
        lp.frame(&mut events, Some(1.8));
        assert!(events.is_empty(), "the pinch that started cancelled the hold");
    }

    #[test]
    fn fires_within_guides_the_repaint() {
        let mut lp = LongPress::default();
        let mut events = vec![touch(7, TouchPhase::Start, Pos2::new(1.0, 1.0))];
        lp.frame(&mut events, Some(1.0));
        assert!(close_enough(lp.fires_within(Some(1.0)), Some(Duration::from_millis(500))));
        assert!(close_enough(lp.fires_within(Some(1.4)), Some(Duration::from_millis(100))));
        assert_eq!(lp.fires_within(None), None);
    }
}
