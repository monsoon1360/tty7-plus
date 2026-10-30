use gpui::{Bounds, Pixels, point, px};

pub use tty7_core::core::window_state::WindowState;

pub trait WindowGeometry: Sized {
    fn from_window(window: &gpui::Window, cx: &gpui::App) -> Self;
    fn bounds(&self) -> Bounds<Pixels>;
}

impl WindowGeometry for WindowState {
    fn from_window(window: &gpui::Window, cx: &gpui::App) -> Self {
        // Persist the visible frame, not client-side decoration shadows that
        // the backend adds again when restoring the window.
        let bounds = window.inner_window_bounds().get_bounds();
        Self {
            x: bounds.origin.x.into(),
            y: bounds.origin.y.into(),
            width: bounds.size.width.into(),
            height: bounds.size.height.into(),
            display_uuid: window.display(cx).and_then(|display| display.uuid().ok()),
        }
    }

    fn bounds(&self) -> Bounds<Pixels> {
        Bounds {
            origin: point(px(self.x), px(self.y)),
            size: gpui::size(px(self.width), px(self.height)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restores_bounds_without_changing_display_identity() {
        let state = WindowState {
            x: -120.5,
            y: 42.0,
            width: 1440.0,
            height: 900.0,
            display_uuid: Some(uuid::Uuid::from_u128(42)),
        };
        assert_eq!(
            state.bounds(),
            Bounds::new(point(px(-120.5), px(42.0)), gpui::size(px(1440.0), px(900.0)))
        );
    }
}
