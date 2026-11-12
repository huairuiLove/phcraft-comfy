//! Adaptive resolution for a dialog's live filter preview. The view's own factor (#6326) asks
//! for every pixel at 100 %, which a desktop absorbs; a phone chip cannot, and the canvas falls
//! seconds behind the drag. A published frame that measures slow while the parameters still
//! move coarsens the next frame, and each settled frame refines one step back, so a resting
//! preview converges on the view's own resolution again. Only in-drag fidelity trades away:
//! the committed filter always runs at full resolution on the job worker.

/// A published preview frame slower than this while the parameters still move coarsens the next
/// one: past it, a drag no longer feels alive.
pub(crate) const SLOW_MS: f64 = 250.0;
/// The coarsest a preview may get, in preview pixels: beneath this it stops reading as the image.
const MIN_PREVIEW_PX: u64 = 150_000;
/// At most this many coarsening steps; each step quarters the preview's pixel count.
const MAX_STEPS: u32 = 5;

/// How far the live filter preview is coarsened right now, decided by measured frame times.
/// Shared across dialogs: the next dialog's first frame starts as coarse as the last drag left
/// it and refines as it settles, so a weak device gets its first frame fast, not its sharpest.
#[derive(Default)]
pub(crate) struct PreviewDegrade {
    steps: u32,
}

impl PreviewDegrade {
    /// The factor to preview at: `k` coarsened by the current steps, never coarser than the
    /// coarsest factor that still leaves [`MIN_PREVIEW_PX`] pixels of a `doc_px`-pixel document,
    /// and never finer than `k` itself.
    pub(crate) fn factor(&self, k: u32, doc_px: u64) -> u32 {
        let coarsest = ((doc_px / MIN_PREVIEW_PX).max(1) as f64).sqrt().floor().max(1.0) as u32;
        k.saturating_mul(1u32 << self.steps.min(MAX_STEPS)).clamp(k, coarsest.max(k))
    }

    /// A frame published while the parameters were still moving took `ms`: past [`SLOW_MS`] it
    /// coarsens the next frame, keeping the drag alive on a weak device. A fast one changes
    /// nothing mid-drag; refinement waits for the parameters to settle.
    pub(crate) fn moving(&mut self, ms: f64) {
        if ms > SLOW_MS && self.steps < MAX_STEPS {
            self.steps += 1;
        }
    }

    /// A frame for the dialog's exact current value published: the parameters have settled, so
    /// refine one step. The next frame's request rebuilds at the finer factor and the worker
    /// recomputes while this frame stays up; at step zero this is a no-op.
    pub(crate) fn settled(&mut self) {
        self.steps = self.steps.saturating_sub(1);
    }

    /// The current coarsening steps.
    #[cfg(test)]
    pub(crate) fn steps(&self) -> u32 {
        self.steps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slow_moving_frames_coarsen_step_by_step_and_fast_ones_do_not() {
        let mut d = PreviewDegrade::default();
        for _ in 0..3 {
            d.moving(SLOW_MS + 1.0);
        }
        assert_eq!(d.steps(), 3);
        for _ in 0..10 {
            d.moving(1.0);
        }
        assert_eq!(d.steps(), 3, "a fast moving frame changes nothing mid-drag");
        for _ in 0..10 {
            d.moving(SLOW_MS + 1.0);
        }
        assert_eq!(d.steps(), MAX_STEPS, "the steps are capped");
    }

    #[test]
    fn settled_frames_refine_back_to_the_view() {
        let mut d = PreviewDegrade::default();
        d.moving(SLOW_MS + 1.0);
        d.moving(SLOW_MS + 1.0);
        d.settled();
        assert_eq!(d.steps(), 1);
        d.settled();
        assert_eq!(d.steps(), 0);
        d.settled();
        assert_eq!(d.steps(), 0, "already at the view's own factor");
    }

    #[test]
    fn the_factor_coarsens_within_the_pixel_floor() {
        let mut d = PreviewDegrade::default();
        // A 12 MP document at 100 % (k = 1): each step doubles the factor, but never past the
        // coarsest factor that still leaves MIN_PREVIEW_PX preview pixels (8 for 12 MP).
        assert_eq!(d.factor(1, 12_000_000), 1, "nothing measured slow yet");
        for _ in 0..2 {
            d.moving(SLOW_MS + 1.0);
        }
        assert_eq!(d.factor(1, 12_000_000), 4);
        for _ in 0..3 {
            d.moving(SLOW_MS + 1.0);
        }
        assert_eq!(d.factor(1, 12_000_000), 8, "the pixel floor caps the steps");
        // A document near the floor can barely coarsen at all.
        assert_eq!(d.factor(1, 600_000), 2);
        // A document beneath the floor never coarsens: it previews whole or not at all.
        assert_eq!(d.factor(1, 100_000), 1);
        // The factor never drops below what the view itself asked for.
        assert_eq!(d.factor(4, 12_000_000), 8);
    }
}
