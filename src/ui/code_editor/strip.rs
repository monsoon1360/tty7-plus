//! The editor header's controls.
//!
//! The header carries no file names: the editor itself shows what it has
//! open — Helix in its own status line, the built-in editor in the bar at its
//! foot — so a second copy here could only go stale. What is left is the pill
//! that names the terminal a filled editor covers and takes the reader back
//! to it, and the geometry every cell in that band shares.

use gpui::prelude::*;
use gpui::{AnyElement, Context, FontWeight, Window, div, px, rems};
use gpui_component::{ActiveTheme as _, h_flex};

use crate::ui::app::Tty7App;
use crate::ui::i18n::{L10nKey, t};

/// Height of a cell in the editor's header — the terminal pill.
pub(super) const CELL_H: f32 = 28.;
/// Its corner.
pub(super) const CELL_RADIUS: f32 = 7.;
/// The widest a name may make its cell before it is elided.
pub(super) const CELL_MAX_W: f32 = 168.;
/// Header text: a half step under the body size, like the rail's rows.
pub(super) const CELL_TEXT: f32 = 12.5 / 16.;

impl Tty7App {
    /// A filled editor's lead: the terminal it covers, by name, and a way back
    /// to it that keeps the file in memory without changing the layout.
    pub(super) fn render_back_to_terminal(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let name = self
            .tabs
            .get(self.active)
            .map(|tab| self.tab_label(tab, self.active, Some(window), cx))
            .unwrap_or_default();
        let badge_bg = gpui::rgb(cx.global::<crate::ui::presets::Surfaces>().window.selected);
        h_flex()
            .flex_none()
            .items_center()
            .child(
                h_flex()
                    .id("editor-back-to-terminal")
                    .occlude()
                    .h(px(CELL_H))
                    .pl(px(8.))
                    .pr(px(10.))
                    .gap(px(7.))
                    .items_center()
                    .rounded(px(CELL_RADIUS))
                    .text_size(rems(CELL_TEXT))
                    .text_color(theme.muted_foreground)
                    .hover(|s| s.bg(theme.sidebar_accent).text_color(theme.foreground))
                    .child(
                        div()
                            .flex_none()
                            .size(px(16.))
                            .rounded_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(badge_bg)
                            .font_family(theme.mono_font_family.clone())
                            .text_size(px(9.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(">_"),
                    )
                    .child(
                        div()
                            .max_w(px(CELL_MAX_W))
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .overflow_hidden()
                            .child(name),
                    )
                    .tooltip(|window, cx| {
                        gpui_component::tooltip::Tooltip::new(t(L10nKey::EditorBackToTerminal))
                            .build(window, cx)
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_terminal_area(window, cx);
                    })),
            )
            .child(
                div()
                    .flex_none()
                    .w(crate::ui::theme::hairline(window))
                    .h(px(16.))
                    .mx(px(6.))
                    .bg(theme.border),
            )
            .into_any_element()
    }
}
