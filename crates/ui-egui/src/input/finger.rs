//! Finger or pen: whether the pointer's current press came from a finger touch. A touch screen
//! that reports a force only for its pen (an iPad: winit maps an Apple Pencil contact to
//! `Event::Touch { force: Some(_) }`, and a finger to `force: None` — no iPad ever had 3D
//! Touch) answers "may this press paint" when the shell runs pen-only painting
//! ([`PhotocraftApp::pen_only_painting`](crate::PhotocraftApp::pen_only_painting), the iPad):
//! the brush-family tools and every Liquify brush — Freeze and Thaw included — take only the
//! pen, so the finger stays a pointing and navigating device. A mouse, automation
//! (`ui.pointer`) and any touch that reports a force paint as before, and with pen-only
//! painting off (every other shell) nothing changes anywhere.

use egui::{Event, PointerButton, TouchId, TouchPhase};

/// Most touch ids [`FingerPresses`] remembers (the oldest go first).
const CONTACTS: usize = 16;

/// Per-app finger/pen tracking. The app feeds it the frame's input events once per frame,
/// before the canvas runs.
#[derive(Default)]
pub struct FingerPresses {
    /// The touch contacts down, and whether each has reported a force (a pen). On iOS a pencil
    /// contact carries a force from its first event, even a featherlight one; a finger never
    /// reports any.
    down: Vec<(TouchId, bool)>,
    /// The contact that began last: egui-winit emits the emulated primary press right after the
    /// `Touch` start of the contact that owns it, so that contact — not whichever is oldest —
    /// says whose press this is.
    last_start: Option<TouchId>,
    /// The primary pointer's current press belongs to a contact that reported no force.
    finger: bool,
}

impl FingerPresses {
    /// Follow a frame's input events. Idempotent, so a frame that shows the same events to more
    /// than one pass classifies the same either way.
    pub fn observe(&mut self, events: &[Event]) {
        for event in events {
            match *event {
                Event::Touch { id, phase, force, .. } => match phase {
                    TouchPhase::Start => {
                        self.down.retain(|(c, _)| *c != id);
                        if self.down.len() >= CONTACTS {
                            self.down.remove(0);
                        }
                        self.down.push((id, force.is_some()));
                        self.last_start = Some(id);
                    }
                    TouchPhase::Move if force.is_some() => {
                        if let Some(c) = self.down.iter_mut().find(|(c, _)| *c == id) {
                            c.1 = true;
                        }
                    }
                    TouchPhase::Move => {}
                    TouchPhase::End | TouchPhase::Cancel => self.down.retain(|(c, _)| *c != id),
                },
                Event::PointerButton { button: PointerButton::Primary, pressed, .. } => {
                    // A press with no touch behind it (a mouse, `ui.pointer` automation) is no
                    // finger; the lift of the press ends the question until the next press.
                    self.finger = pressed
                        && self.last_start.is_some_and(|id| {
                            self.down.iter().find(|(c, _)| *c == id).is_some_and(|(_, pen)| !pen)
                        });
                }
                _ => {}
            }
        }
    }

    /// The primary pointer's current press is a finger touch.
    pub fn press_is_finger(&self) -> bool {
        self.finger
    }
}

/// Pen-only painting (the iPad shell): refuse a finger's press on a painting surface, with the
/// status-bar hint saying why. `painting` says the press would paint (a brush-family tool, a
/// Liquify brush); `true` means the press must go no further.
pub fn refuses_finger(app: &mut crate::PhotocraftApp, painting: bool) -> bool {
    if !painting || !app.pen_only_painting || !app.finger.press_is_finger() {
        return false;
    }
    app.ui.status = "Painting needs the Apple Pencil".into();
    app.ui.status_error = true;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Modifiers, pos2};

    fn touch(id: u64, phase: TouchPhase, force: Option<f32>) -> Event {
        Event::Touch { device_id: egui::TouchDeviceId(1), id: TouchId(id), phase, pos: pos2(10.0, 10.0), force }
    }

    fn press(pressed: bool) -> Event {
        Event::PointerButton { pos: pos2(10.0, 10.0), button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE }
    }

    #[test]
    fn a_finger_press_is_a_finger_a_pencil_press_is_not() {
        let mut f = FingerPresses::default();
        // iOS as winit delivers it: the touch and the emulated press in one frame, the touch first.
        f.observe(&[touch(7, TouchPhase::Start, None), press(true)]);
        assert!(f.press_is_finger(), "a finger touch reports no force");
        f.observe(&[press(false), touch(7, TouchPhase::End, None)]);
        assert!(!f.press_is_finger(), "the lift ends the press");

        // An Apple Pencil contact carries a force (winit maps UITouch.force), a featherlight one too.
        f.observe(&[touch(8, TouchPhase::Start, Some(0.0)), press(true)]);
        assert!(!f.press_is_finger(), "the pencil paints");
        f.observe(&[press(false), touch(8, TouchPhase::End, None)]);

        // A press with no touch behind it — a mouse, `ui.pointer` automation — is no finger.
        f.observe(&[press(true)]);
        assert!(!f.press_is_finger());
    }

    #[test]
    fn a_finger_still_down_after_its_release_does_not_steal_a_pencil_press() {
        // The long-press menu releases the pointer while the finger is still on the glass
        // (long_press.rs); a pencil pressing after that is the pencil's press.
        let mut f = FingerPresses::default();
        f.observe(&[touch(1, TouchPhase::Start, None), press(true)]);
        f.observe(&[press(false)]);
        f.observe(&[touch(2, TouchPhase::Start, Some(0.4)), press(true)]);
        assert!(!f.press_is_finger(), "the pencil that just came down, not the finger resting since before");
    }

    #[test]
    fn observing_a_frame_twice_changes_nothing() {
        let frame = [touch(7, TouchPhase::Start, None), press(true)];
        let mut once = FingerPresses::default();
        once.observe(&frame);
        let mut twice = FingerPresses::default();
        twice.observe(&frame);
        twice.observe(&frame);
        assert_eq!(once.press_is_finger(), twice.press_is_finger());
    }
}
