//! Copy rejoins the rows a program wrapped itself. Claude Code (Ink) wraps at
//! the pane's width by writing CRLF plus the block's indent, so the grid never
//! sets WRAPLINE on those rows and a plain copy keeps every break. The rule
//! was measured on 16 captures of Claude panes; `fixtures/` holds three.

use alacritty_terminal::Term;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::Flags;
use gpui::Context;
use unicode_width::UnicodeWidthChar;

use super::TerminalView;
use crate::core::cli_agent::CLIAgent;
use crate::core::config::Config;

/// One line as the copy sees it: the grid's rows up to the next one without
/// WRAPLINE, read from column 0 whatever the selection's start.
struct Row {
    text: String,
    /// Where the last physical row's text ends, in cells.
    end: usize,
}

/// What `copy_selection` copies: the selection with Claude's wrapped rows
/// joined, in a Claude pane with `copy_join_wrapped` on and `raw` off. Every
/// other program's output is copied as the grid holds it: `ls` columns,
/// separators and an editor's own wraps all look like Claude's.
pub(super) fn selection_text(
    view: &TerminalView,
    raw: bool,
    cx: &Context<TerminalView>,
) -> Option<String> {
    let join = !raw
        && cx.global::<Config>().copy_join_wrapped
        && view.terminal.foreground_agent() == Some(CLIAgent::Claude);
    let term = view.terminal.term.lock();
    if join {
        joined(&term)
    } else {
        term.selection_to_string()
    }
}

/// The selection's text with app-wrapped rows joined. Any doubt (block
/// selection, a row count that doesn't line up) copies raw.
fn joined<T>(term: &Term<T>) -> Option<String> {
    let raw = term.selection_to_string()?;
    let Some(range) = term.selection.as_ref().and_then(|s| s.to_range(term)) else {
        return Some(raw);
    };
    if range.is_block {
        return Some(raw);
    }
    let rows = grid_rows(term, range.start.line, range.end.line);
    Some(rejoin(&raw, &rows, term.columns()))
}

/// `raw` split at its newlines, which must be one per row boundary.
fn rejoin(raw: &str, rows: &[Row], cols: usize) -> String {
    // A Lines selection ends in a newline its rows don't account for.
    let (body, tail) = match raw.strip_suffix('\n') {
        Some(body) if body.split('\n').count() == rows.len() => (body, "\n"),
        _ => (raw, ""),
    };
    let parts: Vec<&str> = body.split('\n').collect();
    if parts.len() != rows.len() {
        return raw.to_string();
    }
    apply(&parts, &joins(rows, cols)) + tail
}

fn grid_rows<T>(term: &Term<T>, first: Line, last: Line) -> Vec<Row> {
    let cols = term.columns();
    let mut rows = Vec::new();
    let mut text = String::new();
    let mut line = first;
    while line <= last {
        let grid_row = &term.grid()[line];
        text += &term.bounds_to_string(
            Point::new(line, Column(0)),
            Point::new(line, Column(cols - 1)),
        );
        let wraps = grid_row[Column(cols - 1)].flags.contains(Flags::WRAPLINE);
        if !wraps || line == last {
            let end = (0..cols)
                .rev()
                .find(|&c| {
                    let cell = &grid_row[Column(c)];
                    cell.c != ' ' && !cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
                })
                .map_or(0, |c| {
                    c + 1 + grid_row[Column(c)].flags.contains(Flags::WIDE_CHAR) as usize
                });
            rows.push(Row {
                text: std::mem::take(&mut text).trim_end_matches('\n').to_string(),
                end,
            });
        }
        line += 1;
    }
    rows
}

/// What goes between two rows.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Sep {
    Newline,
    Space,
    /// Ink broke inside a word: a row that is one word, or wide (CJK) text,
    /// which wraps between any two characters.
    Nothing,
}

/// For each boundary between `rows[i]` and `rows[i + 1]`, what goes there.
fn joins(rows: &[Row], cols: usize) -> Vec<Sep> {
    let mut out = Vec::with_capacity(rows.len().saturating_sub(1));
    // (leading spaces, text column) of the row that opened the current run.
    let mut opened: Option<(usize, usize)> = None;
    let mut fenced = false;
    // The indent of a `⎿` tool result whose rows we are inside.
    let mut tool: Option<usize> = None;
    for pair in rows.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        let a_trim = a.text.trim();
        if a_trim.starts_with("```") {
            fenced = !fenced;
        }
        if !a_trim.is_empty() {
            if tool.is_some_and(|t| lead(&a.text) <= t) {
                tool = None;
            }
            if a_trim.starts_with('⎿') {
                tool = Some(lead(&a.text));
            }
        }
        let (lo, hi) = *opened.get_or_insert_with(|| (lead(&a.text), text_col(&a.text)));
        let b_trim = b.text.trim();
        let word = cells(b_trim.split(' ').next().unwrap_or(""));
        // Ink breaks one cell early when the next word would land exactly on
        // the edge, so a word that just fits still counts as not fitting.
        // A word too long for any row tells nothing about where `a` ended.
        let full = a.end + 1 + word >= cols && lead(&b.text) + word <= cols;
        let join = full
            && !fenced
            && tool.is_none()
            && !b_trim.is_empty()
            && b_trim
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii() || c.is_alphabetic())
            && !a_trim.ends_with('…')
            && (lo..=hi).contains(&lead(&b.text))
            && text_col(&b.text) == lead(&b.text)
            && !structural(&a.text)
            && !structural(&b.text);
        let wide = |c: Option<char>| c.is_some_and(|c| c.width() == Some(2));
        // A one-word row that fills the row is a word Ink cut in two: its hard
        // breaks fill to the edge, its word wraps stop short of it.
        let one_word = a.end >= cols && !a.text[text_byte(&a.text)..].trim_end().contains(' ');
        out.push(if !join {
            opened = None;
            Sep::Newline
        } else if one_word || (wide(a_trim.chars().last()) && wide(b_trim.chars().next())) {
            Sep::Nothing
        } else {
            Sep::Space
        });
    }
    out
}

fn apply(parts: &[&str], seps: &[Sep]) -> String {
    let mut out = parts[0].to_string();
    for (part, &sep) in parts[1..].iter().zip(seps) {
        if sep == Sep::Newline {
            out.push('\n');
            out.push_str(part);
            continue;
        }
        out.truncate(out.trim_end().len());
        if sep == Sep::Space {
            out.push(' ');
        }
        out.push_str(part.trim_start());
    }
    out
}

/// Cells `text` takes in the grid, which sizes each char on its own: VS16
/// adds nothing, so `⚠️` is one cell, not the two `str` width gives it.
fn cells(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}

fn lead(text: &str) -> usize {
    text.len() - text.trim_start_matches(' ').len()
}

/// Where a row's text starts once a list or block marker is skipped: `- `,
/// `1. `, or a run of symbols and a space (`⏺ `, `⚠️ `, `◯ `).
fn text_col(text: &str) -> usize {
    let at = text_byte(text);
    lead(text) + cells(&text[lead(text)..at])
}

/// The byte where a row's text starts, past its indent and any marker.
fn text_byte(text: &str) -> usize {
    let indent = lead(text);
    let rest = &text[indent..];
    let Some((marker, _)) = rest.split_once(' ') else {
        return indent;
    };
    let digits = marker.trim_end_matches(['.', ')']);
    let is_marker = matches!(marker, "-" | "*" | "+" | "•")
        || (marker.len() > digits.len()
            && (1..=3).contains(&digits.len())
            && digits.bytes().all(|b| b.is_ascii_digit()))
        || (!marker.is_empty()
            && marker
                .chars()
                .all(|c| !c.is_ascii() && !c.is_alphanumeric()));
    if is_marker {
        indent + marker.len() + 1
    } else {
        indent
    }
}

/// Box drawing, block elements, a raw table row, or a prompt: never joined.
fn structural(text: &str) -> bool {
    let t = text.trim();
    text.chars().any(|c| ('\u{2500}'..='\u{259f}').contains(&c))
        || ["|", "$ ", "> ", "❯ ", "```", "⎿"]
            .iter()
            .any(|p| t.starts_with(p))
}

impl TerminalView {
    /// ⌘⌥C and Copy Raw: Copy, with the grid's rows as it holds them.
    pub(crate) fn copy_raw(&mut self, cx: &mut Context<Self>) -> bool {
        let editor_selection = self.input_active() && self.cmd.selected_text().is_some();
        if editor_selection || !self.has_selection() {
            return self.copy_contextual(false, cx);
        }
        self.copy_selection_as(true, cx);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::index::Side;
    use alacritty_terminal::selection::{Selection, SelectionType};

    /// Rows the way `tty7 capture --plain` prints them; a capture line longer
    /// than the pane was soft-wrapped, so its last row is the remainder.
    fn unwrap(text: &str, cols: usize) -> String {
        let rows: Vec<Row> = text
            .split('\n')
            .map(|p| {
                let w = cells(p.trim_end());
                Row {
                    text: p.to_string(),
                    end: if w > cols { (w - 1) % cols + 1 } else { w },
                }
            })
            .collect();
        rejoin(text, &rows, cols)
    }

    fn term(cols: usize, lines: usize, input: &str) -> Term<VoidListener> {
        let mut term = Term::new(
            alacritty_terminal::term::Config::default(),
            &crate::terminal::size::TermSize::new(cols, lines),
            VoidListener,
        );
        alacritty_terminal::vte::ansi::Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new()
            .advance(&mut term, input.as_bytes());
        term
    }

    fn select(
        term: &mut Term<VoidListener>,
        ty: SelectionType,
        from: (i32, usize),
        to: (i32, usize),
    ) {
        let mut sel = Selection::new(ty, Point::new(Line(from.0), Column(from.1)), Side::Left);
        sel.update(Point::new(Line(to.0), Column(to.1)), Side::Right);
        term.selection = Some(sel);
    }

    const C111: &str = include_str!("fixtures/copy_unwrap_111.txt");
    const C172: &str = include_str!("fixtures/copy_unwrap_172.txt");
    const C_CODE: &str = include_str!("fixtures/copy_unwrap_code.txt");

    fn has_line(out: &str, line: &str) -> bool {
        out.lines().any(|l| l == line)
    }

    #[test]
    fn wrapped_prose_joins() {
        let out = unwrap(C111, 111);
        assert!(has_line(
            &out,
            "⏺ #84 is now based on the new patch-stack main-niu, and GitHub reports it can merge cleanly. Its code is the same as what passed review: the only change is that its notes moved into the hotkey-window feature page. Its tests pass locally."
        ));
        assert!(has_line(
            &out,
            "※ recap: The goal is for tty7 to show the hotkey window on launch, instead of creating a new workspace, when it's your only workspace. That's PR #84, which passed review and is waiting on CI. Next, it merges and tty7 reloads on its own, and I'll check your sessions survive. (disable recaps in /config)"
        ));
        // A system line Claude continues at column 0.
        assert!(has_line(
            &out,
            "⏺ Background command \"Merge #84 once CI passes (network-error tolerant), then reload tty7\" completed (exit code 0)"
        ));
        let out = unwrap(C172, 172);
        assert!(has_line(
            &out,
            "※ recap: Goal: clean up shuck findings in your dotfiles, most-used first; the shell startup files are done and committed locally. Next, pick Z1, Z2 or Z3 for the no-op `unset GLOBAL_RCS` line, and say \"push\" for the 19 unpushed commits. (disable recaps in /config)"
        ));
        // `&&/||` in prose is not a table.
        assert!(out.contains(
            "fixing &&/|| logic into proper if/else, correcting a comment-continuation bug, adding"
        ));
    }

    #[test]
    fn the_reported_paragraph_joins() {
        let rows = "⏺ If the Theme field won't change or the popup comes back after they pick it, send me a screenshot. That would\n  be a bug.";
        assert_eq!(
            unwrap(rows, 111),
            "⏺ If the Theme field won't change or the popup comes back after they pick it, send me a screenshot. That would be a bug."
        );
    }

    #[test]
    fn list_items_keep_their_newlines() {
        let out = unwrap(C111, 111);
        assert!(out.contains("  Here's how to try it:\n  1. Close every workspace except the hotkey one.\n  2. Quit tty7 and open it again.\n"));
        let bullets = "  - Rule location: the rule is .claude/rules/app-layout.md, plus its Cursor copy and a pointer from the backend\n    skill.\n  - What it says: nothing new goes under <app>/services/, and other apps import only <app>.public and\n    <app>.models.";
        assert_eq!(
            unwrap(bullets, 111),
            "  - Rule location: the rule is .claude/rules/app-layout.md, plus its Cursor copy and a pointer from the backend skill.\n  - What it says: nothing new goes under <app>/services/, and other apps import only <app>.public and <app>.models."
        );
    }

    #[test]
    fn code_tables_and_tool_output_never_join() {
        let out = unwrap(C111, 111);
        // A box drawn round a summary.
        assert!(out.contains("  │  joins when ... the line runs to the right edge (the next word wouldn't\n  │                 have fit)"));
        // A tool call's command and its char-wrapped output stay as the grid has them.
        assert!(out.contains("tasks/befky\n      62qi.output;"));
        assert!(out.contains(
            "https://github.com/NorthIsUp/tty7/actions/runs/369394730\n     36/job/110627346251"
        ));
        assert!(out.contains("panicked at crates/tty7\n     -core/src/host/server.rs"));
        let fenced = "```\nlet joined = rows.iter().map(|r| r.text.as_str()).collect::<Vec<_>>().join(\" \"); // a long line of code\nthat continues\n```";
        let code_width = cells(fenced.lines().nth(1).unwrap());
        assert_eq!(unwrap(fenced, code_width), fenced);
        let table = "| a long table cell that runs right up to the edge of the pane, wider than the |\nrest |";
        assert_eq!(unwrap(table, 80), table);
    }

    #[test]
    fn a_short_row_before_an_indented_row_does_not_join() {
        let out = unwrap(C111, 111);
        assert!(out.contains("  Here's how to try it:\n  1."));
        let rows = "  Still open on your side: rotate the key.\n  Then rerun the push.";
        assert_eq!(unwrap(rows, 111), rows);
    }

    #[test]
    fn a_narrow_pane_joins_the_same() {
        // The reported paragraph and a list item, as Ink wraps them at 52 columns.
        let rows = "⏺ If the Theme field won't change or the popup comes\n  back after they pick it, send me a screenshot.\n  That would be a bug.\n\n  1. Close every workspace except the hotkey one,\n     then quit tty7 and open it again.\n  2. Quit tty7.";
        assert_eq!(
            unwrap(rows, 52),
            "⏺ If the Theme field won't change or the popup comes back after they pick it, send me a screenshot. That would be a bug.\n\n  1. Close every workspace except the hotkey one, then quit tty7 and open it again.\n  2. Quit tty7."
        );
    }

    #[test]
    fn the_grid_selection_joins_from_a_mid_row_start() {
        let soft = "a".repeat(45);
        let mut term = term(
            40,
            6,
            &format!(
                "⏺ If the Theme field won't change or\r\n  the popup comes back after they pick\r\n  it.\r\n{soft}"
            ),
        );
        select(&mut term, SelectionType::Simple, (0, 2), (4, 39));
        assert_eq!(
            joined(&term).unwrap(),
            format!(
                "If the Theme field won't change or the popup comes back after they pick it.\n{soft}"
            )
        );
    }

    #[test]
    fn wide_text_joins_without_a_space() {
        // Ten CJK chars fill 20 cells; Ink wraps between any two.
        let mut term = term(20, 3, "这是一个很长的中文句\r\n继续写完。");
        select(&mut term, SelectionType::Simple, (0, 0), (1, 19));
        assert_eq!(joined(&term).unwrap(), "这是一个很长的中文句继续写完。");
    }

    #[test]
    fn wide_on_one_side_only_keeps_the_space() {
        let emoji = format!("  {} 🎉\n  Next steps", "w".repeat(36));
        assert_eq!(
            unwrap(&emoji, 42),
            format!("  {} 🎉 Next steps", "w".repeat(36))
        );
        let mixed = format!("  {} 詳しくは\n  README を", "w".repeat(30));
        assert_eq!(
            unwrap(&mixed, 42),
            format!("  {} 詳しくは README を", "w".repeat(30))
        );
    }

    #[test]
    fn a_block_selection_copies_raw() {
        let mut term = term(
            40,
            3,
            "⏺ If the Theme field won't change or\r\n  the popup comes back",
        );
        select(&mut term, SelectionType::Block, (0, 0), (1, 39));
        assert_eq!(joined(&term), term.selection_to_string());
        assert!(joined(&term).unwrap().contains("or\n"));
    }

    #[test]
    fn a_lines_selection_keeps_its_trailing_newline() {
        let mut term = term(
            40,
            3,
            "⏺ If the Theme field won't change or\r\n  the popup comes back",
        );
        select(&mut term, SelectionType::Lines, (0, 0), (1, 0));
        assert_eq!(
            joined(&term).unwrap(),
            "⏺ If the Theme field won't change or the popup comes back\n"
        );
    }

    #[test]
    fn rows_that_dont_line_up_with_the_text_copy_raw() {
        let rows = [Row {
            text: "x".repeat(40),
            end: 40,
        }];
        let raw = format!("{}\nmore", "x".repeat(40));
        assert_eq!(rejoin(&raw, &rows, 40), raw);
    }

    #[test]
    fn a_one_word_row_at_the_edge_joins_without_a_space() {
        let url = "https://github.com/NorthIsUp/tty7/actions/runs/36939473036";
        let (head, tail) = url.split_at(40);
        let rows = format!("  {head}\n  {tail} is the run.");
        assert_eq!(unwrap(&rows, 42), format!("  {url} is the run."));
    }

    #[test]
    fn a_vs16_emoji_is_one_cell() {
        assert_eq!(cells("⚠️"), 1);
        // 2 + "⚠️ " (2 cells) + 37 = 41 cells: the next word would not fit at 44.
        let rows = format!("  ⚠️ {}\n  more words", "w".repeat(37));
        assert_eq!(
            unwrap(&rows, 44),
            format!("  ⚠️ {} more words", "w".repeat(37))
        );
        assert_eq!(unwrap(&rows, 48), rows);
    }

    #[test]
    fn a_wrapped_code_line_joins_and_separate_code_lines_dont() {
        let out = unwrap(C_CODE, 111);
        assert!(has_line(
            &out,
            "  ! themes env version history sampleorg/tty/testing | head -1; cd ~/src/sampleorg/Sample_1/.worktrees/sm-12345-tty-set-run/src/sampl && .venv/bin/python ~/tmp/set_sync_testing.py --apply"
        ));
        assert!(out.contains("  ! cd /Users/adam/src/tty7-mdfix && mise run reload\n"));
        assert!(out.contains(
            "  # themes/palette.py: drop `import random`\n  from colour.utils.random import get_random_string\n  ...\n"
        ));
    }

    #[test]
    fn the_status_bar_never_joins() {
        let rows = "  #14 #15 #16 #17 #18 #19 #20 #21 #24 #25 #27 #28 #29 #30 #31 #32 #33 #34 #35 #36 #37 #38 #39 #40 #41 #42 …\n  ⏵⏵ auto mode on (shift+tab to cycle) · ← 19 agents";
        assert_eq!(unwrap(rows, 111), rows);
    }
}
