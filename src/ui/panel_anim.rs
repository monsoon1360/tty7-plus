//! Eased width for the four workspace regions.

use gpui::Window;
use std::time::Instant;

/// The shared transition length (`tab_strip::TRANSITION_MS`, 140 ms).
pub(crate) const PANEL_ANIM_MS: u64 = crate::ui::tab_strip::TRANSITION_MS;

/// Animations run in the app, never under `cargo test`: gpui's animation
/// clock is the real one, and the layout tests measure geometry one parked
/// frame after a toggle.
pub(crate) const PANEL_ANIMATIONS: bool = !cfg!(test);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PanelSlot {
    Sidebar = 0,
    Terminal = 1,
    Document = 2,
    Details = 3,
}

#[derive(Clone, Copy)]
struct Slide {
    from: f32,
    started: Instant,
}

pub(crate) struct PanelAnimator {
    slides: [Option<Slide>; 4],
    shown: [bool; 4],
    drawn: [f32; 4],
    primed: [bool; 4],
}

impl Default for PanelAnimator {
    fn default() -> Self {
        Self {
            slides: [None; 4],
            shown: [false; 4],
            drawn: [0.; 4],
            primed: [false; 4],
        }
    }
}

impl PanelAnimator {
    /// This frame's width for `slot`. Starts a slide on the first frame the
    /// open flag changes, and returns `target` when it ends.
    pub(crate) fn width(
        &mut self,
        slot: PanelSlot,
        open: bool,
        target: f32,
        window: &mut Window,
    ) -> f32 {
        let i = slot as usize;
        let target = if open { target.max(0.) } else { 0. };
        if !PANEL_ANIMATIONS {
            self.shown[i] = open;
            self.drawn[i] = target;
            return target;
        }
        if !self.primed[i] {
            self.primed[i] = true;
            self.shown[i] = open;
            self.drawn[i] = target;
            return target;
        }
        if self.shown[i] != open {
            self.shown[i] = open;
            self.slides[i] = Some(Slide {
                from: self.drawn[i],
                started: Instant::now(),
            });
        }
        let Some(slide) = self.slides[i] else {
            self.drawn[i] = target;
            return target;
        };
        let step = slide.started.elapsed().as_secs_f32() / (PANEL_ANIM_MS as f32 / 1000.);
        if step >= 1. {
            self.slides[i] = None;
            self.drawn[i] = target;
            return target;
        }
        window.request_animation_frame();
        let delta = gpui::ease_out_quint()(step);
        let width = slide.from + (target - slide.from) * delta;
        self.drawn[i] = width;
        width
    }

    pub(crate) fn active(&self, slot: PanelSlot) -> bool {
        self.slides[slot as usize].is_some()
    }

    pub(crate) fn set_drawn(&mut self, slot: PanelSlot, width: f32) {
        self.drawn[slot as usize] = width;
    }

    /// The width a closing panel started from — the terminal keeps laying
    /// itself out at this width while its wrapper clips it away.
    pub(crate) fn slide_from(&self, slot: PanelSlot) -> Option<f32> {
        self.slides[slot as usize].map(|slide| slide.from)
    }
}
