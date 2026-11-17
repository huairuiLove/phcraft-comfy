//! Which touch contact the emulated pointer press belongs to, and whether it is a finger.
//!
//! winit emulates the mouse pointer from touch contacts: the contact that started last owns the
//! press — an owning contact's End frees the pointer for the next contact that starts, and
//! iPadOS cancels a resting finger when the Apple Pencil lands, so after any cancellation the
//! newest contact is the pen. [`FingerPresses`] follows the frame's touch events (from
//! `logic_frame`, before the canvas runs; kittest harnesses reach it too, since they skip
//! `raw_input_hook`) to answer one question: is the emulated primary press a finger's? A finger
//! is a contact that began without a force — the Apple Pencil's contacts always carry one, and
//! no iPad ever had 3D Touch — while no contact at all (a mouse, `ui.pointer` automation) is
//! never a finger.
//!
//! This is the pen-only painting policy's classifier (`PhotocraftApp::pen_only_painting`): a
//! resting finger after the long-press menu must not steal a later pencil press's
//! classification, which "any finger is down" ([`crate::stylus::Stylus::finger_down`], the
//! Object Selection gate) would.

/// The emulated pointer press's contact, classified. Feed it one frame's events.
#[derive(Debug, Default)]
pub struct FingerPresses {
    /// The contacts currently down that began without a force: fingers.
    fingers: Vec<egui::TouchId>,
    /// The contact that started last and is still down: the one whose press the emulated
    /// pointer carries (`None` once it lifts, until the next contact starts).
    latest: Option<egui::TouchId>,
    /// The egui frame [`Self::update_for_frame`] last ran in.
    fed_frame: Option<u64>,
}

/// Most touch ids remembered (the oldest go first, as everywhere else in the input state).
const CONTACTS: usize = 16;

impl FingerPresses {
    /// [`Self::frame`] once per egui frame: a host can run a frame in several passes, and the
    /// same events must not be counted twice (the same guard `Stylus::update_for_frame` keeps).
    pub fn update_for_frame(&mut self, frame: u64, events: &[egui::Event]) {
        if self.fed_frame.replace(frame) != Some(frame) {
            self.frame(events);
        }
    }

    /// Follow one frame's touch events. Judged at the contact's start, as
    /// [`crate::stylus::Stylus::update`] judges: the raw force then says finger or pen, before
    /// any pen-pressure-0 mapping could lend a known pen a force.
    pub fn frame(&mut self, events: &[egui::Event]) {
        for e in events {
            let egui::Event::Touch { id, phase, force, .. } = e else { continue };
            match phase {
                egui::TouchPhase::Start => {
                    if force.filter(|f| f.is_finite()).is_some() {
                        self.fingers.retain(|f| f != id);
                    } else if !self.fingers.contains(id) {
                        if self.fingers.len() >= CONTACTS {
                            self.fingers.remove(0);
                        }
                        self.fingers.push(*id);
                    }
                    self.latest = Some(*id);
                }
                egui::TouchPhase::End | egui::TouchPhase::Cancel => {
                    if self.latest == Some(*id) {
                        self.latest = None;
                    }
                    self.fingers.retain(|f| f != id);
                }
                egui::TouchPhase::Move => {}
            }
        }
    }

    /// Is the contact the emulated primary press belongs to a finger? A mouse, `ui.pointer`
    /// automation and any force-reporting touch (the pencil) are not.
    pub fn pointer_is_finger(&self) -> bool {
        self.latest.is_some_and(|id| self.fingers.contains(&id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Modifiers, Pos2, TouchDeviceId, TouchId, TouchPhase};

    fn touch(id: u64, phase: TouchPhase, force: Option<f32>) -> egui::Event {
        egui::Event::Touch { device_id: TouchDeviceId(1), id: TouchId(id), phase, pos: Pos2::new(10.0, 10.0), force }
    }

    #[test]
    fn a_contact_without_a_force_is_a_finger_a_force_is_the_pencil() {
        let mut f = FingerPresses::default();
        f.frame(&[touch(1, TouchPhase::Start, None)]);
        assert!(f.pointer_is_finger(), "a finger began and owns the pointer");
        f.frame(&[touch(1, TouchPhase::End, None)]);
        assert!(!f.pointer_is_finger(), "the lift frees the pointer");
        f.frame(&[touch(2, TouchPhase::Start, Some(0.0))]);
        assert!(!f.pointer_is_finger(), "the pencil reports a force, even 0.0");
    }

    #[test]
    fn the_contact_that_started_last_owns_the_pointer() {
        let mut f = FingerPresses::default();
        // A finger rests (its long-press menu's release has come and gone); the pencil lands
        // and iPadOS cancels the finger.
        f.frame(&[touch(1, TouchPhase::Start, None)]);
        f.frame(&[touch(2, TouchPhase::Start, Some(0.3)), touch(1, TouchPhase::Cancel, None)]);
        assert!(!f.pointer_is_finger(), "the resting finger did not steal the pencil's press");
        // Without the cancellation the same holds: the newest contact is the owner.
        let mut f = FingerPresses::default();
        f.frame(&[touch(1, TouchPhase::Start, None), touch(2, TouchPhase::Start, Some(0.3))]);
        assert!(!f.pointer_is_finger());
        // The pencil lifts: nothing owns the pointer, finger still down or not.
        f.frame(&[touch(2, TouchPhase::End, Some(0.3))]);
        assert!(!f.pointer_is_finger());
        f.frame(&[touch(3, TouchPhase::Start, None)]);
        assert!(f.pointer_is_finger(), "the next finger that starts owns it again");
    }

    #[test]
    fn no_contact_at_all_is_never_a_finger() {
        let f = FingerPresses::default();
        assert!(!f.pointer_is_finger(), "a mouse or ui.pointer automation paints");
        let mut f = FingerPresses::default();
        f.frame(&[egui::Event::PointerButton { pos: Pos2::new(5.0, 5.0), button: egui::PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE }]);
        assert!(!f.pointer_is_finger());
    }
}
