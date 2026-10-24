use gpui::{AnyElement, Context, Window, div, prelude::*, px, rems};
use gpui_component::button::Button;
use gpui_component::input::Input;
use gpui_component::{
    ActiveTheme as _, Icon, IconName, InteractiveElementExt as _, Sizable as _, WindowExt as _,
    h_flex, v_flex,
};
use std::path::PathBuf;

use crate::core::config::{Config, RightPanelTab};
use crate::daemon::protocol::PaneProcs;
use crate::ui::app::{
    CONTENT_INSET, TILE_GLYPH_SM, TILE_SIZE_SM, TITLE_BAR_BORDER, TitleStripRow as _, Tty7App,
    tile_trailing_inset,
};
use crate::ui::i18n::{L10nKey, t, t_fmt};

/// Wide enough for the word tabs, bare, beside the panel toggle at
/// the default interface size — the Changes count is dropped before a label is
/// ever cut. A larger UI font raises the floor past this; see
/// [`right_panel_tabs_floor`].
pub(crate) const MIN_WIDTH: f32 = 240.;

/// What the tab row needs with the count dropped: the labels, the row's lead,
/// its 2px gaps, and the panel toggle at the trailing end.
fn right_panel_tabs_floor(window: &Window, cx: &gpui::App) -> f32 {
    let chrome = match cfg!(target_os = "macos") {
        true => crate::ui::tab_strip::trailing_chrome_tiles_w(),
        false => tile_trailing_inset(),
    };
    (TAB_ROW_LEAD + crate::ui::tab_strip::right_panel_tab_labels_w(window, cx) + chrome).ceil()
}

/// The tab row's leading inset: the selected tab's pill starts on
/// `CONTENT_INSET`, the edge every row fill below it starts on, and each tab's
/// click target reaches `TAB_OUTER_PAD` past its pill.
const TAB_ROW_LEAD: f32 = crate::ui::app::CONTENT_INSET - crate::ui::tab_strip::TAB_OUTER_PAD;

/// How tall the tab row is where it sits below the title bar (Windows and
/// Linux): the tab's 26px hover pill plus 2px either side. A full title-bar
/// height here stacked a second 40px band under the first and left the labels
/// sitting well down from the window's top edge.
const TAB_ROW_HEIGHT: f32 = TILE_SIZE_SM + 6.;

/// How wide a panel edge is to grab. Both edges a window can drag — the tab
/// sidebar's and this panel's — are the same target, so they are one number.
pub(crate) const RESIZE_HANDLE_WIDTH: f32 = 8.;

/// The panel's type scale, in rems, on the same ladder as the rest of the
/// window.
///
/// This panel used to carry its own run of pixel sizes — 12 for body, 11.5/11
/// under it — which put its *primary* text at the size everything else uses
/// for *secondary* text, so the panel read a step smaller than the sidebar
/// beside it, and stayed that size when `ui_font_size` moved. In rems `TEXT`
/// and `META` are exactly `text_sm()` and `text_xs()`; they are spelled out
/// only because the mono variants have to be derived from them.
///
/// Mono sits a notch under the sans it pairs with: at an equal size its
/// x-height and stems read a size larger, which turns a label and its value
/// into two sizes instead of one line. The notch is a rem fraction rather than
/// a fixed pixel, so the correction scales with the text it is correcting.
const STEP: f32 = 1. / 16.;
pub(crate) const TEXT: f32 = 14. * STEP;
pub(crate) const TEXT_MONO: f32 = TEXT - STEP;
pub(crate) const META: f32 = 12. * STEP;
pub(crate) const META_MONO: f32 = META - STEP;

/// Compact section headings: a half step under the caption size, set apart by
/// weight and ink rather than by size.
pub(crate) const HEADING: f32 = 11.5 * STEP;

/// The word tabs at the top of the panel, a half step under body text.
pub(crate) const TAB_TEXT: f32 = 12.5 * STEP;

/// Height of the compact file-search well.
const SEARCH_WELL_H: f32 = 28.;

/// The leading glyph on a panel row — the file tree's folder and file marks.
///
/// Keep row glyphs on the same 16px grid as the toolbar.
pub(crate) const ROW_GLYPH: f32 = crate::ui::app::TILE_GLYPH;

// Git and Files share this type ramp so switching panels does not change the
// apparent size of the interface. Files reaches the same body size through
// `text_sm()`.
//

/// A row's own padding: lists sit `CONTENT_INSET` in from the panel's edges,
/// which is where a hovered row's fill starts, and each row pads its content
/// this much further in. The text column is therefore [`TEXT_INSET`] — 20px —
/// and the fill bleeds 8px past it on both sides.
///
/// It lives here rather than in one tab so Git and Files share the same row
/// geometry. Text outside a row sits at `TEXT_INSET` directly.
pub(crate) const ROW_INSET: f32 = 8.;

/// Where the panel's text column starts, from either edge: the list inset
/// plus the row's own padding.
pub(crate) const TEXT_INSET: f32 = CONTENT_INSET + ROW_INSET;

/// The corner of a row's hover and selection fill, in every tab.
pub(crate) const ROW_FILL_RADIUS: gpui::Pixels = px(6.);

/// The space between the Files tab's search well and the tree under it.
const FILES_SEARCH_GAP: f32 = 10.;

/// The strip the row and group action buttons live in, revealed by hovering
/// `row`.
///
/// Absolutely positioned and opaque, so it covers the tail of the row's text
/// rather than pushing it aside: hovering a row must not move a single pixel
/// of it, or the list crawls under the pointer.
///
/// It stops the mouse-down by hand instead of calling `occlude()`, which is
/// the obvious way to keep a click off the row underneath and was what made
/// the buttons vanish the moment the pointer reached them. `occlude()` is a
/// *hitbox* behaviour, and gpui inserts hitboxes in prepaint, which never
/// looks at `visibility` — so the strip blocked the mouse even while it was
/// invisible. Blocking cuts the hit test short at the blocking hitbox, and the
/// row's hitbox is behind this one because a parent prepaints before its
/// children; `group_hover` is nothing more than "is the group's hitbox
/// hovered", so the row stopped counting as hovered and the strip hid itself
/// — background, buttons and all — with the pointer sitting right on it. The
/// buttons' own hitboxes came from prepaint and outlived the paint, so they
/// went on answering tooltips for glyphs that were no longer drawn.
///
/// Stopping propagation buys the same "this click is ours, not the row's"
/// without lying to the hit test: children register their handlers after this
/// one and gpui bubbles back to front, so a button still gets its click first.
pub(crate) fn action_strip(row: &gpui::SharedString, backing: u32) -> gpui::Div {
    h_flex()
        .absolute()
        .right(px(ROW_INSET))
        .top_0()
        .bottom_0()
        .items_center()
        .gap(px(1.))
        .bg(gpui::rgb(backing))
        .invisible()
        .group_hover(row.clone(), |s| s.visible())
        .on_any_mouse_down(|_, _, cx| cx.stop_propagation())
}

/// Height of a bare inline input row — Source Control's new-branch and
/// checkout rows.
///
/// gpui-component sizes an `Input` border-box, and `.xsmall()` is
/// `input_h(Size::XSmall)` = `h_5()` = 20px: one `LINE_HEIGHT` of `Rems(1.25)`
/// = 20px with `input_py(Size::XSmall)` = 0 above and below. (`.appearance(false)`
/// only drops the background, border and radius; the padding and the height
/// stay.) Thirty leaves that field 5px of slack top and bottom.
///
/// The filled search well of [`Tty7App::panel_search`] is not this: it is its
/// own 28px shape, and the strip around it is exactly that tall so the well
/// sits on the panel's 8px top inset rather than a pixel under it.
pub(crate) const SEARCH_H: f32 = 30.;

/// The search well's glyph, and the space between it and the field. The
/// `.xsmall()` field adds 4px of leading padding of its own, so the text lands
/// 11 + 3 + 4 = 18px past the glyph's left edge — on `TEXT_INSET + 18`.
const SEARCH_GLYPH: f32 = 11.;
const SEARCH_GLYPH_GAP: f32 = 3.;

#[derive(Default)]
pub(crate) struct RightPanelState {
    pub(crate) procs_pane: Option<u64>,
    pub(crate) procs: Option<PaneProcs>,
    pub(crate) procs_loading: bool,
    pub(crate) procs_gen: u64,
    pub(crate) procs_forwards: Option<crate::ui::app::ForwardRoute>,
    /// Who to ask about `procs_pane` — `None` means this machine's daemon.
    pub(crate) procs_host: Option<crate::ui::host_ops::SharedHost>,
    /// How this pane's loopback ports can be reached; only remote panes whose
    /// ports need forwarding require the background watch.
    pub(crate) port_route: crate::terminal::view::PortRoute,
    /// The remote ports already forwarded unasked, per set of forwards.
    ///
    /// Kept so that a forward the user then deletes is not immediately rebuilt
    /// by the next poll — the automatic offer is made once per port, and after
    /// that the port is theirs to forward or not.
    ///
    /// Keyed by owner rather than held for the pane in front, because a
    /// workspace's forwards are shared by all of its panes: switching tabs
    /// would otherwise re-announce every port the workspace had already
    /// forwarded, and switching back would re-offer what was just dismissed.
    /// A few `u16` per connection is not worth reclaiming.
    pub(crate) auto_forwarded:
        std::collections::HashMap<crate::ui::app::ForwardOwnerKey, std::collections::HashSet<u16>>,
    pub(crate) tree_scroll: gpui::ScrollHandle,
    /// A path the tree should scroll onto, and how many more renders it may
    /// take to get there. The row is usually not drawn yet when the request is
    /// made — its parents were only just expanded and their listings are still
    /// on their way — so the index has to be recomputed until it appears. The
    /// countdown is what stops a path that never arrives from being compared
    /// against every row forever.
    pub(crate) tree_reveal: Option<(PathBuf, u8)>,
}

/// How many renders a reveal waits for its row to show up. Generous: it costs
/// one path comparison per row, and a cold directory listing over SSH can take
/// a moment.
pub(crate) const TREE_REVEAL_RENDERS: u8 = 60;

/// How often the background watch looks for new remote listening ports.
const PORT_WATCH_POLL: std::time::Duration = std::time::Duration::from_millis(5000);

/// How many newly-seen ports one poll may forward. A dev server brings up one
/// or two; a number this side of a dozen is a process opening listeners in a
/// loop, and forwarding all of them helps nobody.
const AUTO_FORWARD_BURST: usize = 4;

/// The active pane's forwarding route and process-query host.
#[derive(Clone)]
pub(crate) struct PaneForwardCtx {
    pub(crate) pane_id: u64,
    /// `Some` when the pane has somewhere to hold managed forwards.
    pub(crate) route: Option<crate::ui::app::ForwardRoute>,
    pub(crate) port_route: crate::terminal::view::PortRoute,
    /// The host that owns this pane's processes, when that is not this
    /// machine. A remote workspace's panes live in the peer's registry, so the
    /// local daemon — which is what `query_procs` asks — has never heard of
    /// them and answers with an empty list.
    pub(crate) host: Option<crate::ui::host_ops::SharedHost>,
}

impl Tty7App {
    pub(crate) fn right_panel_open(&self, _cx: &gpui::App) -> bool {
        self.right_panel_visible && !self.tabs.is_empty()
    }

    /// What the sidebar has reserved, from this panel's point of view.
    pub(crate) fn sidebar_floor(&self, cx: &gpui::App) -> f32 {
        if self.sidebar_open(cx) {
            crate::ui::tab_sidebar::MIN_SIDEBAR_WIDTH
        } else {
            0.
        }
    }

    /// The narrowest the panel may be drawn: `MIN_WIDTH`, or wider when the
    /// interface font or language makes the two tab labels and adjacent
    /// chrome tiles need more than that.
    pub(crate) fn right_panel_min_px(&self, window: &Window, cx: &gpui::App) -> f32 {
        MIN_WIDTH.max(right_panel_tabs_floor(window, cx))
    }

    pub(crate) fn right_panel_max_px(&self, window: &Window, cx: &gpui::App) -> f32 {
        crate::ui::app::side_panel_max(
            window.viewport_size().width.as_f32(),
            self.right_panel_min_px(window, cx),
            self.sidebar_floor(cx) + self.document_floor(cx),
        )
    }

    pub(crate) fn right_panel_px(&self, window: &Window, cx: &gpui::App) -> f32 {
        self.right_panel_width.get().clamp(
            self.right_panel_min_px(window, cx),
            self.right_panel_max_px(window, cx),
        )
    }

    pub(crate) fn toggle_right_panel(&mut self, cx: &mut Context<Self>) {
        let next = !self.right_panel_visible;
        self.right_panel_visible = next;
        self.update_config(cx, |cfg| cfg.right_panel_visible = next);
        cx.notify();
    }

    pub(crate) fn find_in_files(&mut self, cx: &mut Context<Self>) {
        self.panel_search.focus_pending = true;
        self.set_right_panel_tab(RightPanelTab::Files, cx);
    }

    pub(crate) fn set_right_panel_tab(&mut self, tab: RightPanelTab, cx: &mut Context<Self>) {
        self.right_panel_tab = tab;
        self.right_panel_visible = true;
        self.update_config(cx, |cfg| {
            cfg.right_panel_tab = tab;
            cfg.right_panel_visible = true;
        });
        cx.notify();
    }

    pub(crate) fn render_right_panel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let panel_open = self.right_panel_open(cx);
        // A closing panel keeps drawing its last frame: the wrapper clips it
        // away over the slide, so the content must not blank first.
        let drawing = panel_open
            || self
                .panel_anim
                .active(crate::ui::panel_anim::PanelSlot::Details);
        if let Some(open) = self.sftp_panel.open_pane_id
            && (!panel_open || self.remote_files_pane(window, cx).map(|(id, _)| id) != Some(open))
        {
            self.sftp_close_browser(cx);
        }
        if !drawing {
            self.sftp_panel.panel_was_closed = true;
            return None;
        }
        let width = self.right_panel_px(window, cx);
        let tab = self.right_panel_tab;

        let body = match tab {
            RightPanelTab::Scm => self.render_panel_scm(window, cx),
            RightPanelTab::Files => self.render_panel_files(window, cx),
        };
        let (backing, handle) = self.right_panel_resize(window, cx);

        Some(
            v_flex()
                .id("right-panel")
                .relative()
                .flex_none()
                .w(px(width))
                .h_full()
                .child(backing)
                .bg(crate::ui::theme::workspace_surface_color(cx))
                .border_l(crate::ui::theme::hairline(window))
                .border_color(cx.theme().sidebar_border)
                .children(cfg!(target_os = "macos").then(|| {
                    let row = h_flex()
                        .id("right-panel-titlebar-drag")
                        .flex_none()
                        // Match TitleBar's content height, including its invisible
                        // bottom border. Otherwise this row centres the toggle
                        // half a point below its position in the closed panel.
                        .title_strip_row(px(TITLE_BAR_BORDER), cx.theme().transparent);
                    crate::ui::app::window_move_gesture(
                        row,
                        "right-panel-titlebar-drag",
                        window,
                        cx,
                    )
                    .on_double_click(|_, window, _| window.titlebar_double_click())
                    .items_center()
                    .pl(px(TAB_ROW_LEAD))
                    .relative()
                    .children(self.right_panel_tabs(cx))
                    .child(div().flex_1())
                    // The toggle keeps its title-bar size and trailing inset
                    // when it moves into this row, so opening the panel does
                    // not shift the glyph or change its hover target.
                    .child(self.window_chrome(cx))
                }))
                // Only Files steps down 8px. Its first row
                // is a filled search well whose top edge is the first thing you
                // see, so it wants air under the tab row. Git opens on
                // bare text centred in a 28px row, which already sits ~7px
                // under the row's top; adding 8 more put its first line a
                // visible step lower than the Files well beside them.
                .children(
                    (cfg!(target_os = "macos") && tab == RightPanelTab::Files)
                        .then(|| div().flex_none().h(px(8.))),
                )
                .child(body)
                .children(self.sftp_transfers_footer(cx))
                .child(handle)
                .into_any_element(),
        )
    }

    fn right_panel_resize(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> (AnyElement, AnyElement) {
        use gpui::{Bounds, MouseButton, MouseMoveEvent, MouseUpEvent, Pixels, canvas};
        use std::cell::Cell as StdCell;
        use std::rc::Rc;

        let container: Rc<StdCell<Option<Bounds<Pixels>>>> = Rc::new(StdCell::new(None));
        // Read while there is still a `cx` to read it from: the drag handler
        // below only ever sees a `Window`, and the cap it clamps against has to
        // be the same one the layout applies or the panel springs back from
        // wherever it was dropped.
        let others_floor = self.sidebar_floor(cx) + self.document_floor(cx);
        let own_floor = self.right_panel_min_px(window, cx);
        let backing = canvas(
            {
                let container = container.clone();
                move |bounds, _window, _cx| container.set(Some(bounds))
            },
            {
                let container = container.clone();
                let width_cell = self.right_panel_width.clone();
                let dragging = self.right_panel_dragging.clone();
                move |_bounds, _state, window, _cx| {
                    window.on_mouse_event({
                        let container = container.clone();
                        let width_cell = width_cell.clone();
                        let dragging = dragging.clone();
                        move |ev: &MouseMoveEvent, _phase, window, _cx| {
                            if !dragging.get() {
                                return;
                            }
                            let Some(b) = container.get() else {
                                return;
                            };
                            let right = b.origin.x + b.size.width;
                            let raw = (right - ev.position.x).as_f32();
                            let max = crate::ui::app::side_panel_max(
                                window.viewport_size().width.as_f32(),
                                own_floor,
                                others_floor,
                            );
                            width_cell.set(raw.clamp(own_floor, max));
                            window.refresh();
                        }
                    });
                    window.on_mouse_event({
                        let width_cell = width_cell.clone();
                        let dragging = dragging.clone();
                        move |_ev: &MouseUpEvent, _phase, window, cx| {
                            if !dragging.get() {
                                return;
                            }
                            dragging.set(false);
                            let w = width_cell.get();
                            let cfg = cx.global_mut::<Config>();
                            if cfg.right_panel_width != w {
                                cfg.right_panel_width = w;
                                cfg.save();
                            }
                            window.refresh();
                        }
                    });
                }
            },
        )
        .absolute()
        .size_full()
        .into_any_element();

        let active = self.right_panel_dragging.get();
        let handle = div()
            .group("right-panel-resize")
            .occlude()
            .absolute()
            .top_0()
            .left(px(-(RESIZE_HANDLE_WIDTH / 2.)))
            .w(px(RESIZE_HANDLE_WIDTH))
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .cursor_col_resize()
            .child(
                div()
                    .w(px(1.))
                    .h_full()
                    .when(active, |d| d.bg(cx.theme().drag_border))
                    .group_hover("right-panel-resize", |s| s.bg(cx.theme().drag_border)),
            )
            .on_mouse_down(MouseButton::Left, {
                let dragging = self.right_panel_dragging.clone();
                move |_ev, window, _cx| {
                    dragging.set(true);
                    window.refresh();
                }
            })
            .into_any_element();

        (backing, handle)
    }

    pub(crate) fn panel_title(
        &self,
        text: &str,
        count: Option<String>,
        trailing: Option<AnyElement>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let has_trailing = trailing.is_some();
        let tabs = (!cfg!(target_os = "macos")).then(|| self.right_panel_tabs(cx));
        if tabs.is_none() && !has_trailing {
            return div().flex_none().into_any_element();
        }
        let row = crate::ui::app::window_move_gesture(
            h_flex().id("panel-title"),
            "panel-title-drag",
            window,
            cx,
        );
        row.flex_none()
            .h(px(if tabs.is_some() { TAB_ROW_HEIGHT } else { 32. }))
            .items_center()
            // A bare heading is text, so it starts on the text column; a
            // trailing tile lines its glyph up with that column's far edge.
            .pl(px(TEXT_INSET))
            .pr(px(match (&tabs, has_trailing) {
                (Some(_), _) => tile_trailing_inset(),
                (None, true) => TEXT_INSET - crate::ui::app::TILE_PAD_SM,
                (None, false) => TEXT_INSET,
            }))
            // Where the tabs live in this row, they are the heading: the
            // current one already names the panel, and the Changes tab carries
            // the count. No rule under it — the body's own inset is the break.
            .when(tabs.is_some(), |row| row.pl(px(TAB_ROW_LEAD)))
            .when(tabs.is_none(), |row| {
                row.child(
                    h_flex()
                        .flex_shrink_0()
                        .items_baseline()
                        .gap(px(7.))
                        .child(
                            // The title step of the panel ramp, SEMIBOLD and
                            // uppercased. It reads as a label rather than as
                            // content because of the weight and the caps.
                            div()
                                .text_size(rems(META))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(cx.theme().muted_foreground)
                                .child(text.to_string()),
                        )
                        .when_some(count, |this, c| {
                            this.child(
                                // A count is a token hanging off the heading, not
                                // part of it: one step down, mono, regular weight.
                                div()
                                    .text_size(rems(META_MONO))
                                    .font_family(cx.theme().mono_font_family.clone())
                                    .text_color(cx.theme().muted_foreground)
                                    .child(c),
                            )
                        }),
                )
            })
            // Leading, as on macOS: the tabs open the row and whatever the
            // panel adds trails at the far end.
            .when_some(tabs, |this, tiles| {
                this.child(
                    h_flex()
                        .flex_shrink_0()
                        // Full height, so each tab's click target is the
                        // whole row, not a glyph-tall strip in its middle.
                        .h_full()
                        .items_center()
                        .children(tiles),
                )
            })
            .child(div().flex_1().min_w_0())
            .when_some(trailing, |this, t| this.child(t))
            .into_any_element()
    }

    pub(crate) fn panel_search(
        &self,
        input: &gpui::Entity<gpui_component::input::InputState>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.panel_search_with(input, None, cx)
    }

    /// [`Self::panel_search`] with a tile at its far end. The tile sits
    /// inside the same `CONTENT_INSET` as the well, so its edge lines up with
    /// the commit row's above it.
    pub(crate) fn panel_search_with(
        &self,
        input: &gpui::Entity<gpui_component::input::InputState>,
        trailing: Option<AnyElement>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let has_trailing = trailing.is_some();
        // A 28px well on the faint fill, the same shape as the sidebar's
        // search. It is a list-width box — `CONTENT_INSET` from both edges,
        // like a row's hover fill — with its glyph on the text column.
        h_flex()
            .flex_none()
            .items_center()
            .h(px(SEARCH_WELL_H))
            .px(px(CONTENT_INSET))
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .gap(px(SEARCH_GLYPH_GAP))
                    .h(px(SEARCH_WELL_H))
                    .px(px(ROW_INSET))
                    .rounded(px(7.))
                    .bg(cx.theme().muted)
                    .child(
                        Icon::new(IconName::Search)
                            .size(px(SEARCH_GLYPH))
                            .text_color(cx.theme().muted_foreground),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            // A filter with no way out of it but selecting the
                            // text and deleting it is a filter people leave on
                            // and then wonder where their files went. The
                            // button only exists while there is something to
                            // clear, so an empty field still reads as one line
                            // of chrome.
                            .child(Input::new(input).appearance(false).xsmall().cleanable(true)),
                    ),
            )
            .when(has_trailing, |row| row.gap(px(6.)))
            .children(trailing)
            .into_any_element()
    }

    pub(crate) fn panel_empty(
        &self,
        text: &str,
        hint: Option<&str>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        v_flex()
            .px(px(TEXT_INSET))
            .py(px(4.))
            .gap(px(3.))
            .text_size(rems(TEXT))
            .text_color(muted)
            .child(text.to_string())
            .children(hint.map(|h| {
                div()
                    .text_size(rems(META))
                    .text_color(muted)
                    .child(h.to_string())
            }))
            .into_any_element()
    }

    /// Which pane to watch, how its forward requests reach a daemon, and
    /// whether its loopback ports need tunnelling.
    pub(crate) fn pane_forward_ctx(
        &self,
        window: &Window,
        cx: &gpui::App,
    ) -> Option<PaneForwardCtx> {
        let leaf = self.tabs.get(self.active)?.detail_pane(window, cx)?;
        let view = leaf.read(cx);
        // A pane holds managed forwards once it has somewhere to hold them:
        // a live native-ssh connection, or the workspace's shared one.
        let connected_ssh = view
            .remote_context()
            .is_some_and(|c| c.kind == crate::daemon::protocol::RemoteKind::NativeSsh)
            && matches!(
                view.ssh_phase(),
                Some(crate::daemon::protocol::SshPhase::Connected)
            );
        Some(PaneForwardCtx {
            pane_id: view.pane_id,
            route: (connected_ssh || view.workspace().is_some()).then(|| view.forward_route()),
            port_route: view.port_route(cx),
            host: (!view.host_id().is_local())
                .then(|| view.host(cx))
                .flatten(),
        })
    }

    /// Point the port watch at whatever pane is in front, once a frame.
    ///
    /// Driven from the app's own render rather than the panel's: the watch has
    /// to run with the panel shut, which is exactly when a new port appearing
    /// is worth saying something about.
    pub(crate) fn sync_port_watch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ctx = self.pane_forward_ctx(window, cx);
        self.right_panel.port_route = ctx
            .as_ref()
            .map_or(crate::terminal::view::PortRoute::Blocked, |c| c.port_route);
        let pane_id = ctx.as_ref().map(|c| c.pane_id);
        let host = ctx.as_ref().and_then(|c| c.host.clone());
        let route = ctx.and_then(|c| c.route);
        self.sync_procs(pane_id, route, host, cx);
    }

    /// Whether the poll has to keep going with the panel closed: on a pane
    /// whose ports need forwarding, noticing a new listener *is* the feature,
    /// and a shut panel is not a reason to stop looking.
    fn watching_ports(&self) -> bool {
        self.right_panel.port_route == crate::terminal::view::PortRoute::Forward
            && self.right_panel.procs_forwards.is_some()
    }

    /// Forward the loopback ports this poll saw for the first time, and say so.
    ///
    /// Once per port, not once per poll: a forward the user then deletes stays
    /// deleted. Only ports bound somewhere this machine could reach through the
    /// tunnel — a listener pinned to one of the far host's own interfaces is a
    /// different service, and guessing at it would build a forward to nothing.
    fn auto_forward_ports(&mut self, cx: &mut Context<Self>) {
        if !self.watching_ports() {
            return;
        }
        let Some(route) = self.right_panel.procs_forwards.clone() else {
            return;
        };
        // Read out before the ledger is touched: the ports are behind the same
        // borrow the `seen` entry needs.
        let listening: Vec<u16> = match self.right_panel.procs.as_ref() {
            Some(procs) => procs
                .ports
                .iter()
                .filter(|p| crate::daemon::protocol::PortEntry::reaches_loopback(&p.addr))
                .map(|p| p.port)
                .collect(),
            None => return,
        };
        let owner = route.owner_key();
        let seen = self.right_panel.auto_forwarded.entry(owner).or_default();
        let mut fresh: Vec<u16> = Vec::new();
        for port in listening {
            if fresh.len() >= AUTO_FORWARD_BURST {
                break;
            }
            // Two rows may name one port — a pre-forking server puts one per
            // worker on screen — and they want one forward between them.
            if seen.contains(&port) || fresh.contains(&port) {
                continue;
            }
            fresh.push(port);
        }
        if fresh.is_empty() {
            return;
        }
        // Claimed before the request goes out, so the next poll — two seconds
        // away, and this round trip crosses a network — does not ask again.
        seen.extend(fresh.iter().copied());
        cx.spawn(async move |this, cx| {
            let built = cx
                .background_executor()
                .spawn(async move {
                    fresh
                        .into_iter()
                        .map(|port| {
                            let local = route
                                .ensure_loopback("127.0.0.1", port)
                                .map(|f| f.local_port);
                            (port, local)
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            let _ = this.update_in(cx, |app, window, cx| {
                for (port, local) in built {
                    match local {
                        Ok(local) => window.push_notification(
                            t_fmt(
                                L10nKey::PortAutoForwarded,
                                &[("port", &port.to_string()), ("local", &local.to_string())],
                            ),
                            cx,
                        ),
                        Err(e) => {
                            // Usually a connection that is not up yet. Let the
                            // next poll try again rather than writing the port
                            // off for the life of the pane.
                            log::debug!("could not auto-forward :{port}: {e}");
                            if let Some(seen) = app.right_panel.auto_forwarded.get_mut(&owner) {
                                seen.remove(&port);
                            }
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn sync_procs(
        &mut self,
        pane_id: Option<u64>,
        forwards: Option<crate::ui::app::ForwardRoute>,
        host: Option<crate::ui::host_ops::SharedHost>,
        cx: &mut Context<Self>,
    ) {
        let Some(pane_id) = pane_id else { return };
        let source_changed = self.right_panel.procs_pane != Some(pane_id)
            || self.right_panel.procs_host.as_ref().map(|host| host.id())
                != host.as_ref().map(|host| host.id())
            || self
                .right_panel
                .procs_forwards
                .as_ref()
                .map(|route| route.owner_key())
                != forwards.as_ref().map(|route| route.owner_key());
        self.right_panel.procs_forwards = forwards;
        self.right_panel.procs_host = host.clone();
        if source_changed {
            self.right_panel.procs_pane = Some(pane_id);
            self.right_panel.procs = None;
            self.right_panel.procs_gen += 1;
            self.right_panel.procs_loading = false;
        }
        // Only remote panes that need automatic forwarding warrant polling.
        if !self.right_panel.procs_loading && self.watching_ports() {
            self.right_panel.procs_loading = true;
            let generation = self.right_panel.procs_gen;
            self.spawn_procs_query(pane_id, generation, host, cx);
        }
    }

    fn spawn_procs_query(
        &mut self,
        pane_id: u64,
        generation: u64,
        host: Option<crate::ui::host_ops::SharedHost>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let procs = cx
                .background_executor()
                .spawn(async move {
                    // A remote workspace's pane runs on the peer, so the peer
                    // is the only one that can walk its process tree; the local
                    // daemon does not have the pane at all and would answer
                    // with an empty list. `None` back from the host means it
                    // could not be asked, which is not the same as an empty
                    // answer — see `Host::pane_procs`.
                    let procs = match &host {
                        Some(host) => host.pane_procs(pane_id),
                        None => Some(crate::terminal::RemoteTerminal::query_procs(pane_id)),
                    };
                    procs
                })
                .await;
            let keep_polling = this
                .update(cx, |app, cx| {
                    if app.right_panel.procs_gen != generation {
                        return false;
                    }
                    // A failed probe leaves the last known ports in place.
                    if let Some(procs) = procs {
                        app.right_panel.procs = Some(procs);
                    }
                    cx.notify();
                    let wanted = app.watching_ports();
                    if !wanted {
                        app.right_panel.procs_loading = false;
                    }
                    wanted
                })
                .unwrap_or(false);
            if !keep_polling {
                return;
            }
            // The list and route are from the same generation. Never forward
            // a stale host's listeners through a newly selected host's route.
            let _ = this.update(cx, |app, cx| {
                if app.right_panel.procs_gen == generation {
                    app.auto_forward_ports(cx);
                }
            });
            cx.background_executor().timer(PORT_WATCH_POLL).await;
            let _ = this.update(cx, |app, cx| {
                if app.right_panel.procs_gen != generation {
                    return;
                }
                if app.watching_ports() {
                    let host = app.right_panel.procs_host.clone();
                    app.spawn_procs_query(pane_id, generation, host, cx);
                } else {
                    app.right_panel.procs_loading = false;
                }
            });
        })
        .detach();
    }

    /// The Files panel's dotfile switch: an eye in the title bar, selected
    /// while hidden files are listed. The same switch is in a row's
    /// right-click menu, where it has always lived; this one puts it where
    /// someone who has never right-clicked a row can find it.
    fn files_hidden_toggle(&self, cx: &mut Context<Self>) -> AnyElement {
        let shown = self.file_tree.show_hidden;
        let (icon, label) = match shown {
            true => (IconName::Eye, L10nKey::FileTreeContextHideDotfiles),
            false => (IconName::EyeOff, L10nKey::FileTreeContextShowDotfiles),
        };
        div()
            .occlude()
            .child(
                crate::ui::tab_strip::chrome_tile_sized(
                    Button::new("panel-files-hidden").icon(Icon::new(icon)),
                    TILE_SIZE_SM,
                    TILE_GLYPH_SM,
                    shown,
                    cx,
                )
                .rounded_md()
                .accessible_label(t(label))
                .tooltip(t(label))
                .on_click(cx.listener(|this, _, _w, cx| this.file_tree_toggle_hidden(cx))),
            )
            .into_any_element()
    }

    fn render_panel_files(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let remote = self.remote_files_pane(window, cx);
        let host = remote.as_ref().map(|(_, host)| host.clone());
        if self.sftp_sync_pane(remote.map(|(id, _)| id), window, cx) {
            return self.render_panel_sftp(host.unwrap_or_default(), window, cx);
        }

        if std::mem::take(&mut self.panel_search.focus_pending) {
            self.file_search
                .update(cx, |input, cx| input.focus(window, cx));
        }
        let hidden = self.files_hidden_toggle(cx);
        let title = self.panel_title(t(L10nKey::PanelFilesTitle), None, Some(hidden), window, cx);
        // One field for names and text alike. The switches only shape the
        // text search; names are always matched loosely.
        let toggles = self.panel_search_toggles(cx);
        let search = self.panel_search_with(&self.file_search.clone(), Some(toggles), cx);
        let rows = self.render_file_tree_rows(window, cx);
        v_flex()
            .flex_1()
            .min_h_0()
            .child(title)
            // The well and the tree are two blocks, not a field glued to its
            // list: a pause between them, the same 10px the Changes tab keeps
            // between its pinned parts.
            .child(div().flex_none().pb(px(FILES_SEARCH_GAP)).child(search))
            .child(rows)
            .into_any_element()
    }

    /// The host label for whatever the Files panel is currently showing over
    /// SFTP, for copy that has to name the machine it is about to change.
    pub(crate) fn remote_files_host(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        self.remote_files_pane(window, cx).map(|(_, host)| host)
    }

    fn remote_files_pane(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<(u64, String)> {
        use crate::daemon::protocol::{RemoteKind, SshPhase};
        let leaf = self.tabs.get(self.active)?.detail_pane(window, cx)?;
        let view = leaf.read(cx);
        let remote = view.remote_context()?;
        if remote.kind != RemoteKind::NativeSsh
            || !matches!(view.ssh_phase(), Some(SshPhase::Connected))
        {
            return None;
        }
        Some((view.pane_id, remote.target))
    }
}

/// Width of the fixed cell a git status letter is centred in: the widest
/// letter at `META_MONO`, and nothing more, so the name after it starts
/// `ROW_INSET + 10 + 8` = 18px past the row's fill edge.
///
/// Load-bearing beyond this function: `scm/panel.rs` sets its group-header
/// chevron in a box no wider than this, flush with the cell's leading edge, so
/// the group arrows sit over the status letters, and it keeps its own
/// `BADGE_W` in step. Changing it here without changing it there breaks that
/// column.
pub(crate) const BADGE_W: f32 = 10.;

/// A single-letter git status marker in a fixed-width cell.
///
/// Mono and SEMIBOLD so `M`, `A`, `D` and `U` all read as the same kind of
/// mark at a glance, and centred in a cell wide enough for the widest of them
/// at [`PANEL_TEXT_META`] — that is what makes a column of them line up
/// instead of drifting with the glyph widths.
pub(crate) fn git_badge(letter: &str, color: gpui::Hsla, mono: &gpui::SharedString) -> AnyElement {
    div()
        .flex_none()
        .w(px(BADGE_W))
        .text_center()
        .text_size(rems(META_MONO))
        .font_family(mono.clone())
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(color)
        .child(letter.to_string())
        .into_any_element()
}

/// A small filled pill around a mono token — a pid, a port number.
///
/// The padding and the radius are derived from the text size: at
/// [`PANEL_TEXT_META`] the line box is `round(10.5 × 1.618) = 17px`, so 1.5px
/// of vertical padding makes the pill 20px tall — one pixel more than the 19px
/// line of [`PANEL_TEXT`] beside it, which is what sets the height of a ports
/// row. Horizontal padding of 5px is about half an em of breathing room on
/// each side, and radius 4 is a fifth of the pill's height.
pub(crate) fn info_chip(
    text: &str,
    bg: gpui::Hsla,
    fg: gpui::Hsla,
    mono: &gpui::SharedString,
) -> AnyElement {
    div()
        .flex_none()
        .px(px(5.))
        .py(px(1.5))
        .rounded(px(4.))
        .bg(bg)
        .text_size(rems(META_MONO))
        .font_family(mono.clone())
        .text_color(fg)
        .child(text.to_string())
        .into_any_element()
}

pub fn reveal_label() -> &'static str {
    if cfg!(target_os = "macos") {
        t(L10nKey::PanelRevealInFinder)
    } else {
        t(L10nKey::PanelOpenFolder)
    }
}
