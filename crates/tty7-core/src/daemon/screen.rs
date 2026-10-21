//! Authoritative, headless terminal state. Only synthesized presentation leaves this module.
use std::fmt::Write as _;
use std::sync::Arc;

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Osc52, Term, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Processor};
use serde::{Deserialize, Serialize};

use super::protocol::WinSize;
use crate::core::osc::{OscTokenizer, OscTokenizerCheckpoint};
use crate::terminal::command_cursor::{CommandCursorStyle, CommandMark};
use crate::terminal::parked_cursor::{CursorCut, ParkedCursorRepair, ParkedCursorScanner};
use crate::terminal::prompt_reflow::{self, PromptBreak, PromptMark};
use crate::terminal::size::TermSize;

pub(crate) const HISTORY_ROWS: usize = 5000;
pub(crate) const HISTORY_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const UNAVAILABLE: &str = "authoritative terminal state unavailable: this pane was inherited without a complete parser checkpoint; open a new pane";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ScreenCursor {
    pub row: u16,
    pub col: u16,
    pub shape: String,
    pub visible: bool,
    pub blink: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ScreenModes {
    pub app_cursor: bool,
    pub app_keypad: bool,
    pub bracketed_paste: bool,
    #[serde(default)]
    pub mouse: bool,
    #[serde(default)]
    pub sgr_mouse: bool,
    #[serde(default)]
    pub alternate_scroll: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct ScreenSnapshot {
    pub revision: u64,
    pub epoch: u64,
    pub cols: u16,
    pub rows: u16,
    pub alternate: bool,
    pub cursor: ScreenCursor,
    pub modes: ScreenModes,
    pub lines: Vec<Arc<String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct HistoryLine {
    pub text: String,
    pub ansi: String,
    pub cols: u16,
}

#[derive(Clone, Debug)]
pub(crate) struct HistorySnapshot {
    pub lines: Vec<HistoryLine>,
    pub truncated: bool,
}

// Queries, clipboard writes, titles and bells deliberately have no side effects.
// Only the controlling desktop parser may reply to the PTY.
#[derive(Default, Serialize, Deserialize)]
struct Silent;
impl EventListener for Silent {}

#[derive(Clone, Copy)]
struct Size { cols: usize, rows: usize }
impl Size {
    fn new(size: WinSize) -> Self {
        Self { cols: usize::from(size.cols).max(2), rows: usize::from(size.rows).max(1) }
    }
}
impl Dimensions for Size {
    fn columns(&self) -> usize { self.cols }
    fn screen_lines(&self) -> usize { self.rows }
    fn total_lines(&self) -> usize { self.rows }
}

/// Versioned, complete parser state, not an ANSI replay or rendered-grid seed.
#[derive(Serialize, Deserialize)]
pub(crate) struct ScreenCheckpoint {
    version: u32,
    term: Term<Silent>,
    parser: Processor,
    revision: u64,
    epoch: u64,
    history_truncated: bool,
    conpty: bool,
    at_prompt: bool,
    #[serde(deserialize_with = "restore_marks")]
    marks: OscTokenizer,
    command_cursor: CommandCursorStyle,
    prompt_break: PromptBreak,
    cursor_scan: ParkedCursorScanner,
    parked_cursor: ParkedCursorRepair,
}

fn restore_marks<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<OscTokenizer, D::Error> {
    let checkpoint = OscTokenizerCheckpoint::deserialize(deserializer)?;
    Ok(OscTokenizer::from_checkpoint(&[b"133"], checkpoint))
}

#[derive(Clone, Copy)]
enum Cut { Command(CommandMark), Prompt(PromptMark), Parked(CursorCut) }

pub(crate) struct ScreenMirror {
    state: ScreenCheckpoint,
    cached: Option<Arc<ScreenSnapshot>>,
    cuts: Vec<(usize, Cut)>,
}

impl ScreenMirror {
    pub(crate) fn new(size: WinSize, conpty: bool) -> Self {
        Self::with_config(size, conpty, &crate::core::config::Config::load())
    }

    pub(crate) fn with_config(size: WinSize, conpty: bool, user: &crate::core::config::Config) -> Self {
        let mut config = crate::terminal::config::terminal_config_from_user(user);
        config.scrolling_history = HISTORY_ROWS;
        config.osc52 = Osc52::Disabled;
        config.conpty_resize = conpty;
        Self {
            state: ScreenCheckpoint {
                version: 1,
                term: Term::new(config, &Size::new(size), Silent),
                parser: Processor::new(),
                revision: 1,
                epoch: 1,
                history_truncated: false,
                conpty,
                at_prompt: false,
                marks: OscTokenizer::new(&[b"133"]),
                command_cursor: CommandCursorStyle::default(),
                prompt_break: PromptBreak::default(),
                cursor_scan: ParkedCursorScanner::new(),
                parked_cursor: ParkedCursorRepair::default(),
            },
            cached: None,
            cuts: Vec::new(),
        }
    }

    pub(crate) fn checkpoint(&self) -> std::io::Result<Vec<u8>> {
        serde_json::to_vec(&self.state).map_err(std::io::Error::other)
    }

    pub(crate) fn restore(bytes: &[u8]) -> std::io::Result<Self> {
        let mut state: ScreenCheckpoint = serde_json::from_slice(bytes).map_err(std::io::Error::other)?;
        if state.version != 1 {
            return Err(std::io::Error::other("unsupported terminal checkpoint version"));
        }
        state.term.reset_checkpoint_damage();
        // A restored image has no shared cache, even when the geometry stayed the same.
        state.epoch = state.epoch.saturating_add(1);
        state.revision = state.revision.saturating_add(1);
        Ok(Self { state, cached: None, cuts: Vec::new() })
    }

    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        if bytes.is_empty() { return; }
        let alternate = self.alternate();
        self.cuts.clear();
        self.state.marks.feed_at(bytes, |offset, payload| {
            if let Some(mark) = CommandMark::parse(payload) {
                self.cuts.push((offset, Cut::Command(mark)));
            } else if let Some(mark) = PromptMark::parse(payload) {
                self.cuts.push((offset, Cut::Prompt(mark)));
            }
        });
        if self.state.conpty {
            self.state.cursor_scan.feed(bytes, |offset, cut| self.cuts.push((offset, Cut::Parked(cut))));
            self.cuts.sort_by_key(|(offset, _)| *offset);
        }
        let state = &mut self.state;
        let mut at = 0;
        for (offset, cut) in &self.cuts {
            state.parser.advance(&mut state.term, &bytes[at..*offset]);
            match *cut {
                Cut::Command(mark) => {
                    state.command_cursor.apply(&mut state.term, mark);
                    if mark == CommandMark::Started { state.at_prompt = false; }
                }
                Cut::Prompt(mark) => {
                    state.prompt_break.apply(&mut state.term, mark);
                    state.at_prompt = true;
                }
                Cut::Parked(cut) => state.parked_cursor.apply(&mut state.term, cut),
            }
            at = *offset;
        }
        state.parser.advance(&mut state.term, &bytes[at..]);
        if self.alternate() != alternate {
            self.state.epoch = self.state.epoch.saturating_add(1);
        }
        self.state.revision = self.state.revision.saturating_add(1);
        if self.state.term.normal_grid().history_size() >= HISTORY_ROWS {
            self.state.history_truncated = true;
        }
        self.state.history_truncated |= self.state.term.limit_normal_history_bytes(HISTORY_BYTES);
    }

    pub(crate) fn resize(&mut self, size: WinSize) {
        let size = Size::new(size);
        if self.state.term.columns() == size.cols && self.state.term.screen_lines() == size.rows { return; }
        prompt_reflow::resize(&mut self.state.term, TermSize::new(size.cols, size.rows), self.state.at_prompt);
        self.state.history_truncated |= self.state.term.limit_normal_history_bytes(HISTORY_BYTES);
        self.state.epoch = self.state.epoch.saturating_add(1);
        self.state.revision = self.state.revision.saturating_add(1);
    }

    fn alternate(&self) -> bool { self.state.term.mode().contains(TermMode::ALT_SCREEN) }

    pub(crate) fn snapshot(&mut self) -> Arc<ScreenSnapshot> {
        if self.state.parser.sync_timeout().sync_timeout().is_some_and(|until| until <= std::time::Instant::now()) {
            let alternate = self.alternate();
            self.state.parser.stop_sync(&mut self.state.term);
            self.state.history_truncated |= self.state.term.limit_normal_history_bytes(HISTORY_BYTES);
            self.state.revision = self.state.revision.saturating_add(1);
            if self.alternate() != alternate { self.state.epoch = self.state.epoch.saturating_add(1); }
        }
        if let Some(cached) = &self.cached {
            if cached.revision == self.state.revision { return cached.clone(); }
        }
        let term = &mut self.state.term;
        let rows = term.screen_lines();
        let full = self.cached.as_ref().is_none_or(|old| old.epoch != self.state.epoch);
        let damaged: Vec<usize> = match term.damage() {
            TermDamage::Full => (0..rows).collect(),
            TermDamage::Partial(lines) if !full => lines.map(|line| line.line).collect(),
            TermDamage::Partial(_) => (0..rows).collect(),
        };
        let mut lines = self.cached.as_ref().filter(|_| !full)
            .map(|old| old.lines.clone()).unwrap_or_else(|| vec![Arc::new(String::new()); rows]);
        for row in damaged {
            let ansi = render_line(term, term.grid(), Line(row as i32), false).1;
            if lines[row].as_str() != ansi { lines[row] = Arc::new(ansi); }
        }
        term.reset_damage();
        let mode = *term.mode();
        let cursor = term.renderable_content().cursor;
        let style = term.cursor_style();
        let snapshot = Arc::new(ScreenSnapshot {
            revision: self.state.revision,
            epoch: self.state.epoch,
            cols: term.columns() as u16,
            rows: rows as u16,
            alternate: mode.contains(TermMode::ALT_SCREEN),
            cursor: ScreenCursor {
                row: cursor.point.line.0.max(0) as u16,
                col: cursor.point.column.0 as u16,
                shape: match style.shape {
                    CursorShape::Underline => "underline",
                    CursorShape::Beam => "bar",
                    _ => "block",
                }.into(),
                visible: mode.contains(TermMode::SHOW_CURSOR) && style.shape != CursorShape::Hidden,
                blink: style.blinking,
            },
            modes: ScreenModes {
                app_cursor: mode.contains(TermMode::APP_CURSOR),
                app_keypad: mode.contains(TermMode::APP_KEYPAD),
                bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
                mouse: mode.intersects(TermMode::MOUSE_MODE),
                sgr_mouse: mode.contains(TermMode::SGR_MOUSE),
                alternate_scroll: mode.contains(TermMode::ALTERNATE_SCROLL),
            },
            lines,
        });
        self.cached = Some(snapshot.clone());
        snapshot
    }

    pub(crate) fn history(&self) -> HistorySnapshot {
        let term = &self.state.term;
        let grid = term.normal_grid();
        let count = grid.history_size();
        // Capture the retained rows and their adjoining normal screen under the
        // same pane lock. A separately delivered live screen can be newer than
        // this immutable view and must never be spliced onto its end.
        if count == 0 { return HistorySnapshot { lines: Vec::new(), truncated: self.state.history_truncated }; }
        let retained = count.min(HISTORY_ROWS);
        let mut lines = Vec::with_capacity((retained + grid.screen_lines()).min(HISTORY_ROWS));
        let mut bytes = 0;
        let mut truncated = self.state.history_truncated || count > retained;
        for index in (-(retained as i32)..grid.screen_lines() as i32).rev() {
            let (text, ansi) = render_line(term, grid, Line(index), true);
            let len = text.len() + ansi.len();
            if lines.len() == HISTORY_ROWS || bytes + len > HISTORY_BYTES { truncated = true; break; }
            bytes += len;
            lines.push(HistoryLine { text, ansi, cols: grid.columns() as u16 });
        }
        lines.reverse();
        HistorySnapshot { lines, truncated }
    }
}

/// Deliberately emit only SGR, horizontal positioning, and printable cell text.
/// Each cell is positioned from the model, never from the browser's Unicode tables.
fn render_line(term: &Term<Silent>, grid: &Grid<Cell>, row: Line, with_text: bool) -> (String, String) {
    let mut text = String::new();
    let mut ansi = String::from("\x1b[0m");
    let mut previous: Option<&Cell> = None;
    for col in 0..grid.columns() {
        let cell = &grid[row][Column(col)];
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) { continue; }
        let _ = write!(ansi, "\x1b[{}G", col + 1);
        if previous.is_none_or(|old| old.fg != cell.fg || old.bg != cell.bg || old.flags != cell.flags || old.underline_color() != cell.underline_color()) {
            ansi.push_str("\x1b[0m");
            for (flag, code) in [(Flags::BOLD, "1"), (Flags::DIM, "2"), (Flags::ITALIC, "3"),
                (Flags::INVERSE, "7"), (Flags::HIDDEN, "8"), (Flags::STRIKEOUT, "9")] {
                if cell.flags.contains(flag) { let _ = write!(ansi, "\x1b[{code}m"); }
            }
            let underline = if cell.flags.contains(Flags::UNDERCURL) { Some("4:3") }
                else if cell.flags.contains(Flags::DOTTED_UNDERLINE) { Some("4:4") }
                else if cell.flags.contains(Flags::DASHED_UNDERLINE) { Some("4:5") }
                else if cell.flags.contains(Flags::DOUBLE_UNDERLINE) { Some("4:2") }
                else if cell.flags.contains(Flags::UNDERLINE) { Some("4") } else { None };
            if let Some(code) = underline { let _ = write!(ansi, "\x1b[{code}m"); }
            emit_color(&mut ansi, term, cell.fg, 38);
            emit_color(&mut ansi, term, cell.bg, 48);
            if let Some(color) = cell.underline_color() { emit_color(&mut ansi, term, color, 58); }
        }
        let ch = if cell.c.is_control() || cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) { ' ' } else { cell.c };
        ansi.push(ch);
        if with_text { text.push(ch); }
        for ch in cell.zerowidth().into_iter().flatten().filter(|ch| !ch.is_control()) {
            ansi.push(*ch);
            if with_text { text.push(*ch); }
        }
        previous = Some(cell);
    }
    ansi.push_str("\x1b[0m");
    text.truncate(text.trim_end_matches(' ').len());
    (text, ansi)
}

fn emit_color(out: &mut String, term: &Term<Silent>, color: Color, target: u8) {
    let palette_index = match color { Color::Named(color) => Some(color as usize), Color::Indexed(index) => Some(index as usize), _ => None };
    let color = palette_index.and_then(|index| term.colors()[index]).map(Color::Spec).unwrap_or(color);
    match color {
        Color::Spec(rgb) => { let _ = write!(out, "\x1b[{target};2;{};{};{}m", rgb.r, rgb.g, rgb.b); }
        Color::Indexed(index) => { let _ = write!(out, "\x1b[{target};5;{index}m"); }
        Color::Named(NamedColor::Foreground) => { if target == 38 { out.push_str("\x1b[39m"); } }
        Color::Named(NamedColor::Background) => { if target == 48 { out.push_str("\x1b[49m"); } }
        Color::Named(color) => {
            let index = color as usize;
            if index < 16 {
                if target == 38 || target == 48 {
                    let base = usize::from(target) - 8;
                    let code = if index < 8 { base + index } else { base + 60 + index - 8 };
                    let _ = write!(out, "\x1b[{code}m");
                } else {
                    let _ = write!(out, "\x1b[{target};5;{index}m");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn size(cols: u16, rows: u16) -> WinSize { WinSize { cols, rows, cell_w: 0, cell_h: 0 } }
    fn text(mirror: &ScreenMirror, row: i32) -> String { render_line(&mirror.state.term, mirror.state.term.grid(), Line(row), true).0 }

    #[test]
    fn split_utf8_and_escape_sequences_match_whole_stream() {
        let bytes = "ab界e\u{301}\x1b[31mR\x1b[2;3H!\x1b[?1h\x1b[?2004h".as_bytes();
        let mut whole = ScreenMirror::with_config(size(12, 3), false, &crate::core::config::Config::default());
        whole.feed(bytes);
        for split in 0..=bytes.len() {
            let mut mirror = ScreenMirror::with_config(size(12, 3), false, &crate::core::config::Config::default());
            mirror.feed(&bytes[..split]);
            mirror.feed(&bytes[split..]);
            let actual = mirror.snapshot();
            let expected = whole.snapshot();
            assert_eq!(actual.lines, expected.lines);
            assert_eq!(actual.cursor, expected.cursor);
            assert_eq!(actual.modes, expected.modes);
        }
    }

    #[test]
    fn rows_are_shared_and_clear_cursor_modes_and_epoch_are_state_derived() {
        let mut mirror = ScreenMirror::with_config(size(10, 3), false, &crate::core::config::Config::default());
        mirror.feed(b"abc\r\nsecond");
        let before = mirror.snapshot();
        assert!(Arc::ptr_eq(&before, &mirror.snapshot()));
        mirror.feed(b"\x1b[1;2HX\x1b[?25l\x1b[5 q\x1b=");
        let after = mirror.snapshot();
        assert!(Arc::ptr_eq(&before.lines[2], &after.lines[2]));
        assert_eq!(text(&mirror, 0), "aXc");
        assert!(!after.cursor.visible);
        assert_eq!(after.cursor.shape, "bar");
        assert!(after.modes.app_keypad);
        assert_eq!(after.epoch, before.epoch);
        mirror.feed(b"\x1b[2J\x1b[H");
        assert_eq!(text(&mirror, 0), "");
        mirror.resize(size(5, 2));
        assert!(mirror.snapshot().epoch > after.epoch);
    }

    #[test]
    fn alternate_history_is_primary_and_views_are_immutable() {
        let mut mirror = ScreenMirror::with_config(size(8, 2), false, &crate::core::config::Config::default());
        mirror.feed(b"one\r\ntwo\r\nthree\r\nfour");
        let history = mirror.history();
        let before = mirror.snapshot();
        mirror.feed(b"\x1b[?1049hALT");
        assert!(mirror.snapshot().alternate);
        assert!(mirror.snapshot().epoch > before.epoch);
        assert_eq!(mirror.history().lines[0].text, history.lines[0].text);
        mirror.feed(b"\x1b[?1049l\r\nfive");
        assert_eq!(history.lines[0].text, "one");
        assert_eq!(text(&mirror, 1), "five");
    }

    #[test]
    fn synthesized_wide_combining_rows_round_trip_without_queries() {
        let mut mirror = ScreenMirror::with_config(size(14, 2), false, &crate::core::config::Config::default());
        mirror.feed("界e\u{301}❤\u{fe0f}\x1b[38;2;1;2;3mR\x1b[6n\x1b]52;c;YWJj\x07".as_bytes());
        let snapshot = mirror.snapshot();
        let mut painted = ScreenMirror::with_config(size(14, 2), false, &crate::core::config::Config::default());
        painted.feed(snapshot.lines[0].as_bytes());
        assert_eq!(text(&mirror, 0), text(&painted, 0));
        assert!(!snapshot.lines[0].contains("52;"));
        assert!(!snapshot.lines[0].contains("6n"));
        assert!(!snapshot.lines[0].contains('\n'));
    }

    #[test]
    fn checkpoint_preserves_both_grids_saved_cursor_and_partial_parser_input() {
        let prefix = "primary\x1b7\r\nline\x1b[?1049h界\x1b[38;2;1;".as_bytes();
        let suffix = b"2;3mZ\x1b[?1049l\x1b8!";
        let mut source = ScreenMirror::with_config(size(16, 3), false, &crate::core::config::Config::default());
        source.feed(prefix);
        let mut restored = ScreenMirror::restore(&source.checkpoint().unwrap()).unwrap();
        source.feed(suffix);
        restored.feed(suffix);
        assert_eq!(source.snapshot().lines, restored.snapshot().lines);
        assert_eq!(source.snapshot().cursor, restored.snapshot().cursor);
        let mut partial = ScreenMirror::with_config(size(16, 3), false, &crate::core::config::Config::default());
        partial.feed(&[0xe7, 0x95]);
        let mut restored = ScreenMirror::restore(&partial.checkpoint().unwrap()).unwrap();
        restored.feed(&[0x8c]);
        assert_eq!(text(&restored, 0), "界");
    }

    #[test]
    fn history_snapshot_obeys_row_and_byte_budgets() {
        let mut mirror = ScreenMirror::with_config(size(8, 2), false, &crate::core::config::Config::default());
        for _ in 0..HISTORY_ROWS + 10 { mirror.feed(b"12345678\r\n"); }
        let history = mirror.history();
        assert!(history.truncated);
        assert!(history.lines.len() <= HISTORY_ROWS);
        assert!(history.lines.iter().map(|line| line.text.len() + line.ansi.len()).sum::<usize>() <= HISTORY_BYTES);
    }

    #[test]
    fn phone_history_keeps_the_phone_grid_width_without_losing_wrapped_text() {
        let mut mirror = ScreenMirror::with_config(size(80, 3), false, &crate::core::config::Config::default());
        let expected = "x".repeat(2100);
        mirror.feed(expected.as_bytes());
        mirror.feed(b"\r\nend");
        mirror.resize(size(37, 3));
        let history = mirror.history();
        assert!(history.lines.iter().all(|line| line.cols == 37));
        assert_eq!(history.lines.iter().filter(|line| line.text != "end").map(|line| line.text.as_str()).collect::<String>(), expected);
        assert_eq!(history.lines.last().unwrap().text, "end");
    }

    #[test]
    fn history_includes_the_adjoining_normal_screen_in_one_immutable_view() {
        let mut mirror = ScreenMirror::with_config(size(16, 2), false, &crate::core::config::Config::default());
        mirror.feed(b"first\r\nsecond");
        assert!(mirror.history().lines.is_empty(), "one screen needs no history overlay");
        mirror.feed(b"\r\nthird");
        let captured = mirror.history();
        mirror.feed(b"\r\nfourth");
        let captured_text: Vec<_> = captured.lines.iter().map(|line| line.text.trim_end()).collect();
        assert_eq!(captured_text, ["first", "second", "third"]);
        let newer = mirror.history();
        let newer_text: Vec<_> = newer.lines.iter().map(|line| line.text.trim_end()).collect();
        assert_eq!(newer_text, ["first", "second", "third", "fourth"]);
        mirror.feed(b"\x1b[?1049hforeign alternate screen");
        let alternate = mirror.history();
        let alternate_text: Vec<_> = alternate.lines.iter().map(|line| line.text.trim_end()).collect();
        assert_eq!(alternate_text, newer_text, "shell history ends with its normal screen, not the alternate program");
    }

    #[test]
    fn cached_row_patches_paint_the_same_cells_as_a_full_snapshot() {
        fn paint(target: &mut ScreenMirror, snapshot: &ScreenSnapshot, base: Option<&ScreenSnapshot>) {
            for (row, ansi) in snapshot.lines.iter().enumerate() {
                if base.is_some_and(|old| old.lines[row] == *ansi) { continue; }
                target.feed(format!("\x1b[{};1H\x1b[2K{}", row + 1, ansi).as_bytes());
            }
        }
        let mut source = ScreenMirror::with_config(size(16, 3), false, &crate::core::config::Config::default());
        source.feed("ab界e\u{301}\x1b[31mR\r\nsecond".as_bytes());
        let first = source.snapshot();
        let mut painted = ScreenMirror::with_config(size(16, 3), false, &crate::core::config::Config::default());
        paint(&mut painted, &first, None);
        source.feed(b"\x1b[1;2H\x1b[38;2;9;8;7m!\x1b[2;3H\x1b[K");
        let next = source.snapshot();
        paint(&mut painted, &next, Some(&first));
        for row in 0..3 {
            for col in 0..16 {
                let expected = &source.state.term.grid()[Line(row)][Column(col)];
                let actual = &painted.state.term.grid()[Line(row)][Column(col)];
                assert_eq!(actual.c, expected.c);
                assert_eq!(actual.fg, expected.fg);
                assert_eq!(actual.bg, expected.bg);
                assert_eq!(actual.zerowidth(), expected.zerowidth());
            }
        }
    }

    #[test]
    fn primary_history_byte_accounting_survives_alt_and_invalidates_mutated_rows() {
        let mut mirror = ScreenMirror::with_config(size(8, 2), false, &crate::core::config::Config::default());
        mirror.feed(b"one\r\ntwo\r\nthree\r\nfour");
        assert_eq!(mirror.state.term.normal_grid().history_size(), 2);
        assert!(!mirror.state.term.limit_normal_history_bytes(4096));
        // Mutating a cached row must invalidate its footprint, including heap combining marks.
        for _ in 0..2048 {
            mirror.state.term.grid_mut()[Line(-1)][Column(0)].push_zerowidth('\u{301}');
        }
        mirror.feed(b"\x1b[?1049h\x1b[HALT");
        assert!(mirror.state.term.limit_normal_history_bytes(4096));
        assert!(mirror.alternate());
        assert_eq!(text(&mirror, 0), "ALT");
        assert_eq!(mirror.state.term.normal_grid().history_size(), 0);
    }

    #[test]
    fn checkpoints_keep_partial_shell_marks_and_prompt_cursor_repairs() {
        let mut source = ScreenMirror::with_config(size(16, 3), false, &crate::core::config::Config::default());
        source.feed(b"\x1b[6 q\x1b]133;C\x07\x1b[2 q\x1b]133;");
        let mut restored = ScreenMirror::restore(&source.checkpoint().unwrap()).unwrap();
        source.feed(b"D;0\x07$ ");
        restored.feed(b"D;0\x07$ ");
        assert_eq!(source.snapshot().cursor.shape, "bar");
        assert_eq!(source.snapshot().cursor, restored.snapshot().cursor);
        assert_eq!(source.snapshot().lines, restored.snapshot().lines);
    }
}
